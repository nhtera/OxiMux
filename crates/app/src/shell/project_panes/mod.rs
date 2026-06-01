//! `ProjectPanes` — workspace-level container for one project's pane
//! groups.
//!
//! The workspace owns one `ProjectPanes` per open project; each holds:
//!
//! - a `PaneGroupManager` (pure-data layout tree)
//! - a `HashMap<PaneGroupId, Entity<PaneGroup>>` (live entities)
//! - the per-project notifier + save-callback + window-activation
//!   observer (single source of truth shared down to each group's
//!   per-tab status watcher via `Arc<AtomicBool>`).
//!
//! Splits at the workspace level create new sibling pane groups via the
//! manager. File-open and split actions target the active group.

mod render;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{App, AppContext, Context, Entity, FocusHandle, Focusable, Subscription, Window};
use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};

use oximux_agents::{AgentStatusStream, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_storage::{PaneBufferRepo, PaneRelayIdRepo};

use crate::notifier::{Notifier, TabId};
use crate::persisted_terminals::{
    PersistedAgentTab, PersistedGroup, PersistedLeafTab, PersistedSubPane, PersistedTab,
    PersistedTabKind, PersistedTabs, PersistedTree, snapshot_tree,
};
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::tab_drag_zones::Zone;
use crate::shell::pane_group::{PaneGroup, PaneGroupTabKind};
use crate::shell::pane_group_manager::{CloseGroupError, GroupSplitOutcome, PaneGroupManager};
use crate::shell::pane_tree::{Axis, PaneGroupId, SplitInsert};

/// Hovered drop target during a cross-group tab drag — drives the
/// pane-body 5-zone overlay render. Set by `on_drag_move` on the body
/// container; cleared on drop or when the drag exits every body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabDragHoveredTarget {
    pub group_id: PaneGroupId,
    pub zone: Zone,
}
use crate::shell::terminal_view::TerminalView;

/// Persistence sink invoked after every tab/topology change. Captures
/// `SettingsRepo` + `project_id`; serializes the snapshot to JSON +
/// writes one row in `settings`.
pub type SaveCallback = Arc<dyn Fn(PersistedTabs) + Send + Sync>;

pub struct ProjectPanes {
    manager: PaneGroupManager,
    groups: HashMap<PaneGroupId, Entity<PaneGroup>>,
    /// One observer per group entity so layout / focus changes inside a
    /// group bubble up to the workspace render + trigger a save.
    _observers: HashMap<PaneGroupId, Subscription>,
    /// Focus-in subscription per group so the manager's `active_group_id`
    /// follows wherever the user actually puts focus. Without this,
    /// split / close-group actions target whichever group was last
    /// explicitly activated, not the one the user is in.
    _focus_observers: HashMap<PaneGroupId, Subscription>,
    focus_handle: FocusHandle,
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    /// Canonical window-activation flag — updated by the observer below
    /// and read by every per-tab status watcher across all groups via a
    /// shared `Arc`.
    window_active: Arc<AtomicBool>,
    _window_activation_observer: Subscription,
    /// Snapshot sink. `None` during tests / construction.
    save_callback: Option<SaveCallback>,
    /// Surrounding chrome width. Forwarded to every group on set so
    /// PTY grid math stays current.
    chrome_w_px: f32,
    /// Hovered drop target during a cross-group tab drag (Phase D).
    /// `None` whenever no drag is active OR the cursor sits outside every
    /// pane body. The body's `on_drag_move` sets it to (group, zone) and
    /// the matching render pass paints the overlay.
    hovered_drop_target: Option<TabDragHoveredTarget>,
    /// Workspace-global counter for default terminal labels. Bumped on
    /// every shell spawn (initial group, split-spawn, +-new-tab) so
    /// labels stay unique across panes (global, not per-group).
    next_terminal_n: u64,
}

