//! `PaneGroup` — one tab-strip leaf in the workspace's group layout tree.
//!
//! Each `PaneGroup` is a single tab strip with one active tab + its
//! content. The workspace owns a tree of these via `PaneGroupManager`;
//! splitting creates a new sibling group beside (or above/below) the
//! focused one. Each group is independent: opening a file in one group
//! does NOT affect any other group's tab list.

pub mod render;
pub mod sub_pane;
pub mod tab_drag;
pub mod tab_drag_zones;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gpui::{
    AppContext, Context, FocusHandle, Focusable, Point, ScrollHandle, SharedString, Subscription,
    Task, Window, px,
};
use oximux_agents::{AgentRuntime, AgentStatusStream, CliRuntime, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    CloseTab, FocusNextSubPane, FocusPrevSubPane, NewAgent, NewTab, NextTab, PrevTab,
    RequestOpenAdapterPicker, SplitSubPaneDown, SplitSubPaneRight,
};
use crate::shell::pane_tree::{Axis, SplitInsert};
use crate::notifier::{Notifier, TabId};
use crate::shell::agent_status_task::spawn_status_task;
use crate::shell::agent_tab_label;
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::sub_pane::TerminalSplitTree;
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};

/// Discriminator for `PaneGroupTab` carrying any per-kind metadata.
pub enum PaneGroupTabKind {
    Terminal,
    Agent {
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
    },
    Editor { path: PathBuf },
}

pub struct PaneGroupTab {
    pub label: SharedString,
    pub content: PaneContent,
    pub kind: PaneGroupTabKind,
    /// User-assigned color tag, set via the right-click "Tab Color"
    /// palette. Renders as a 2px left-edge accent bar on the chip.
    /// `None` = no color (default chrome color).
    pub color: Option<TabColor>,
    /// User-assigned custom title from the "Change Title" menu row.
    /// When `Some`, the chip and persistence use this in place of the
    /// default label (e.g. "Terminal 5").
    pub custom_title: Option<SharedString>,
    pub _observer: Option<Subscription>,
    pub _status_task: Option<Task<()>>,
}

/// Closed enum of user-pickable tab colors. Renders to a concrete
/// theme-independent RGB at chip-paint time. Picked to match the
/// reference editor's 9-swatch palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabColor {
    Blue,
    Purple,
    Pink,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Gray,
}

impl TabColor {
    /// Resolve to a concrete hex (`u32` RGB). Theme-independent so the
    /// user's color choice stays recognizable across light/dark modes.
    pub fn rgb(self) -> u32 {
        match self {
            TabColor::Blue => 0x3B82F6,
            TabColor::Purple => 0xA855F7,
            TabColor::Pink => 0xEC4899,
            TabColor::Red => 0xEF4444,
            TabColor::Orange => 0xF97316,
            TabColor::Yellow => 0xEAB308,
            TabColor::Green => 0x22C55E,
            TabColor::Teal => 0x14B8A6,
            TabColor::Gray => 0x9CA3AF,
        }
    }
}

/// Hover state during an in-progress tab drag. Drives the 2px blue
/// insertion bar rendered on the targeted chip's edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabDragHover {
    /// Visible position the dragged tab would land relative to.
    pub target_visible_idx: usize,
    /// Whether the insertion bar paints on the left edge (Before) or
    /// the right edge (After) of the target chip.
    pub side: TabInsertSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabInsertSide {
    Before,
    After,
}

pub struct PaneGroup {
    tabs: Vec<PaneGroupTab>,
    /// Visible tab order. Each entry is an index into `tabs` (the
    /// insertion-order vector). `tabs.len() == tab_order.len()` is an
    /// invariant maintained by every mutator. Drag-reorder mutates only
    /// this vector; entity refs in `tabs` stay put.
    tab_order: Vec<usize>,
    /// `Some` while a tab inside this group is being dragged AND the
    /// cursor is over one of its chips. Reset to `None` on drop or when
    /// drag leaves the strip.
    drag_hover: Option<TabDragHover>,
    active: usize,
    focus_handle: FocusHandle,
    /// Monotonic counter for default terminal labels. `ProjectPanes`
    /// overrides this via `set_next_terminal_n` before each spawn so the
    /// numbering is global across panes (the reference UX-style), not per-group.
    next_terminal_n: u64,
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) typography: Typography,
    pub(crate) cwd: PathBuf,
    pub(crate) cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    /// Shared with the owning `ProjectPanes` window-activation observer
    /// so per-tab status watchers read the same flag.
    window_active: Arc<AtomicBool>,
    /// Chrome width in window pixels (rail + sidebar) — forwarded by the
    /// workspace so terminal grid dispatch can compute target area.
    chrome_w_px: f32,
    /// Scroll state for the tab-strip viewport. Render attaches via
    /// `.track_scroll(handle)`; the auto-pin logic below sets the offset
    /// to a large negative x after every tab append so the strip's paint
    /// phase clamps to the right edge (keeping new + active tabs visible).
    tab_strip_scroll: ScrollHandle,
}

