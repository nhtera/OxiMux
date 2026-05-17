//! MainPane — workspace grid of terminal panes (Phase 1 step 5 + step 7).
//!
//! Owns a `panes: HashMap<PaneId, Entity<TabbedPane>>` entity store paired
//! with a pure-data [`crate::shell::pane_tree::PaneTree`] describing the split
//! layout. Render walks the tree top-down, looks up each leaf's TabbedPane in
//! the HashMap, divides the available rect along each Split axis, and stages
//! per-leaf `(cols, rows)` via `TabbedPane::set_target_grid` (which fans the
//! grid to every tab inside).
//!
//! Action handlers (`SplitHorizontal`, `SplitVertical`, `ClosePane`,
//! `FocusNextPane`, `NewTab`, `CloseTab`, `NextTab`, `PrevTab`) are wired on
//! the root render `div`. GPUI dispatches actions up the element tree from
//! the focused leaf, so ordinary keystrokes still reach the focused
//! `TerminalView` while `Cmd-*` combos bubble up here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render, Styled, Subscription, Window,
    div,
};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    CloseTab, FocusNextPane, FocusPrevPane, NewTab, NextTab, PrevTab, SplitHorizontal,
    SplitVertical,
};
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::pane_layout::{ActiveDrag, DIVIDER_HIT_PX, build_node};
use crate::shell::pane_tree::{Axis, PaneId, PaneTree};
use crate::shell::tabbed_pane::{TAB_STRIP_HEIGHT_PX, TabbedPane};
use crate::shell::terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView};

/// Vertical chrome subtracted from viewport when computing terminal area
/// (top bar + status bar). Density tokens are 40 + 24 = 64.
const CHROME_H_PX: f32 = 40.0 + 24.0;

const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;

// Divider geometry (`DIVIDER_HIT_PX`, `DIVIDER_LINE_PX`) and the visual
// builders (`build_node`, `build_divider`) live in `super::pane_layout`.

pub struct MainPane {
    tree: PaneTree,
    panes: HashMap<PaneId, Entity<TabbedPane>>,
    focused: PaneId,
    next_id: AtomicU64,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    /// Horizontal chrome reserved by surrounding panels (left rail + right
    /// sidebar). Live value; updated by `WorkspaceRoot` on every render so
    /// the PTY grid shrinks when the right sidebar opens. Defaults to the
    /// left rail width so initial mount before the first sync still produces
    /// a sane terminal grid.
    chrome_w_px: f32,
    /// One observer per TabbedPane. When a pane notifies (click, keystroke,
    /// PTY output via its tab's bubbled notify, tab swap) we re-notify
    /// MainPane so render runs and `sync_focused_from_window` repaints the
    /// active-pane ring + dim overlays. Stored to keep subscriptions alive.
    _pane_observers: HashMap<PaneId, Subscription>,
    /// Set on divider `on_mouse_down`, read by the element-level
    /// `on_mouse_move` listener on the MainPane root to translate cursor
    /// position into weight updates. Cleared on `on_mouse_up` or when a
    /// move event arrives with the left button no longer held (recovers
    /// from the "released outside MainPane" case). `None` means no drag
    /// in progress.
    active_drag: Option<ActiveDrag>,
}

impl MainPane {
    /// Build a `MainPane` around a pre-spawned initial TabbedPane. PTY spawn
    /// + TabbedPane wrap happen at the caller so this builder is infallible.
    pub fn new(
        initial_pane: Entity<TabbedPane>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = PaneId(0);
        let next_id = AtomicU64::new(1);
        let sub = cx.observe(&initial_pane, |_, _, cx| cx.notify());
        let mut panes = HashMap::new();
        panes.insert(id, initial_pane);
        let mut pane_observers = HashMap::new();
        pane_observers.insert(id, sub);
        let focus_handle = cx.focus_handle();
        let chrome_w_px = density.w_left_rail;
        Self {
            tree: PaneTree::Leaf(id),
            panes,
            focused: id,
            next_id,
            theme,
            density,
            typography,
            focus_handle,
            chrome_w_px,
            _pane_observers: pane_observers,
            active_drag: None,
        }
    }

