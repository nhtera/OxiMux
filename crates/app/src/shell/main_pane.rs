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
    ParentElement, Render, Styled, Subscription, Window, div, hsla, px,
};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    CloseTab, FocusNextPane, NewTab, NextTab, PrevTab, SplitHorizontal, SplitVertical,
};
use crate::shell::pane_tree::{Axis, PaneId, PaneTree};
use crate::shell::tabbed_pane::{TAB_STRIP_HEIGHT_PX, TabbedPane};
use crate::shell::terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView};

/// Hardcoded cell metrics for Geist Mono 14 px. Replace with
/// `text_system().line_height()` + advance lookup during the step 9
/// perf/measurement pass.
const CELL_WIDTH_PX: f32 = 8.4;
const CELL_HEIGHT_PX: f32 = 18.0;

/// Chrome subtracted from viewport when computing the terminal area. Mirrors
/// `WorkspaceRoot` composition + density tokens.
const CHROME_W_PX: f32 = 240.0; // sidebar
const CHROME_H_PX: f32 = 36.0 + 22.0; // top bar + status bar

const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;

/// Translucent veil drawn over unfocused leaves. Mid-charcoal at ~22% alpha
/// reads as "dim, but the text under it is still legible" — close to iTerm2's
/// inactive-pane treatment without going full grey-out.
const DIM_ALPHA: f32 = 0.22;

pub struct MainPane {
    tree: PaneTree,
    panes: HashMap<PaneId, Entity<TabbedPane>>,
    focused: PaneId,
    next_id: AtomicU64,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    /// One observer per TabbedPane. When a pane notifies (click, keystroke,
    /// PTY output via its tab's bubbled notify, tab swap) we re-notify
    /// MainPane so render runs and `sync_focused_from_window` repaints the
    /// active-pane ring + dim overlays. Stored to keep subscriptions alive.
    _pane_observers: HashMap<PaneId, Subscription>,
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
        Self {
            tree: PaneTree::Leaf(id),
            panes,
            focused: id,
            next_id,
            theme,
            density,
            typography,
            focus_handle,
            _pane_observers: pane_observers,
        }
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
        self.sync_focused_from_window(window, cx);
        let leaves = self.tree.in_order_leaves();
        if leaves.len() < 2 {
            return;
        }
        let Some(pos) = leaves.iter().position(|id| *id == self.focused) else {
            return;
        };
        let next = leaves[(pos + 1) % leaves.len()];
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
        let (w, h) = available_area(window, self.density.pad_panel);
        dispatch_grids_inner(&self.tree, &self.panes, w, h, cx);
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

fn available_area(window: &Window, pad_panel: f32) -> (f32, f32) {
    let v = window.viewport_size();
    let w = (f32::from(v.width) - CHROME_W_PX - pad_panel * 2.0).max(CELL_WIDTH_PX);
    let h = (f32::from(v.height) - CHROME_H_PX - pad_panel * 2.0).max(CELL_HEIGHT_PX);
    (w, h)
}

fn dispatch_grids_inner(
    node: &PaneTree,
    panes: &HashMap<PaneId, Entity<TabbedPane>>,
    w: f32,
    h: f32,
    cx: &mut Context<MainPane>,
) {
    match node {
        PaneTree::Leaf(id) => {
            // Subtract the tab strip's height when this pane has multiple
            // tabs so the terminal rect matches what render actually shows.
            let strip_h = panes
                .get(id)
                .filter(|p| p.read(cx).tab_count() > 1)
                .map(|_| TAB_STRIP_HEIGHT_PX)
                .unwrap_or(0.0);
            let usable_h = (h - strip_h).max(CELL_HEIGHT_PX);
            let cols = ((w / CELL_WIDTH_PX).floor() as u16).max(MIN_COLS);
            let rows = ((usable_h / CELL_HEIGHT_PX).floor() as u16).max(MIN_ROWS);
            if let Some(pane) = panes.get(id) {
                pane.update(cx, |tp, cx| tp.set_target_grid(cols, rows, cx));
            }
        }
        PaneTree::Split { axis, children } if !children.is_empty() => {
            let n = children.len() as f32;
            let (cw, ch) = match axis {
                Axis::Horizontal => (w / n, h),
                Axis::Vertical => (w, h / n),
            };
            for c in children {
                dispatch_grids_inner(c, panes, cw, ch, cx);
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

        div()
            .id("oximux-main-pane")
            .track_focus(&focus_handle)
            .size_full()
            .on_action(cx.listener(Self::on_split_horizontal))
            .on_action(cx.listener(Self::on_split_vertical))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .child(build_node(
                &self.tree,
                &self.panes,
                &self.theme,
                self.focused,
            ))
    }
}

fn build_node(
    node: &PaneTree,
    panes: &HashMap<PaneId, Entity<TabbedPane>>,
    theme: &Theme,
    focused: PaneId,
) -> gpui::Div {
    match node {
        PaneTree::Leaf(id) => {
            // `overflow_hidden` is load-bearing: each `TerminalView` paints
            // rows with `whitespace_nowrap`, so if the alacritty grid still
            // holds content from before a resize (or the TUI hasn't
            // repainted on SIGWINCH), cells will overflow the leaf's flex
            // slice and bleed over the next pane + its separator. Clipping
            // here keeps every pane confined to its assigned slot.
            //
            // `relative` is load-bearing too: the unfocused dim veil below
            // uses `absolute().inset_0()` to cover the leaf without
            // affecting layout, and absolute positions resolve against the
            // nearest positioned ancestor.
            let mut leaf = div()
                .relative()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .overflow_hidden();
            if let Some(pane) = panes.get(id) {
                leaf = leaf.child(pane.clone());
            }
            // Dim overlay for unfocused leaves. Plain non-interactive div —
            // no `.occlude()`, no `.id()`, no listeners — so mouse-downs
            // pass through to the TerminalView beneath and click-to-focus
            // works on the first click. DO NOT add `.occlude()`, an `.id()`,
            // or any event handler here without re-routing pointer events,
            // or click-to-activate on unfocused panes will silently break.
            if *id != focused {
                leaf = leaf.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(hsla(0.0, 0.0, 0.0, DIM_ALPHA)),
                );
            }
            leaf
        }
        PaneTree::Split { axis, children } => {
            let mut row = div()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .overflow_hidden();
            row = match axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            // Each non-first child carries a 1px separator border on its
            // leading edge (border_inactive). The focused leaf gets a 2px
            // ring on all sides (focus_ring) that overrides any separator
            // border — this is the visible "active pane" indicator.
            for (i, c) in children.iter().enumerate() {
                let mut child = build_node(c, panes, theme, focused);
                if i > 0 {
                    child = match axis {
                        Axis::Horizontal => child.border_l_1().border_color(theme.border_inactive),
                        Axis::Vertical => child.border_t_1().border_color(theme.border_inactive),
                    };
                }
                if let PaneTree::Leaf(leaf_id) = c
                    && *leaf_id == focused
                {
                    child = child.border_2().border_color(theme.focus_ring);
                }
                row = row.child(child);
            }
            row
        }
    }
}