impl PaneGroup {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window_active: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            tabs: Vec::new(),
            tab_order: Vec::new(),
            drag_hover: None,
            active: 0,
            focus_handle: cx.focus_handle(),
            next_terminal_n: 1,
            theme,
            density,
            typography,
            cwd,
            cli_runtime,
            notifier,
            window_active,
            chrome_w_px: density.w_left_rail,
            tab_strip_scroll: ScrollHandle::new(),
        }
    }

    pub fn tabs(&self) -> &[PaneGroupTab] {
        &self.tabs
    }

    /// Iterate tabs in their visible order (post-drag-reorder). Each
    /// item carries the insertion-order index alongside the tab so
    /// callers can keep using the canonical idx for click handlers and
    /// active-tracking.
    pub fn visible_tabs(&self) -> impl Iterator<Item = (usize, &PaneGroupTab)> + '_ {
        self.tab_order.iter().filter_map(move |&idx| {
            self.tabs.get(idx).map(|t| (idx, t))
        })
    }

    /// Move a tab from one visible position to another. Mutates only
    /// `tab_order`; entity refs in `tabs` and the `active` insertion
    /// index are preserved so the active highlight follows the moved
    /// tab automatically. No-op when indices are out of range or
    /// identical.
    pub fn move_tab(&mut self, from_visible_idx: usize, to_visible_idx: usize) {
        if from_visible_idx == to_visible_idx {
            return;
        }
        if from_visible_idx >= self.tab_order.len() || to_visible_idx >= self.tab_order.len() {
            return;
        }
        let moved = self.tab_order.remove(from_visible_idx);
        self.tab_order.insert(to_visible_idx, moved);
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
    }

    pub fn drag_hover(&self) -> Option<TabDragHover> {
        self.drag_hover
    }

    /// Update the drag hover indicator. Triggers a re-render only when
    /// the value actually changes — `on_drag_move` fires on every
    /// pointer move, so naive `cx.notify()` would thrash.
    pub fn set_drag_hover(
        &mut self,
        hover: Option<TabDragHover>,
        cx: &mut Context<Self>,
    ) {
        if self.drag_hover == hover {
            return;
        }
        self.drag_hover = hover;
        cx.notify();
    }

    /// Visible position of the tab with `insertion_idx`, if any. Used
    /// by drop handlers to translate the drag payload's insertion-idx
    /// into the visible-idx that `move_tab` expects.
    pub fn visible_position_of(&self, insertion_idx: usize) -> Option<usize> {
        self.tab_order.iter().position(|&i| i == insertion_idx)
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> Option<&PaneGroupTab> {
        self.tabs.get(self.active)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn agent_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.kind, PaneGroupTabKind::Agent { .. }))
            .count()
    }

    /// Count of TTY-backed tabs (terminals + agents) in this group.
    /// Excludes editor tabs.
    pub fn tty_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    PaneGroupTabKind::Terminal | PaneGroupTabKind::Agent { .. }
                )
            })
            .count()
    }

    /// Index of an existing editor tab for `path`, if any. Used by
    /// `ProjectPanes` to activate-rather-than-reopen across groups.
    pub fn editor_tab_index(&self, path: &std::path::Path) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == path))
    }

    pub(crate) fn chrome_w_px(&self) -> f32 {
        self.chrome_w_px
    }

    /// ScrollHandle the render layer should attach to the tab-strip
    /// viewport via `.track_scroll(...)`. Exposed so the strip builder
    /// (free function in `render.rs`) can wire it up.
    pub(crate) fn tab_strip_scroll_handle(&self) -> ScrollHandle {
        self.tab_strip_scroll.clone()
    }

    /// Snap the tab-strip viewport to its right edge. Called after every
    /// tab append so the newly-added (and now-active) tab is visible —
    /// matches the reference editor's `stickToEndRef` behavior. The raw
    /// offset value is intentionally far-negative; the strip's paint
    /// phase clamps it to the actual `max_offset` once the new tab is
    /// measured. Idempotent if the strip already fits without overflow.
    fn pin_tab_strip_to_end(&self) {
        self.tab_strip_scroll
            .set_offset(Point::new(px(-100_000.0), px(0.0)));
    }

    pub(crate) fn focus_handle_clone(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_chrome_width(&mut self, new_chrome: f32, cx: &mut Context<Self>) {
        if (self.chrome_w_px - new_chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = new_chrome;
        cx.notify();
    }

    /// Override the next-terminal-label seed. `ProjectPanes` uses this to
    /// route a workspace-global counter into each group right before a
    /// spawn, keeping terminal numbering monotonic across splits.
    pub fn set_next_terminal_n(&mut self, n: u64) {
        self.next_terminal_n = n;
    }

    /// Peek at the next-terminal seed (without mutating). Used by
    /// `ProjectPanes::take_next_terminal_n` to compute the workspace-wide
    /// floor across every group's local counter.
    pub fn next_terminal_n_peek(&self) -> u64 {
        self.next_terminal_n
    }

    /// Append a freshly-spawned shell terminal as a new tab. Returns the
    /// index of the new tab; `None` if PTY spawn failed.
    pub fn open_terminal_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let (backend, session_id) = spawn_local_pty(self.cwd.clone())?;
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(backend, session_id, theme, density, typography, window, cx)
        });
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        let n = self.next_terminal_n;
        self.next_terminal_n += 1;
        let tab = PaneGroupTab {
            label: SharedString::from(format!("Terminal {n}")),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            // Tab-level observer is unused for terminal tabs — sub-pane
            // observers inside TerminalSplitTree drive re-renders.
            _observer: None,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        Some(self.active)
    }

    pub fn open_or_activate_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self.tabs.iter().position(
            |t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == &path),
        ) {
            self.active = idx;
            self.focus_active(window, cx);
            cx.notify();
            return idx;
        }
        let path_for_view = path.clone();
        let view = cx.new(|cx| oximux_editor::EditorView::new(path_for_view, window, cx));
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let label = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled")
            .to_string();
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Editor(view),
            kind: PaneGroupTabKind::Editor { path },
            color: None,
            custom_title: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        self.active
    }

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
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(backend, term_id, theme, density, typography, window, cx)
        });
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let label = match label_override {
            Some(s) => SharedString::from(s),
            None => {
                let current_labels: Vec<SharedString> =
                    self.tabs.iter().map(|t| t.label.clone()).collect();
                agent_tab_label::next_label_for(adapter_id, &current_labels)
            }
        };
        let status_task = spawn_status_task(
            status_rx.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            TabId::from(session_id),
            label.clone(),
            cx,
        );
        // Agent tabs are terminal-backed: wrap the agent PTY view in a
        // single-leaf sub-pane tree so Cmd+D can later add side PTYs.
        let agent_observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        self.tabs.push(PaneGroupTab {
            label,
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, agent_observer)),
            kind: PaneGroupTabKind::Agent {
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
            },
            color: None,
            custom_title: None,
            _observer: None,
            _status_task: Some(status_task),
        });
        let _ = observer; // legacy single-view observer no longer used
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        cx.notify();
        self.active
    }

    /// Remove a tab from this group by its insertion-order idx and
    /// return its owning `PaneGroupTab`. Used by the drag-to-split path
    /// to transfer entity ownership across `PaneGroup`s without rebuilding
    /// the inner terminal/editor view (preserves PTY + scrollback).
    ///
    /// Fixes `tab_order` (drops the entry pointing at `idx`, shifts the
    /// higher indices down by one) and `active` (decrements past the
    /// removed slot) so the source group renders correctly afterwards.
    pub fn take_tab(&mut self, idx: usize, cx: &mut Context<Self>) -> Option<PaneGroupTab> {
        if idx >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(idx);
        if let Some(pos) = self.tab_order.iter().position(|&i| i == idx) {
            self.tab_order.remove(pos);
        }
        for entry in self.tab_order.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active == idx {
            // Active tab moved out — clamp to the last visible position.
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
        } else if idx < self.active {
            self.active -= 1;
        }
        cx.notify();
        Some(removed)
    }

    /// Append a `PaneGroupTab` that was moved in from another group.
    /// The original `_observer` is re-attached against this `cx` so the
    /// destination group re-renders on inner view updates. Returns the
    /// insertion-order index in the destination group.
    pub fn push_existing_tab(
        &mut self,
        mut tab: PaneGroupTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Drop the prior cx's observer (would point at the source group's
        // entity) and re-subscribe under this group so notify() fires
        // here on inner-view changes. For terminal tabs we re-attach to
        // the ACTIVE sub-pane only — splits inside the moved tab keep
        // working because their inner observers stay alive in the tree.
        tab._observer = match &tab.content {
            PaneContent::Terminal(tree) => tree
                .active_view()
                .map(|view| cx.observe(view, |_this, _v, cx| cx.notify())),
            PaneContent::Editor(view) => {
                Some(cx.observe(view, |_this, _v, cx| cx.notify()))
            }
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        self.active
    }

    /// Append a pre-built terminal tab (used by the restore path).
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        view: gpui::Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        self.tabs.push(PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            _observer: None,
            _status_task: None,
        });
        self.tab_order.push(self.tabs.len() - 1);
        self.pin_tab_strip_to_end();
        cx.notify();
    }

    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(idx);
        // Drop the removed index from `tab_order`, then decrement every
        // remaining entry that pointed past it so the vector stays
        // consistent with the now-shifted `tabs` vector.
        if let Some(pos) = self.tab_order.iter().position(|&i| i == idx) {
            self.tab_order.remove(pos);
        }
        for entry in self.tab_order.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        if let PaneGroupTabKind::Agent { session_id, .. } = removed.kind {
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_this, _cx| {
                if let Err(err) = runtime.cancel(session_id).await {
                    tracing::warn!(?err, "pane-group close_tab: agent cancel failed");
                }
            })
            .detach();
        }
        if self.tabs.is_empty() {
            self.active = 0;
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Close every tab in this group except `keep_idx`. Iterates in
    /// reverse so each `close_tab` call sees stable indices for the
    /// untouched portion. No-op when `keep_idx` is out of range.
    pub fn close_others(
        &mut self,
        keep_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keep_idx >= self.tabs.len() {
            return;
        }
        for idx in (0..self.tabs.len()).rev() {
            if idx != keep_idx {
                self.close_tab(idx, window, cx);
            }
        }
    }

    /// Close every tab whose index is greater than `from_idx`. Reverse
    /// iteration keeps each `close_tab` index valid against the still-
    /// unprocessed tail.
    pub fn close_to_right(
        &mut self,
        from_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self.tabs.len();
        if from_idx + 1 >= len {
            return;
        }
        for idx in (from_idx + 1..len).rev() {
            self.close_tab(idx, window, cx);
        }
    }

    /// Close every tab in this group. The empty group is purged by
    /// `ProjectPanes::purge_empty_groups` on the next render frame.
    pub fn close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for idx in (0..self.tabs.len()).rev() {
            self.close_tab(idx, window, cx);
        }
    }

    /// Assign a color tag (or clear with `None`) to the tab at `idx`.
    /// The chip renders a 2px left-edge bar in the chosen color.
    pub fn set_tab_color(&mut self, idx: usize, color: Option<TabColor>, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.color = color;
            cx.notify();
        }
    }

    /// Override the tab's visible title with `title` (or restore the
    /// default by passing `None`). The chip and persistence read
    /// `custom_title.unwrap_or(label)`.
    pub fn set_tab_title(
        &mut self,
        idx: usize,
        title: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.custom_title = title;
            cx.notify();
        }
    }

    /// Resolve the tab's visible title — custom override if set, else
    /// the default label. Used by the chip render and persistence.
    pub fn visible_title(&self, idx: usize) -> Option<SharedString> {
        let tab = self.tabs.get(idx)?;
        Some(tab.custom_title.clone().unwrap_or_else(|| tab.label.clone()))
    }

    pub fn set_active(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        if idx == self.active {
            return;
        }
        self.active = idx;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| match &t.kind {
            PaneGroupTabKind::Agent { session_id, .. } => TabId::from(*session_id) == tab_id,
            PaneGroupTabKind::Terminal | PaneGroupTabKind::Editor { .. } => false,
        }) else {
            return false;
        };
        self.set_active(idx, window, cx);
        true
    }

    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = (self.active + 1) % self.tabs.len();
        self.set_active(next, window, cx);
    }

    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let prev = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.set_active(prev, window, cx);
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.focus_handle.focus(window, cx);
            return;
        };
        let handle = tab.content.focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn active_editor_path(&self, cx: &gpui::App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.active)?;
        match &tab.content {
            PaneContent::Editor(view) => Some(view.read(cx).file_path().to_path_buf()),
            PaneContent::Terminal(_) => None,
        }
    }

    /// Walk every tab (active + inactive) and yield the captured PTY
    /// scrollback bytes for the terminal kinds. Editor tabs contribute
    /// an empty buffer so the ordinal counter stays aligned with the
    /// tab vector.
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &gpui::App) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(tree) => {
                    // Persist the ACTIVE sub-pane's scrollback only.
                    // Multi-sub-pane persistence is a follow-up; v1
                    // restore restores a single sub-pane per tab.
                    let bytes = tree
                        .active_view()
                        .map(|v| v.read(cx).serialize_buffer(max_bytes))
                        .unwrap_or_default();
                    out.push(bytes);
                }
                PaneContent::Editor(_) => out.push(Vec::new()),
            }
        }
        out
    }

    pub fn collect_pane_external_ids(&self, cx: &gpui::App) -> Vec<Option<String>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(tree) => {
                    out.push(tree.active_view().and_then(|v| v.read(cx).external_id()));
                }
                PaneContent::Editor(_) => out.push(None),
            }
        }
        out
    }

    pub(crate) fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_terminal_tab(window, cx);
    }

    pub(crate) fn on_close_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Cmd+W disambiguation: if the active tab's sub-pane tree has
        // MORE THAN ONE live sub-pane, close just the FOCUSED sub-pane
        // (re-resolved here for the same reason as split — `tree.active`
        // doesn't auto-track mouse-driven focus changes). Otherwise
        // fall through to closing the whole tab.
        let active_idx = self.active;
        if let Some(active_tab) = self.tabs.get_mut(active_idx) {
            if let PaneContent::Terminal(tree) = &mut active_tab.content {
                if tree.live_count() > 1 {
                    let focused_idx = tree
                        .iter_live()
                        .find(|(_, v)| v.read(cx).focused())
                        .map(|(i, _)| i);
                    if let Some(idx) = focused_idx {
                        tree.set_active(idx);
                    }
                    if tree.close_active() {
                        if let Some(view) = tree.active_view() {
                            view.read(cx).focus_handle(cx).focus(window, cx);
                        }
                        cx.notify();
                        return;
                    }
                }
            }
        }
        self.close_tab(active_idx, window, cx);
    }

    pub(crate) fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.next_tab(window, cx);
    }

    pub(crate) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.prev_tab(window, cx);
    }

    pub(crate) fn on_new_agent(
        &mut self,
        _: &NewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(Box::new(RequestOpenAdapterPicker), cx);
    }

    /// Split the active terminal tab's CURRENTLY-FOCUSED sub-pane along
    /// `axis`. Before splitting we re-resolve which sub-pane actually
    /// has keyboard focus (by walking the tree and asking each
    /// `TerminalView`) — necessary because the stored `tree.active`
    /// only updates on previous splits/cycles, not when the user clicks
    /// into a different sub-pane to give it focus. Without this
    /// re-resolution Cmd+D would split whichever pane was last cycled
    /// to, not the one the user is actually typing in.
    ///
    /// Spawns a fresh PTY rooted at the group's `cwd` (full CWD
    /// inheritance from the source pane is a follow-up — for now we
    /// use the tab group's working directory). No-op when the active
    /// tab is not a terminal or PTY spawn fails.
    fn split_active_sub_pane(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.cwd.clone();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        // Sub-panes only apply to terminal-backed tabs; editor tabs are
        // single-view.
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Re-resolve focused sub-pane: scan live entries and pick the one
        // whose view reports `focused()`. Fallback to stored active
        // (covers the keyboard-only case where focus query may race
        // against terminal mount; the cycle/split paths keep active
        // correct in that flow). Two-step borrow: collect the index
        // through an immutable borrow, then mutate.
        let focused_idx = tree
            .iter_live()
            .find(|(_, v)| v.read(cx).focused())
            .map(|(i, _)| i);
        if let Some(idx) = focused_idx {
            tree.set_active(idx);
        }
        let Some((backend, session_id)) = spawn_local_pty(cwd) else {
            return;
        };
        let view = cx.new(|cx| {
            TerminalView::mount(backend, session_id, theme, density, typography, window, cx)
        });
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        tree.split_active(axis, insert, view, observer);
        // Focus the just-spawned sub-pane so keyboard input lands there.
        if let Some(active_view) = tree.active_view() {
            active_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_split_sub_pane_right(
        &mut self,
        _: &SplitSubPaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_active_sub_pane(Axis::Horizontal, SplitInsert::After, window, cx);
    }

    pub(crate) fn on_split_sub_pane_down(
        &mut self,
        _: &SplitSubPaneDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_active_sub_pane(Axis::Vertical, SplitInsert::After, window, cx);
    }

    pub(crate) fn on_focus_next_sub_pane(
        &mut self,
        _: &FocusNextSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_sub_pane_focus(true, window, cx);
    }

    pub(crate) fn on_focus_prev_sub_pane(
        &mut self,
        _: &FocusPrevSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_sub_pane_focus(false, window, cx);
    }

    fn cycle_sub_pane_focus(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        tree.cycle_focus(forward);
        if let Some(view) = tree.active_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }
}

impl Focusable for PaneGroup {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