impl ProjectPanes {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let window_active = Arc::new(AtomicBool::new(window.is_window_active()));
        let window_active_for_observer = window_active.clone();
        let observer = cx.observe_window_activation(window, move |_this, window, _cx| {
            window_active_for_observer.store(window.is_window_active(), Ordering::Relaxed);
        });
        let manager = PaneGroupManager::new();
        let initial_id = manager.active_group_id();
        let group = build_group(
            cwd.clone(),
            theme,
            density,
            typography.clone(),
            cli_runtime.clone(),
            notifier.clone(),
            window_active.clone(),
            cx,
        );
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, initial_id, window, cx);
        let mut groups = HashMap::new();
        groups.insert(initial_id, group);
        let mut observers = HashMap::new();
        observers.insert(initial_id, group_observer);
        let mut focus_observers = HashMap::new();
        focus_observers.insert(initial_id, group_focus_observer);
        Self {
            manager,
            groups,
            _observers: observers,
            _focus_observers: focus_observers,
            focus_handle: cx.focus_handle(),
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
            notifier,
            window_active,
            _window_activation_observer: observer,
            save_callback: None,
            chrome_w_px: density.w_left_rail,
            hovered_drop_target: None,
            next_terminal_n: 1,
        }
    }

    /// Pull-and-bump the workspace-global terminal counter. Called right
    /// before every shell spawn so labels stay monotonic across panes.
    /// Walks every existing tab label, parses any `Terminal N` suffix,
    /// and floors the counter at `max(N) + 1` so restored sessions don't
    /// re-issue colliding labels (the per-group counter resets on app
    /// boot but labels persist).
    fn take_next_terminal_n(&mut self, cx: &App) -> u64 {
        let mut highest = self.next_terminal_n.saturating_sub(1);
        for group in self.groups.values() {
            let g = group.read(cx);
            highest = highest.max(g.next_terminal_n_peek().saturating_sub(1));
            for (_, tab) in g.visible_tabs() {
                if let Some(rest) = tab.label.strip_prefix("Terminal ")
                    && let Ok(parsed) = rest.parse::<u64>()
                {
                    highest = highest.max(parsed);
                }
            }
        }
        let n = highest + 1;
        self.next_terminal_n = n + 1;
        n
    }

    pub fn manager(&self) -> &PaneGroupManager {
        &self.manager
    }

    pub fn active_group(&self) -> Option<Entity<PaneGroup>> {
        self.groups.get(&self.manager.active_group_id()).cloned()
    }

    pub fn group(&self, id: PaneGroupId) -> Option<Entity<PaneGroup>> {
        self.groups.get(&id).cloned()
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn set_save_callback(&mut self, cb: SaveCallback) {
        self.save_callback = Some(cb);
    }

    pub fn window_active(&self) -> bool {
        self.window_active.load(Ordering::Relaxed)
    }

    pub fn agent_count(&self, cx: &App) -> usize {
        self.groups.values().map(|g| g.read(cx).agent_count()).sum()
    }

    /// Pick a target agent session for "send to active agent" actions.
    /// Preference order: (1) the active group's active tab when it's an
    /// agent (most-direct routing for the common "terminal + agent side
    /// by side" layout); (2) any group's active tab that's an agent; (3)
    /// any agent tab anywhere. Returns `None` when no agent is open.
    pub fn target_agent_session(&self, cx: &App) -> Option<AgentSessionId> {
        if let Some(active) = self.active_group()
            && let Some(id) = active.read(cx).active_agent_session()
        {
            return Some(id);
        }
        for group in self.groups.values() {
            if let Some(id) = group.read(cx).active_agent_session() {
                return Some(id);
            }
        }
        self.groups
            .values()
            .find_map(|g| g.read(cx).first_agent_session())
    }

    pub fn tab_count(&self, cx: &App) -> usize {
        self.groups.values().map(|g| g.read(cx).tab_count()).sum()
    }

    /// Total TTY-backed tab count across every pane group. Drives the
    /// status bar's "N TTY" metric.
    pub fn tty_count(&self, cx: &App) -> usize {
        self.groups.values().map(|g| g.read(cx).tty_count()).sum()
    }

    pub fn is_empty(&self, cx: &App) -> bool {
        self.tab_count(cx) == 0
    }

    pub fn set_chrome_width(&mut self, chrome: f32, cx: &mut App) {
        if (self.chrome_w_px - chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = chrome;
        for group in self.groups.values() {
            group.update(cx, |g, cx| g.set_chrome_width(chrome, cx));
        }
    }

    pub fn set_active_group(
        &mut self,
        id: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.manager.set_active(id) {
            if let Some(group) = self.groups.get(&id) {
                group.update(cx, |g, cx| g.focus_active(window, cx));
            }
            cx.notify();
        }
    }

    /// Activate the tab whose agent session matches `tab_id`. Walks
    /// every group; the first hit wins. Returns true if any group had
    /// the matching tab.
    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut found_group: Option<PaneGroupId> = None;
        for (id, group) in &self.groups {
            let hit = group.update(cx, |g, cx| g.set_active_by_tab_id(tab_id, window, cx));
            if hit {
                found_group = Some(*id);
                break;
            }
        }
        if let Some(id) = found_group {
            self.set_active_group(id, window, cx);
            true
        } else {
            false
        }
    }

    /// Spawn the first Terminal tab in the only (initial) pane group.
    pub fn seed_default_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group() {
            let n = self.take_next_terminal_n(cx);
            group.update(cx, |g, cx| {
                g.set_next_terminal_n(n);
                g.open_terminal_tab(window, cx);
            });
        }
    }

    /// Current hovered drop target during a tab drag — `Some` only while
    /// the cursor is over a pane body. Render reads this to paint the
    /// 5-zone overlay on the matching group.
    pub fn hovered_drop_target(&self) -> Option<TabDragHoveredTarget> {
        self.hovered_drop_target
    }

    /// Update the hovered drop target. Triggers a re-render only when
    /// the value actually changes — `on_drag_move` fires on every pointer
    /// move at ~60 fps and unconditional notifies would thrash.
    pub fn set_hovered_drop_target(
        &mut self,
        target: Option<TabDragHoveredTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.hovered_drop_target == target {
            return;
        }
        self.hovered_drop_target = target;
        cx.notify();
    }

    /// Move a tab from `source` group into `target` group by stealing its
    /// `PaneGroupTab` (preserving the inner terminal/editor entity, PTY,
    /// scrollback). Returns `false` when source/target/idx is invalid or
    /// source==target (drop-on-self merge is a no-op).
    pub fn transfer_tab(
        &mut self,
        source: PaneGroupId,
        source_tab_idx: usize,
        target: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if source == target {
            return false;
        }
        let Some(source_entity) = self.groups.get(&source).cloned() else {
            return false;
        };
        let Some(target_entity) = self.groups.get(&target).cloned() else {
            return false;
        };
        let Some(tab) = source_entity.update(cx, |g, cx| g.take_tab(source_tab_idx, cx)) else {
            return false;
        };
        target_entity.update(cx, |g, cx| {
            g.push_existing_tab(tab, window, cx);
        });
        // Focus follows the moved tab; the manager + project-panes
        // notify chain will repaint and purge any group that's now empty.
        self.set_active_group(target, window, cx);
        cx.notify();
        true
    }

    /// Drag-to-split: insert a new sibling group next to `target` along
    /// the axis implied by `zone`, then transfer the dragged tab into it.
    /// `Zone::Center` is a no-op here (merge is handled by `transfer_tab`).
    pub fn split_and_move_tab(
        &mut self,
        source: PaneGroupId,
        source_tab_idx: usize,
        target: PaneGroupId,
        zone: Zone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (axis, insert) = match zone {
            Zone::Center => return false,
            Zone::Left => (Axis::Horizontal, SplitInsert::Before),
            Zone::Right => (Axis::Horizontal, SplitInsert::After),
            Zone::Up => (Axis::Vertical, SplitInsert::Before),
            Zone::Down => (Axis::Vertical, SplitInsert::After),
        };
        // 1. Allocate the new sibling group in the layout tree.
        let Some(GroupSplitOutcome { new_group, .. }) =
            self.manager.split_at_target(target, axis, insert)
        else {
            return false;
        };
        // 2. Create the matching PaneGroup entity (empty — we fill it via
        // the transferred tab next).
        let group = build_group(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            self.cli_runtime.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            cx,
        );
        group.update(cx, |g, cx| g.set_chrome_width(self.chrome_w_px, cx));
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, new_group, window, cx);
        self.groups.insert(new_group, group);
        self._observers.insert(new_group, group_observer);
        self._focus_observers
            .insert(new_group, group_focus_observer);
        // 3. Steal the tab from source into the new group.
        let moved = self.transfer_tab(source, source_tab_idx, new_group, window, cx);
        if !moved {
            // Roll back the empty new group — purge_empty_groups will
            // catch it on next render, but doing it inline keeps the
            // tree consistent for the next user action.
            let _ = self.close_group_by_id(new_group, window, cx);
        }
        moved
    }

    /// Apply new weights to the Split node at `path` (drag-resize handle
    /// between sibling groups). Triggers a re-render only when the call
    /// actually mutates the tree — the drag handler fires on every mouse
    /// move at ~60 fps and unconditional notifies would thrash.
    pub fn set_split_weights(
        &mut self,
        path: &[usize],
        new_weights: Vec<f32>,
        cx: &mut Context<Self>,
    ) -> bool {
        let prev = self.manager.group_tree().split_weights(path);
        let changed = self.manager.set_split_weights(path, new_weights);
        if !changed {
            return false;
        }
        let next = self.manager.group_tree().split_weights(path);
        if prev == next {
            return false;
        }
        cx.notify();
        true
    }

    pub fn split_active_group(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneGroupId> {
        let GroupSplitOutcome { new_group, .. } = self.manager.split_active_group(axis, insert)?;
        let group = build_group(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            self.cli_runtime.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            cx,
        );
        let n = self.take_next_terminal_n(cx);
        group.update(cx, |g, cx| {
            g.set_chrome_width(self.chrome_w_px, cx);
            g.set_next_terminal_n(n);
            g.open_terminal_tab(window, cx);
        });
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, new_group, window, cx);
        self.groups.insert(new_group, group);
        self._observers.insert(new_group, group_observer);
        self._focus_observers
            .insert(new_group, group_focus_observer);
        if let Some(group) = self.groups.get(&new_group) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
        Some(new_group)
    }

    pub fn close_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaneGroupId, CloseGroupError> {
        let closed = self.manager.close_active_group()?;
        self.groups.remove(&closed);
        self._observers.remove(&closed);
        self._focus_observers.remove(&closed);
        if let Some(group) = self.groups.get(&self.manager.active_group_id()) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
        Ok(closed)
    }

    /// Close a specific group by id (no-op when unknown or last).
    pub fn close_group_by_id(
        &mut self,
        id: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaneGroupId, CloseGroupError> {
        if !self.manager.set_active(id) {
            return Err(CloseGroupError::NotFound);
        }
        self.close_active_group(window, cx)
    }

    /// Pre-render sweep: drop any group whose tabs hit zero. Refuses
    /// to close the last group — that path falls through to render-as-
    /// empty so the user still has a stable container.
    pub(crate) fn purge_empty_groups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() <= 1 {
            return;
        }
        let empties: Vec<PaneGroupId> = self
            .groups
            .iter()
            .filter_map(|(id, g)| g.read(cx).is_empty().then_some(*id))
            .collect();
        for id in empties {
            if self.groups.len() <= 1 {
                break;
            }
            let _ = self.close_group_by_id(id, window, cx);
        }
    }

    pub fn open_or_activate_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Already open somewhere? Activate that group + that tab.
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .editor_tab_index(path.as_path())
                .map(|idx| (*id, idx))
        });
        if let Some((id, idx)) = existing {
            self.set_active_group(id, window, cx);
            let group = self.groups.get(&id)?.clone();
            return Some(group.update(cx, |g, cx| {
                g.set_active(idx, window, cx);
                idx
            }));
        }

        // New file: always land in the focused (active) group, mixing
        // freely with whatever it already contains (terminals/agents).
        // Falls back to the topmost group if no active group exists.
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| g.open_or_activate_editor_tab(path, window, cx)))
    }

    /// Focus the agent tab already running in `worktree_path`, if one
    /// exists in any group. Returns `true` when a matching tab was found
    /// and activated; `false` when no tab matches (caller decides whether
    /// to spawn a fresh session). Mirrors the cross-group activate path of
    /// `open_or_activate_editor_tab` but never opens a new tab itself.
    pub fn focus_workspace_tab(
        &mut self,
        worktree_path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .agent_tab_index_for_worktree(worktree_path)
                .map(|idx| (*id, idx))
        });
        let Some((id, idx)) = existing else {
            return false;
        };
        self.set_active_group(id, window, cx);
        if let Some(group) = self.groups.get(&id).cloned() {
            group.update(cx, |g, cx| g.set_active(idx, window, cx));
        }
        true
    }

    /// Collect the worktree paths kept "live" by open PTY tabs across all
    /// groups (as strings, to match the sidebar's `Workspace.worktree_path`).
    /// Drives the left rail's live/idle status dot: a workspace with an
    /// open terminal or agent reads as "live" (green).
    pub fn live_worktree_paths(&self, cx: &gpui::App) -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        for group in self.groups.values() {
            for path in group.read(cx).live_worktree_paths() {
                set.insert(path.display().to_string());
            }
        }
        set
    }

    /// Open or activate a diff tab in the active group for `(path, staged)`.
    /// Mirrors `open_or_activate_editor_tab` but routes through `PaneGroup::
    /// open_or_activate_diff_tab` which constructs a fresh `DiffView` bound
    /// to `repo`. Diff tabs don't deduplicate across groups (unlike editor
    /// tabs) — clicking an SCM row always opens in the focused group.
    pub fn open_or_activate_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| {
            g.open_or_activate_diff_tab(repo, path, staged, untracked, window, cx)
        }))
    }

    /// Open or activate a commit-detail tab in the active group.
    /// Mirrors `open_or_activate_diff_tab` but routes through
    /// `PaneGroup::open_or_activate_commit_tab` which dedups by SHA
    /// rather than path. The new tab loads via `DiffView::load_commit`
    /// so the same DiffView render path handles both single-file and
    /// commit-detail multi-file views.
    pub fn open_or_activate_commit_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        short_oid: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| {
            g.open_or_activate_commit_tab(repo, sha, short_oid, subject, window, cx)
        }))
    }

    pub fn open_terminal_tab_in_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        let n = self.take_next_terminal_n(cx);
        group.update(cx, |g, cx| {
            g.set_next_terminal_n(n);
            g.open_terminal_tab(window, cx);
        });
    }

    /// Open `path` as an editor tab inside the SPECIFIC group identified
    /// by `group_id` (does not consult / mutate the active-group pointer
    /// until the call succeeds). Drag-drop center-zone target uses this
    /// so a file dropped onto a non-focused pane opens there, not in the
    /// previously-focused pane.
    ///
    /// Activates the target group on success. No-op on unknown id.
    pub fn open_file_in_group(
        &mut self,
        group_id: PaneGroupId,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target = self.groups.get(&group_id)?.clone();
        let idx = target.update(cx, |g, cx| g.open_or_activate_editor_tab(path, window, cx));
        self.set_active_group(group_id, window, cx);
        Some(idx)
    }

    /// Drag-to-split for the file-drag flow: insert a new sibling group
    /// next to `target` along the axis implied by `zone`, then open
    /// `path` as the new group's first (and only) editor tab.
    /// `Zone::Center` is rejected — merge is handled by
    /// `open_file_in_group`.
    pub fn split_and_open_file(
        &mut self,
        target: PaneGroupId,
        zone: Zone,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (axis, insert) = match zone {
            Zone::Center => return false,
            Zone::Left => (Axis::Horizontal, SplitInsert::Before),
            Zone::Right => (Axis::Horizontal, SplitInsert::After),
            Zone::Up => (Axis::Vertical, SplitInsert::Before),
            Zone::Down => (Axis::Vertical, SplitInsert::After),
        };
        // 1. Allocate the new sibling group in the layout tree. Mirrors
        // `split_and_move_tab` minus the source-tab transfer.
        let Some(GroupSplitOutcome { new_group, .. }) =
            self.manager.split_at_target(target, axis, insert)
        else {
            return false;
        };
        // 2. Create the matching `PaneGroup` entity (empty — populated by
        // the editor-tab push below).
        let group = build_group(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            self.cli_runtime.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            cx,
        );
        group.update(cx, |g, cx| g.set_chrome_width(self.chrome_w_px, cx));
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, new_group, window, cx);
        self.groups.insert(new_group, group.clone());
        self._observers.insert(new_group, group_observer);
        self._focus_observers
            .insert(new_group, group_focus_observer);
        // 3. Push the editor tab into the new group + activate it.
        group.update(cx, |g, cx| {
            g.open_or_activate_editor_tab(path, window, cx);
        });
        self.set_active_group(new_group, window, cx);
        cx.notify();
        true
    }

    /// Focus the active tab in the active group. Called when activating
    /// this project's panes so keystrokes route correctly.
    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.active_group() else {
            self.focus_handle.focus(window, cx);
            return;
        };
        group.update(cx, |g, cx| g.focus_active(window, cx));
    }

    pub fn active_editor_path(&self, cx: &App) -> Option<PathBuf> {
        self.active_group()
            .and_then(|g| g.read(cx).active_editor_path(cx))
    }

    /// Walk every group + every tab in DFS group order; yield each
    /// terminal tab's PTY scrollback bytes. Editor tabs contribute an
    /// empty buffer (ordinal alignment with the saved blob's tab list).
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &App) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for id in self.manager.in_order_groups() {
            if let Some(group) = self.groups.get(&id) {
                out.extend(group.read(cx).collect_pane_buffers(max_bytes, cx));
            }
        }
        out
    }

    pub fn collect_pane_external_ids(&self, cx: &App) -> Vec<Option<String>> {
        let mut out = Vec::new();
        for id in self.manager.in_order_groups() {
            if let Some(group) = self.groups.get(&id) {
                out.extend(group.read(cx).collect_pane_external_ids(cx));
            }
        }
        out
    }

    /// Spawn a freshly-built agent tab in the active group. Mirrors the
    /// legacy `WorkspaceTabs::push_agent_tab` signature so the spawn
    /// chain in `WorkspaceRoot::spawn_agent_tab` keeps shape.
    #[allow(clippy::too_many_arguments)]
    pub fn push_agent_tab(
        &mut self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        label_override: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_agent_tab(
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
                backend,
                term_id,
                label_override,
                window,
                cx,
            );
        });
    }

    /// Workspace snapshot. v3 schema captures both the flat (legacy) tab
    /// projection AND the workspace-level group split tree + per-group
    /// state so multi-group layouts survive restart. Custom-agent tabs
    /// are skipped (their `(program, args)` config isn't captured).
    ///
    /// **Flat fields (back-compat):**
    /// - `tabs` — every group's tabs concatenated in DFS group order.
    /// - `active` — flat index of the focused tab.
    /// - `tab_order` — flat projection of all per-group orders, in DFS group order.
    ///
    /// **Multi-group fields (v3):**
    /// - `group_tree` — `PersistedTree` mirror of `manager.group_tree()`.
    /// - `groups` — per-leaf state (`tab_count` + local `active` + local `tab_order`).
    /// - `active_group` — DFS index of the focused group.
    ///
    /// The flat + multi-group views agree at all times: each
    /// `PersistedGroup.tab_count` is a contiguous slice of the flat `tabs`
    /// vec, in DFS leaf order. Legacy readers ignore the new fields and
    /// rebuild a single group from the flat slice; v3 readers walk
    /// `group_tree` instead.
    pub fn snapshot(&self, cx: &App) -> PersistedTabs {
        let mut tabs: Vec<PersistedTab> = Vec::new();
        let mut groups: Vec<PersistedGroup> = Vec::new();
        let mut active_offset: Option<usize> = None;
        let mut active_group_dfs_idx: Option<usize> = None;
        let active_group_id = self.manager.active_group_id();
        for (dfs_idx, group_id) in self.manager.in_order_groups().into_iter().enumerate() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            let group_active = group_ref.active();
            // Per-group emit bookkeeping. `orig_to_emitted` maps the
            // group's source tab index → its position WITHIN this group's
            // emitted slice. Custom-agent tabs drop out, shifting every
            // surviving tab's local position; this map keeps `tab_order`
            // projection honest across the skip.
            let mut emitted_in_group: usize = 0;
            let mut orig_to_emitted: HashMap<usize, usize> = HashMap::new();
            let mut local_active: usize = 0;
            let slice_start = tabs.len();
            for (idx, tab) in group_ref.tabs().iter().enumerate() {
                let (agent, kind) = match &tab.kind {
                    PaneGroupTabKind::Terminal => (None, PersistedTabKind::Terminal),
                    PaneGroupTabKind::Editor { path } => (
                        None,
                        PersistedTabKind::Editor {
                            path: path.display().to_string(),
                        },
                    ),
                    // Diff and commit-detail tabs are intentionally NOT
                    // persisted — they regenerate from current `git`
                    // state when the user re-clicks the source row.
                    // Skip the slot so the persisted tab list stays
                    // compact.
                    PaneGroupTabKind::Diff { .. } | PaneGroupTabKind::Commit { .. } => continue,
                    PaneGroupTabKind::Agent {
                        adapter,
                        adapter_id,
                        worktree_path,
                        model,
                        effort,
                        ..
                    } => {
                        if matches!(adapter, AgentAdapter::Custom) {
                            continue;
                        }
                        (
                            Some(PersistedAgentTab {
                                adapter: *adapter,
                                adapter_id: (*adapter_id).to_string(),
                                worktree_path: worktree_path.display().to_string(),
                                model: model.clone(),
                                effort: effort.clone(),
                            }),
                            PersistedTabKind::Terminal,
                        )
                    }
                };
                if group_id == active_group_id && idx == group_active {
                    active_offset = Some(tabs.len());
                    active_group_dfs_idx = Some(dfs_idx);
                    local_active = emitted_in_group;
                }
                // For terminal tabs (non-agent), capture the sub-pane
                // tree topology + per-leaf cwd. Agent tabs are always
                // single-sub-pane; skipping keeps their blobs minimal.
                let (sub_tree, sub_panes, active_sub_pane) =
                    if matches!(tab.kind, PaneGroupTabKind::Terminal) && agent.is_none() {
                        snapshot_sub_pane_tree(tab, cx)
                    } else {
                        (PersistedTree::Leaf, Vec::new(), 0)
                    };
                tabs.push(PersistedTab {
                    label: tab.label.to_string(),
                    tree: sub_tree,
                    agent,
                    kind,
                    sub_panes,
                    active_sub_pane,
                });
                orig_to_emitted.insert(idx, emitted_in_group);
                emitted_in_group += 1;
            }
            // Per-group visual order → LOCAL indices (within this group's
            // slice). Empty when every tab was custom-agent.
            let mut local_order: Vec<usize> = Vec::with_capacity(emitted_in_group);
            for &orig_local in group_ref.tab_order_iter() {
                if let Some(&emitted) = orig_to_emitted.get(&orig_local) {
                    local_order.push(emitted);
                }
            }
            // Always record a PersistedGroup — even when empty — so
            // `groups.len()` matches `group_tree.leaf_count()`. The
            // restorer skips zero-tab groups (they consume nothing off
            // the flat tabs iterator), and an all-custom-agent group
            // restores as a fresh empty group. `slice_start` is the
            // first index of THIS group's slice; keeps invariants
            // visible in debug builds.
            debug_assert_eq!(slice_start + emitted_in_group, tabs.len());
            groups.push(PersistedGroup {
                tab_count: emitted_in_group,
                active: local_active,
                tab_order: local_order,
            });
        }
        // Flat tab_order — concatenate per-group local orders into global
        // indices. Tracks the same DFS group order as `groups`.
        let mut tab_order: Vec<usize> = Vec::with_capacity(tabs.len());
        let mut flat_base = 0usize;
        for group_snap in &groups {
            for &local in &group_snap.tab_order {
                tab_order.push(flat_base + local);
            }
            flat_base += group_snap.tab_count;
        }
        // Group tree mirror. Only emit when we have >1 group with the
        // shape matching the live tree — single-group snapshots stay
        // maximally back-compat with v2 readers (which expect
        // `group_tree: null`).
        let live_leaf_count = self.manager.group_tree().leaf_count();
        let multi_group = groups.len() > 1 && groups.len() == live_leaf_count;
        let group_tree =
            multi_group.then(|| snapshot_tree::<PaneGroupId>(self.manager.group_tree()));
        let active_group = active_group_dfs_idx
            .unwrap_or(0)
            .min(groups.len().saturating_sub(1));
        PersistedTabs {
            tabs,
            active: active_offset.unwrap_or(0),
            next_label_n: 1,
            tab_order,
            group_tree,
            groups: if multi_group { groups } else { Vec::new() },
            active_group,
        }
    }

    /// Build a snapshot + hand it to the save callback. No-op when no
    /// callback is registered.
    ///
    /// Takes `&App` (not `&mut Context<Self>`) so the on-quit hook in
    /// `main.rs` — which only has `&App` in its closure — can call it.
    /// Snapshotting is a pure read of `self` + per-leaf grid reads; no
    /// mutation, so a shared-ref context is sufficient.
    pub fn save_now(&self, cx: &gpui::App) {
        if let Some(cb) = self.save_callback.clone() {
            let snap = self.snapshot(cx);
            cb(snap);
        }
    }

    /// Append a restored terminal tab to the active group. Used by the
    /// project-panes factory.
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| g.push_restored_terminal_tab(label, view, cx));
    }

    /// Multi-sub-pane restore: append a terminal tab whose
    /// `TerminalSplitTree` has been fully reconstructed by the factory
    /// (per-leaf views + observers + tree shape + active position).
    /// Targets the active group; the `_in` variant takes an explicit
    /// group id for multi-group restore.
    pub fn push_restored_terminal_tab_with_tree(
        &mut self,
        label: String,
        tree: crate::shell::pane_group::sub_pane::TerminalSplitTree,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_restored_terminal_tab_with_tree(label, tree, cx)
        });
    }

    /// Multi-sub-pane restore into a SPECIFIC group (multi-group restore
    /// path). No-op when `group_id` isn't registered.
    pub fn push_restored_terminal_tab_with_tree_in(
        &mut self,
        group_id: PaneGroupId,
        label: String,
        tree: crate::shell::pane_group::sub_pane::TerminalSplitTree,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id) else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_restored_terminal_tab_with_tree(label, tree, cx)
        });
    }

    /// Append a restored agent tab to the active group. Builds the view
    /// internally so the call shape mirrors the legacy strip.
    #[allow(clippy::too_many_arguments)]
    pub fn push_restored_agent_tab(
        &mut self,
        persisted: &PersistedAgentTab,
        adapter_id: &'static str,
        label: String,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_agent_tab(
                persisted.adapter,
                adapter_id,
                PathBuf::from(&persisted.worktree_path),
                persisted.model.clone(),
                persisted.effort.clone(),
                session_id,
                status_rx,
                backend,
                term_id,
                Some(label),
                window,
                cx,
            );
        });
    }

    /// Finalize a restore: re-apply the saved visual `tab_order`,
    /// activate the requested tab inside the single restored group,
    /// focus it, and trigger a render. `tab_order.is_empty()` means
    /// the snapshot was pre-v2 with no order field — caller passes the
    /// empty vec and the method skips the re-order pass (insertion
    /// order survives as the visual order, which matches pre-v2 behavior).
    pub fn apply_restored_state(
        &mut self,
        active: usize,
        _next_label_n: u64,
        tab_order: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            if !tab_order.is_empty() {
                g.set_tab_order(tab_order);
            }
            if active < g.tab_count() {
                g.set_active(active, window, cx);
            }
            g.focus_active(window, cx);
        });
        cx.notify();
    }

    /// Multi-group restore (v3 snapshot path). Replaces the placeholder
    /// initial group with N empty groups arranged per `persisted_tree`
    /// and returns their freshly-allocated ids in DFS leaf order. Caller
    /// then pushes each group's tabs via `push_restored_terminal_tab_in`
    /// / `open_editor_in_group_restore` / `push_restored_agent_tab_in`,
    /// and finally calls `apply_restored_state_multi` to apply per-group
    /// active + tab_order + activate the saved focused group.
    pub fn rebuild_groups_from_tree(
        &mut self,
        persisted_tree: &PersistedTree,
        active_group_dfs_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<PaneGroupId> {
        // Walk the persisted tree to allocate fresh PaneGroupIds in DFS
        // order. The closure pushes each newly-minted id into
        // `allocated`, so the returned tree's leaves and the vec stay
        // index-aligned.
        let mut allocated: Vec<PaneGroupId> = Vec::new();
        let mut next_raw: u64 = 1;
        let restored_tree =
            crate::persisted_terminals::restore_tree::<PaneGroupId, _>(persisted_tree, &mut || {
                let id = PaneGroupId(next_raw);
                next_raw += 1;
                allocated.push(id);
                id
            });
        // Drop placeholder group(s) + every observer wired to them.
        self.groups.clear();
        self._observers.clear();
        self._focus_observers.clear();
        // Resolve active group id; default to the first leaf when the
        // saved index is stale (defensive against truncated blobs).
        let active = allocated
            .get(active_group_dfs_idx)
            .copied()
            .or_else(|| allocated.first().copied())
            .unwrap_or(PaneGroupId(0));
        self.manager = PaneGroupManager::from_tree(restored_tree, active, next_raw);
        // Build empty PaneGroup entities for each allocated id.
        for &id in &allocated {
            let group = build_group(
                self.cwd.clone(),
                self.theme,
                self.density,
                self.typography.clone(),
                self.cli_runtime.clone(),
                self.notifier.clone(),
                self.window_active.clone(),
                cx,
            );
            group.update(cx, |g, cx| g.set_chrome_width(self.chrome_w_px, cx));
            let group_observer = observe_group(&group, cx);
            let group_focus_observer = observe_group_focus(&group, id, window, cx);
            self.groups.insert(id, group);
            self._observers.insert(id, group_observer);
            self._focus_observers.insert(id, group_focus_observer);
        }
        cx.notify();
        allocated
    }

    /// Push a restored terminal tab into a SPECIFIC group (multi-group
    /// restore path). No-op when `group_id` isn't registered.
    pub fn push_restored_terminal_tab_in(
        &mut self,
        group_id: PaneGroupId,
        label: String,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id) else {
            return;
        };
        group.update(cx, |g, cx| g.push_restored_terminal_tab(label, view, cx));
    }

    /// Open an editor tab inside a SPECIFIC group during multi-group
    /// restore. Bypasses the active-group resolution so a file dropped
    /// in a non-focused group is restored to the same group it was
    /// captured from. No-op when `group_id` isn't registered.
    pub fn open_editor_in_group_restore(
        &mut self,
        group_id: PaneGroupId,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id).cloned() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.open_or_activate_editor_tab(path, window, cx);
        });
    }

    /// Push a restored agent tab into a SPECIFIC group. Multi-group
    /// counterpart of `push_restored_agent_tab` — the active-group
    /// pointer is irrelevant here because the target is named
    /// explicitly. No-op when `group_id` isn't registered.
    #[allow(clippy::too_many_arguments)]
    pub fn push_restored_agent_tab_in(
        &mut self,
        group_id: PaneGroupId,
        persisted: &PersistedAgentTab,
        adapter_id: &'static str,
        label: String,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id).cloned() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_agent_tab(
                persisted.adapter,
                adapter_id,
                PathBuf::from(&persisted.worktree_path),
                persisted.model.clone(),
                persisted.effort.clone(),
                session_id,
                status_rx,
                backend,
                term_id,
                Some(label),
                window,
                cx,
            );
        });
    }

    /// Finalize multi-group restore. Applies per-group `tab_order` +
    /// local `active` to each registered group, then activates the
    /// saved focused group + focuses its active tab. Mirrors
    /// `apply_restored_state` for the multi-group path.
    pub fn apply_restored_state_multi(
        &mut self,
        per_group: Vec<(PaneGroupId, Vec<usize>, usize)>,
        active_group: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (group_id, tab_order, active) in per_group {
            let Some(group) = self.groups.get(&group_id).cloned() else {
                continue;
            };
            group.update(cx, |g, cx| {
                if !tab_order.is_empty() {
                    g.set_tab_order(tab_order);
                }
                if active < g.tab_count() {
                    g.set_active(active, window, cx);
                }
            });
        }
        self.set_active_group(active_group, window, cx);
        cx.notify();
    }

    /// Persist scrollback bytes for every terminal tab keyed by
    /// `(project_id, ordinal)`. Ordinal counts terminal tabs in DFS
    /// group + per-group order; agent tabs are skipped (`CliRuntime`
    /// reloads their own history on restore).
    pub fn capture_pane_buffers(
        &self,
        repo: &PaneBufferRepo,
        project_id: &str,
        window_id: &str,
        max_bytes_per_pane: usize,
        cx: &App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id, window_id) {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "pane_buffers: delete_for_project failed"
            );
            return;
        }
        let mut ordinal: u32 = 0;
        for group_id in self.manager.in_order_groups() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            for tab in group_ref.tabs() {
                if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                    continue;
                }
                let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                // F3.4: capture EVERY live sub-pane's scrollback (not
                // just the active one). `sub_pane_ordinal` is the DFS
                // leaf position inside this tab; matches the DFS-leaf
                // order the restorer uses to dispatch bytes back into
                // each sub-pane. Single-sub-pane tabs end up writing
                // one row at `(ordinal, 0)`, identical to pre-F3.4.
                for (sub_pane_ordinal, slot) in tree.tree().in_order_leaves().iter().enumerate() {
                    let Some(view) = tree.get(*slot) else {
                        continue;
                    };
                    let bytes = view.read(cx).serialize_buffer(max_bytes_per_pane);
                    if bytes.is_empty() {
                        continue;
                    }
                    if let Err(err) = repo.set(
                        project_id,
                        window_id,
                        ordinal,
                        sub_pane_ordinal as u32,
                        &bytes,
                    ) {
                        tracing::warn!(
                            ?err,
                            project_id,
                            window_id,
                            ordinal,
                            sub_pane_ordinal,
                            "pane_buffers: set failed"
                        );
                    }
                }
                ordinal += 1;
            }
        }
    }

    /// Walk every terminal tab in DFS group order and persist each
    /// leaf's relay-side PTY id, if any. Same ordinal-counting rules
    /// as `capture_pane_buffers`.
    pub fn capture_pane_relay_ids(
        &self,
        repo: &PaneRelayIdRepo,
        project_id: &str,
        window_id: &str,
        relay_session_id: &str,
        cx: &App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id, window_id) {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "pane_relay_ids: delete_for_project failed"
            );
            return;
        }
        let mut ordinal: u32 = 0;
        for group_id in self.manager.in_order_groups() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            for tab in group_ref.tabs() {
                if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                    continue;
                }
                let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                // One relay id per group tab (the active leaf's active
                // view), keyed by tab ordinal. Split leaves and background
                // per-pane tabs aren't individually keyed here — those tabs
                // restore via the dormant tree path (no relay reattach), and
                // any background PTYs left alive on quit are reaped by the
                // daemon's idle-gc. Per-tab relay reattach would need an
                // (ordinal, sub_pane, tab) key — a future enhancement.
                let Some(view) = tree.active_view() else {
                    continue;
                };
                if let Some(pty_id) = view.read(cx).external_id()
                    && let Err(err) =
                        repo.set(project_id, window_id, ordinal, &pty_id, relay_session_id)
                {
                    tracing::warn!(
                        ?err,
                        project_id,
                        window_id,
                        ordinal,
                        "pane_relay_ids: set failed"
                    );
                }
                ordinal += 1;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_group(
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window_active: Arc<AtomicBool>,
    cx: &mut Context<ProjectPanes>,
) -> Entity<PaneGroup> {
    cx.new(|cx| {
        PaneGroup::new(
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
            notifier,
            window_active,
            cx,
        )
    })
}

