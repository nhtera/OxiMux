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

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, Subscription, Window,
};
use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};

use oximux_agents::{AgentStatusStream, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_storage::{PaneBufferRepo, PaneRelayIdRepo};

use crate::notifier::{Notifier, TabId};
use crate::persisted_terminals::{PersistedAgentTab, PersistedTab, PersistedTabs, PersistedTree};
use crate::shell::pane_group::tab_drag_zones::Zone;
use crate::shell::pane_group::{PaneGroup, PaneGroupTabKind};
use crate::shell::pane_group_manager::{
    CloseGroupError, GroupSplitOutcome, PaneGroupManager,
};
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
    /// Window-x of the most-recent `+` click. The button moves with the
    /// tab strip, so a static inset can't anchor the popover.
    last_plus_click_x: Cell<Option<f32>>,
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
    /// labels stay unique across panes — matches the reference UX's numbering.
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
        let observer =
            cx.observe_window_activation(window, move |_this, window, _cx| {
                window_active_for_observer
                    .store(window.is_window_active(), Ordering::Relaxed);
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
            last_plus_click_x: Cell::new(None),
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
                if let Some(rest) = tab.label.strip_prefix("Terminal ") {
                    if let Ok(parsed) = rest.parse::<u64>() {
                        highest = highest.max(parsed);
                    }
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
        self.groups
            .values()
            .map(|g| g.read(cx).agent_count())
            .sum()
    }

    pub fn tab_count(&self, cx: &App) -> usize {
        self.groups
            .values()
            .map(|g| g.read(cx).tab_count())
            .sum()
    }

    /// Total TTY-backed tab count across every pane group. Drives the
    /// status bar's "N TTY" metric.
    pub fn tty_count(&self, cx: &App) -> usize {
        self.groups
            .values()
            .map(|g| g.read(cx).tty_count())
            .sum()
    }

    pub fn is_empty(&self, cx: &App) -> bool {
        self.tab_count(cx) == 0
    }

    pub fn take_plus_click_x(&self) -> Option<f32> {
        self.last_plus_click_x.take()
    }

    pub fn set_plus_click_x(&self, x: f32) {
        self.last_plus_click_x.set(Some(x));
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
        self._focus_observers.insert(new_group, group_focus_observer);
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

    pub fn split_active_group(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneGroupId> {
        let GroupSplitOutcome { new_group, .. } =
            self.manager.split_active_group(axis, insert)?;
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
        self._focus_observers.insert(new_group, group_focus_observer);
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
    pub(crate) fn purge_empty_groups(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let existing: Option<(PaneGroupId, usize)> =
            self.groups.iter().find_map(|(id, group)| {
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

    /// v1 snapshot — flattens every group's tabs into one linear list
    /// keyed by terminal/agent kind. Editor + Custom-agent tabs are
    /// skipped (consistent with the legacy schema). Multi-group layout
    /// is NOT yet persisted; restore reconstructs a single group with
    /// every tab. v2 schema lives in a follow-up slice.
    pub fn snapshot(&self, cx: &App) -> PersistedTabs {
        let mut tabs: Vec<PersistedTab> = Vec::new();
        let mut active_offset: Option<usize> = None;
        let mut flat_idx: usize = 0;
        let active_group_id = self.manager.active_group_id();
        for group_id in self.manager.in_order_groups() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            let group_active = group_ref.active();
            for (idx, tab) in group_ref.tabs().iter().enumerate() {
                let agent = match &tab.kind {
                    PaneGroupTabKind::Terminal => None,
                    PaneGroupTabKind::Editor { .. } => continue,
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
                        Some(PersistedAgentTab {
                            adapter: *adapter,
                            adapter_id: (*adapter_id).to_string(),
                            worktree_path: worktree_path.display().to_string(),
                            model: model.clone(),
                            effort: effort.clone(),
                        })
                    }
                };
                if group_id == active_group_id && idx == group_active {
                    active_offset = Some(flat_idx);
                }
                tabs.push(PersistedTab {
                    label: tab.label.to_string(),
                    tree: PersistedTree::Leaf,
                    agent,
                });
                flat_idx += 1;
            }
        }
        PersistedTabs {
            tabs,
            active: active_offset.unwrap_or(0),
            next_label_n: 1,
        }
    }

    /// Build a snapshot + hand it to the save callback. No-op when no
    /// callback is registered.
    pub fn save_now(&self, cx: &mut Context<Self>) {
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

    /// Finalize a restore: activate the requested tab inside the
    /// single restored group, focus it, and trigger a render.
    pub fn apply_restored_state(
        &mut self,
        active: usize,
        _next_label_n: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            if active < g.tab_count() {
                g.set_active(active, window, cx);
            }
            g.focus_active(window, cx);
        });
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
        max_bytes_per_pane: usize,
        cx: &App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id) {
            tracing::warn!(?err, project_id, "pane_buffers: delete_for_project failed");
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
                let crate::shell::pane_content::PaneContent::Terminal(view) = &tab.content
                else {
                    continue;
                };
                let bytes = view.read(cx).serialize_buffer(max_bytes_per_pane);
                if !bytes.is_empty()
                    && let Err(err) = repo.set(project_id, ordinal, &bytes)
                {
                    tracing::warn!(?err, project_id, ordinal, "pane_buffers: set failed");
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
        relay_session_id: &str,
        cx: &App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id) {
            tracing::warn!(?err, project_id, "pane_relay_ids: delete_for_project failed");
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
                let crate::shell::pane_content::PaneContent::Terminal(view) = &tab.content
                else {
                    continue;
                };
                if let Some(pty_id) = view.read(cx).external_id()
                    && let Err(err) = repo.set(project_id, ordinal, &pty_id, relay_session_id)
                {
                    tracing::warn!(?err, project_id, ordinal, "pane_relay_ids: set failed");
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