    /// Update the surrounding chrome width so PTY grids match the actual
    /// MainPane rect. Called by `WorkspaceRoot::render` whenever the left
    /// rail or right sidebar toggles. No-op if `new_chrome` matches; a
    /// `cx.notify()` would otherwise loop forever (render → update → render).
    pub fn set_chrome_width(&mut self, new_chrome: f32, cx: &mut Context<Self>) {
        if (self.chrome_w_px - new_chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = new_chrome;
        cx.notify();
    }

    pub fn leaf_count(&self) -> usize {
        self.tree.leaf_count()
    }

    fn alloc_id(&self) -> PaneId {
        PaneId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Sync `self.focused` from the window's currently focused element.
    /// Click-to-focus moves platform focus into a `TerminalView` (handled
    /// inside that view), but `MainPane.focused` only changes when we
    /// ourselves issue a focus call. Without this sync the user could click
    /// pane A, press Cmd-D, and end up splitting pane B (whichever was last
    /// action-focused). Called at the top of every action handler and at
    /// the top of render so dim/ring repaint immediately on click.
    fn sync_focused_from_window(&mut self, window: &Window, cx: &App) {
        let Some(active) = window.focused(cx) else {
            return;
        };
        for (id, pane) in &self.panes {
            if pane.read(cx).contains_focus(&active, cx) {
                self.focused = *id;
                return;
            }
        }
    }

    fn split_focused(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = spawn_terminal_view(
            self.theme,
            self.density,
            self.typography.clone(),
            window,
            cx,
        ) else {
            return;
        };
        let new_pane = cx.new(|cx| TabbedPane::new(view, cx));
        let new_id = self.alloc_id();
        if !self.tree.split_leaf(self.focused, axis, new_id) {
            tracing::warn!("split target not in tree; dropping new pane");
            return;
        }
        let sub = cx.observe(&new_pane, |_, _, cx| cx.notify());
        self._pane_observers.insert(new_id, sub);
        self.panes.insert(new_id, new_pane);
        self.focused = new_id;
        focus_pane(&self.panes, new_id, window, cx);
        cx.notify();
    }

    fn on_split_horizontal(
        &mut self,
        _: &SplitHorizontal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Horizontal, window, cx);
    }

    fn on_split_vertical(
        &mut self,
        _: &SplitVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Vertical, window, cx);
    }