fn observe_group(group: &Entity<PaneGroup>, cx: &mut Context<ProjectPanes>) -> Subscription {
    cx.observe(group, |_this, _g, cx| cx.notify())
}

/// Mirror the GPUI focus subtree of `group` onto the manager so that
/// any action routed via `active_group_id` (split / close-group / open
/// editor) lands on whichever group actually has the user's focus —
/// whether they got there by click, keyboard, or typing into a
/// terminal. Cheap: only re-notifies when the active id actually moves.
fn observe_group_focus(
    group: &Entity<PaneGroup>,
    id: PaneGroupId,
    window: &mut Window,
    cx: &mut Context<ProjectPanes>,
) -> Subscription {
    let handle = group.read(cx).focus_handle_clone();
    cx.on_focus_in(&handle, window, move |this, _window, cx| {
        if this.manager.set_active(id) {
            cx.notify();
        }
    })
}

impl Focusable for ProjectPanes {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Capture a terminal tab's `TerminalSplitTree` for persistence: the
/// shape (axes + weights via `snapshot_tree`), the per-leaf cwd via
/// `os_pid()` → `cwd_of_pid`, and the active leaf's DFS position. The
/// DFS-position contract pairs with `from_persisted` on the restore side:
/// the restorer walks the persisted tree in DFS order, allocating fresh
/// slot ids 0..N — `sub_panes[i]` lands on the i-th DFS leaf, and
/// `active_sub_pane` is interpreted the same way.
fn snapshot_sub_pane_tree(
    tab: &crate::shell::pane_group::PaneGroupTab,
    cx: &App,
) -> (PersistedTree, Vec<PersistedSubPane>, usize) {
    let PaneContent::Terminal(tree) = &tab.content else {
        return (PersistedTree::Leaf, Vec::new(), 0);
    };
    let shape = snapshot_tree(tree.tree());
    let leaves = tree.tree().in_order_leaves();
    let mut sub_panes: Vec<PersistedSubPane> = Vec::with_capacity(leaves.len());
    for slot in &leaves {
        // Capture every per-pane tab in the leaf. Prefer the OSC 7 hint
        // (F4.7) before paying for `proc_pidinfo` on each — snapshotting a
        // workspace with many panes becomes O(N) syscalls otherwise. The
        // same read pulls the stable surface/tab ids so they round-trip.
        let tabs: Vec<PersistedLeafTab> = match tree.leaf(*slot) {
            Some(leaf) => leaf
                .tabs()
                .iter()
                .map(|lt| {
                    let view = lt.view().read(cx);
                    let cwd = view
                        .cwd_hint()
                        .or_else(|| {
                            view.os_pid()
                                .and_then(crate::shell::cwd_resolver::cwd_of_pid)
                        })
                        .map(|p| p.display().to_string());
                    PersistedLeafTab {
                        cwd,
                        surface_id: view.surface_id().to_string(),
                        tab_id: view.tab_id().to_string(),
                    }
                })
                .collect(),
            None => Vec::new(),
        };
        let active_tab = tree.leaf(*slot).map(|l| l.active()).unwrap_or(0);
        // Top-level fields mirror the ACTIVE tab so a legacy reader (or a
        // single-tab leaf) restores the visible terminal.
        let active = tabs.get(active_tab);
        let cwd = active.and_then(|t| t.cwd.clone());
        let surface_id = active.map(|t| t.surface_id.clone()).unwrap_or_default();
        let tab_id = active.map(|t| t.tab_id.clone()).unwrap_or_default();
        // Only emit the `tabs` list for genuine multi-tab leaves so
        // single-tab leaves keep the compact legacy blob shape.
        let (tabs, active_tab) = if tabs.len() > 1 {
            (tabs, active_tab)
        } else {
            (Vec::new(), 0)
        };
        sub_panes.push(PersistedSubPane {
            cwd,
            surface_id,
            tab_id,
            tabs,
            active_tab,
        });
    }
    let active_dfs_pos = leaves
        .iter()
        .position(|&slot| slot == tree.active())
        .unwrap_or(0);
    (shape, sub_panes, active_dfs_pos)
}