    /// Remove the currently focused pane and re-home focus on its in-order
    /// neighbor. Reached via `CloseTab`'s last-tab cascade.
    fn close_focused_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.leaf_count() <= 1 {
            return;
        }
        let leaves_before = self.tree.in_order_leaves();
        let Some(closing_pos) = leaves_before.iter().position(|id| *id == self.focused) else {
            return;
        };
        let closing_id = self.focused;
        if !self.tree.remove_leaf(closing_id) {
            return;
        }
        self.panes.remove(&closing_id);
        self._pane_observers.remove(&closing_id);
        let leaves_after = self.tree.in_order_leaves();
        if leaves_after.is_empty() {
            return;
        }
        let neighbor = leaves_after[closing_pos.min(leaves_after.len() - 1)];
        self.focused = neighbor;
        focus_pane(&self.panes, neighbor, window, cx);
        cx.notify();
    }

    fn on_focus_next_pane(
        &mut self,
        _: &FocusNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_focus(1, window, cx);
    }

    fn on_focus_prev_pane(
        &mut self,
        _: &FocusPrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_focus(-1, window, cx);
    }

    /// Shared cycling math for Cmd+] / Cmd+[. `step` is +1 for next, -1 for
    /// prev; wraps modulo `leaves.len()`.
    fn cycle_focus(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        let leaves = self.tree.in_order_leaves();
        if leaves.len() < 2 {
            return;
        }
        let Some(pos) = leaves.iter().position(|id| *id == self.focused) else {
            return;
        };
        let len = leaves.len() as isize;
        let target = ((pos as isize + step).rem_euclid(len)) as usize;
        let next = leaves[target];
        self.focused = next;
        focus_pane(&self.panes, next, window, cx);
        cx.notify();
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        let Some(view) = spawn_terminal_view(
            self.theme,
            self.density,
            self.typography.clone(),
            window,
            cx,
        ) else {
            return;
        };
        if let Some(pane) = self.panes.get(&self.focused).cloned() {
            pane.update(cx, |tp, cx| tp.open_tab(view, window, cx));
        }
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        let Some(pane) = self.panes.get(&self.focused).cloned() else {
            return;
        };
        let pane_should_close = pane.update(cx, |tp, cx| tp.close_active(window, cx));
        if pane_should_close {
            self.close_focused_pane(window, cx);
        }
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        if let Some(pane) = self.panes.get(&self.focused).cloned() {
            pane.update(cx, |tp, cx| tp.next_tab(window, cx));
        }
    }

    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        if let Some(pane) = self.panes.get(&self.focused).cloned() {
            pane.update(cx, |tp, cx| tp.prev_tab(window, cx));
        }
    }

    fn dispatch_grids(&self, window: &Window, cx: &mut Context<Self>) {
        let metrics = CellMetrics::measure(&self.typography, window);
        let (w, h) = available_area(window, self.chrome_w_px, self.density.pad_panel, &metrics);
        // Defer PTY resize during an active divider drag (the reference terminal / iTerm
        // pattern). At 60 fps a drag would otherwise fire 60+ SIGWINCH/sec
        // per affected pane, which makes shells with expensive prompts
        // (oh-my-zsh, p10k) re-paint visibly every frame. The visible
        // layout still updates (build_node uses weights directly); only
        // the PTY backend resize is suppressed. On mouse-up the drag
        // clears, the next render reaches this with `dragging=false`, and
        // each affected pane gets a single resize reflecting the final
        // weights.
        let dragging = self.active_drag.is_some();
        dispatch_grids_inner(&self.tree, &self.panes, w, h, &metrics, cx, dragging);
    }

    /// Called by `pane_layout::build_divider` on left mouse-down to stamp
    /// the drag snapshot into `self.active_drag`. Kept as a method (rather
    /// than letting layout poke the field directly) so the field can stay
    /// private and so any future bookkeeping at drag start has one home.
    pub(super) fn begin_divider_drag(&mut self, drag: ActiveDrag, cx: &mut Context<Self>) {
        self.active_drag = Some(drag);
        cx.notify();
    }

    /// Called by `pane_layout::build_divider` on double-click — resets the
    /// addressed split to equal weights (the reference terminal / Zed dock parity). No-op
    /// if the path doesn't address a `Split`.
    pub(super) fn reset_split_weights(&mut self, path: &[usize], cx: &mut Context<Self>) {
        if let Some(current) = self.tree.split_weights(path) {
            let equal = vec![1.0; current.len()];
            if self.tree.set_split_weights(path, equal) {
                cx.notify();
            }
        }
    }

    /// Compute new weights from a live cursor position and write them back
    /// to the tree. `cursor` is in window coordinates; the math is
    /// invariant to coordinate origin because we only consume the delta
    /// against `start_position`. Skipping the update when either side
    /// would fall below `MIN_FLEX` makes the drag stop at the boundary
    /// instead of cascading.
    fn apply_drag(&mut self, cursor: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.active_drag.as_ref() else {
            return;
        };
        if drag.parent_size_px <= 0.0 {
            return;
        }
        let delta_px = match drag.axis {
            Axis::Horizontal => f32::from(cursor.x - drag.start_position.x),
            Axis::Vertical => f32::from(cursor.y - drag.start_position.y),
        };
        let sum: f32 = drag.initial_weights.iter().sum();
        if sum <= 0.0 {
            return;
        }
        let delta_w = (delta_px / drag.parent_size_px) * sum;
        let left = drag.initial_weights[drag.gap_idx];
        let right = drag.initial_weights[drag.gap_idx + 1];
        let new_left = left + delta_w;
        let new_right = right - delta_w;
        if new_left < crate::shell::pane_tree::MIN_FLEX
            || new_right < crate::shell::pane_tree::MIN_FLEX
        {
            return;
        }
        let mut new_weights = drag.initial_weights.clone();
        new_weights[drag.gap_idx] = new_left;
        new_weights[drag.gap_idx + 1] = new_right;
        let path = drag.split_path.clone();
        if self.tree.set_split_weights(&path, new_weights) {
            cx.notify();
        }
    }
}

fn focus_pane(
    panes: &HashMap<PaneId, Entity<TabbedPane>>,
    id: PaneId,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(pane) = panes.get(&id) {
        let handle = pane.read(cx).active_focus_handle(cx);
        handle.focus(window, cx);
    }
}

fn available_area(
    window: &Window,
    chrome_w_px: f32,
    pad_panel: f32,
    metrics: &CellMetrics,
) -> (f32, f32) {
    let v = window.viewport_size();
    let w = (f32::from(v.width) - chrome_w_px - pad_panel * 2.0).max(metrics.cell_width);
    let h = (f32::from(v.height) - CHROME_H_PX - pad_panel * 2.0).max(metrics.line_height);
    (w, h)
}

fn dispatch_grids_inner(
    node: &PaneTree,
    panes: &HashMap<PaneId, Entity<TabbedPane>>,
    w: f32,
    h: f32,
    metrics: &CellMetrics,
    cx: &mut Context<MainPane>,
    dragging: bool,
) {
    match node {
        PaneTree::Leaf(id) => {
            // While a divider drag is in progress we deliberately do NOT
            // call `set_target_grid` — see `MainPane::dispatch_grids` for
            // the rationale (PTY resize coalescing). The visible layout
            // still tracks the cursor via `build_node`'s pixel math.
            if dragging {
                return;
            }
            // Subtract the tab strip's height when this pane has multiple
            // tabs so the terminal rect matches what render actually shows.
            let strip_h = panes
                .get(id)
                .filter(|p| p.read(cx).tab_count() > 1)
                .map(|_| TAB_STRIP_HEIGHT_PX)
                .unwrap_or(0.0);
            let usable_h = (h - strip_h).max(metrics.line_height);
            let cols = metrics.cols_in(w).max(MIN_COLS);
            let rows = metrics.rows_in(usable_h).max(MIN_ROWS);
            if let Some(pane) = panes.get(id) {
                pane.update(cx, |tp, cx| tp.set_target_grid(cols, rows, cx));
            }
        }
        PaneTree::Split {
            axis,
            children,
            weights,
        } if !children.is_empty() => {
            // Invariant: `weights.len() == children.len()`. The zip below
            // would silently truncate to the shorter side, dropping a pane
            // from PTY dispatch (Codex flagged this in the pre-fix code).
            // `remove_leaf` is the one mutation that could plausibly break
            // it; we now panic in debug if any path got us here misaligned.
            debug_assert_eq!(
                children.len(),
                weights.len(),
                "Split invariant violated in dispatch_grids_inner"
            );
            // Reserve pixel space for the `(n-1)` dividers between siblings
            // so the cols/rows we hand the PTY match the actual painted
            // terminal rect. Without this the right-most pane reports +1 col
            // to the shell and the cursor lands past the visible glyph
            // column on resize.
            let gutter = DIVIDER_HIT_PX * (children.len().saturating_sub(1)) as f32;
            let usable = match axis {
                Axis::Horizontal => (w - gutter).max(metrics.cell_width),
                Axis::Vertical => (h - gutter).max(metrics.line_height),
            };
            let sum_w: f32 = weights.iter().sum();
            let sum_w = if sum_w > 0.0 { sum_w } else { 1.0 };
            for (c, weight) in children.iter().zip(weights.iter()) {
                let frac = weight / sum_w;
                let (cw, ch) = match axis {
                    Axis::Horizontal => (usable * frac, h),
                    Axis::Vertical => (w, usable * frac),
                };
                dispatch_grids_inner(c, panes, cw, ch, metrics, cx, dragging);
            }
        }
        PaneTree::Split { .. } => {}
    }
}

fn spawn_terminal_view(
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<MainPane>,
) -> Option<Entity<TerminalView>> {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        ..SpawnConfig::default()
    };
    let session_id = match backend.spawn(cfg) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "pty spawn failed");
            return None;
        }
    };
    Some(
        cx.new(|cx| {
            TerminalView::mount(backend, session_id, theme, density, typography, window, cx)
        }),
    )
}

impl Focusable for MainPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MainPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-sync from window focus on every render so click-to-focus moves
        // the active-pane ring + dim overlay without waiting for the next
        // action. Each TabbedPane notifies on click + tab swap; our
        // `_pane_observers` forward those notifies back here.
        self.sync_focused_from_window(window, cx);
        self.dispatch_grids(window, cx);
        let focus_handle = self.focus_handle.clone();
        let metrics = CellMetrics::measure(&self.typography, window);
        let (w, h) = available_area(window, self.chrome_w_px, self.density.pad_panel, &metrics);

        // `window.on_mouse_event` only works from the paint phase, and
        // `Render::render` runs during layout — so instead we hang a plain
        // `on_mouse_move` + `on_mouse_up` listener on the root div. Move
        // events fire any time the cursor is anywhere over MainPane (the
        // root spans the whole workspace area), which covers every
        // realistic divider drag without needing window-level tracking.

        div()
            .id("oximux-main-pane")
            .track_focus(&focus_handle)
            .size_full()
            .on_action(cx.listener(Self::on_split_horizontal))
            .on_action(cx.listener(Self::on_split_vertical))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .on_action(cx.listener(Self::on_focus_prev_pane))
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _window, cx| {
                if this.active_drag.is_none() {
                    return;
                }
                // If the left button is no longer held, the user
                // released *outside* MainPane's hitbox (e.g. over the
                // OS title bar or the sidebar) so `on_mouse_up` never
                // fired. Without this check, the divider would keep
                // following the cursor without any button held until
                // the user clicked again. Clear the drag now that we
                // can see the button state.
                if e.pressed_button != Some(MouseButton::Left) {
                    this.active_drag = None;
                    cx.notify();
                    return;
                }
                this.apply_drag(e.position, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, cx| {
                    if this.active_drag.is_some() {
                        this.active_drag = None;
                        cx.notify();
                    }
                }),
            )
            .child(build_node(
                &self.tree,
                &self.panes,
                &self.theme,
                &[],
                w,
                h,
                cx.entity().clone(),
            ))
    }
}
