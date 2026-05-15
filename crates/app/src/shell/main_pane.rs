//! MainPane — workspace grid of terminal panes (Phase 1 step 5).
//!
//! Owns a `panes: HashMap<PaneId, Entity<TerminalView>>` entity store paired
//! with a pure-data [`crate::shell::pane_tree::PaneTree`] that describes the
//! split layout. Render walks the tree top-down, looks up each leaf's
//! entity in the HashMap, divides the available rect along each Split axis,
//! and stages per-leaf `(cols, rows)` via `TerminalView::set_target_grid`.
//!
//! Action handlers (`SplitHorizontal`, `SplitVertical`, `ClosePane`,
//! `FocusNextPane`) are wired on the root render `div`. GPUI dispatches
//! actions up the element tree from the focused leaf, so ordinary
//! keystrokes still reach the focused `TerminalView` while `Cmd-*` combos
//! bubble up here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window, div, px,
};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{ClosePane, FocusNextPane, SplitHorizontal, SplitVertical};
use crate::shell::pane_tree::{Axis, PaneId, PaneTree};
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

pub struct MainPane {
    tree: PaneTree,
    panes: HashMap<PaneId, Entity<TerminalView>>,
    focused: PaneId,
    next_id: AtomicU64,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
}

impl MainPane {
    /// Build a `MainPane` around a pre-spawned initial terminal view. The
    /// PTY spawn is fallible and stays at the caller so this builder is
    /// infallible.
    pub fn new(
        initial_view: Entity<TerminalView>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = PaneId(0);
        let next_id = AtomicU64::new(1);
        let mut panes = HashMap::new();
        panes.insert(id, initial_view);
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
        }
    }

    pub fn leaf_count(&self) -> usize {
        self.tree.leaf_count()
    }

    fn alloc_id(&self) -> PaneId {
        PaneId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Sync `self.focused` from the window's currently focused element.
    /// Click-to-focus already moves the platform focus into a `TerminalView`
    /// (handled inside that view), but `MainPane.focused` only changes when
    /// we ourselves issue a focus call. Without this sync the user could
    /// click pane A, press Cmd-D, and end up splitting pane B (whichever
    /// was last action-focused). Called at the top of every action handler.
    fn sync_focused_from_window(&mut self, window: &Window, cx: &App) {
        let Some(active) = window.focused(cx) else {
            return;
        };
        for (id, view) in &self.panes {
            if view.read(cx).focus_handle(cx) == active {
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
        let new_id = self.alloc_id();
        if !self.tree.split_leaf(self.focused, axis, new_id) {
            tracing::warn!("split target not in tree; dropping new view");
            return;
        }
        self.panes.insert(new_id, view);
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

    fn on_close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
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

    fn dispatch_grids(&self, window: &Window, cx: &mut Context<Self>) {
        let (w, h) = available_area(window, self.density.pad_panel);
        dispatch_grids_inner(&self.tree, &self.panes, w, h, cx);
    }
}

fn focus_pane(
    panes: &HashMap<PaneId, Entity<TerminalView>>,
    id: PaneId,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(view) = panes.get(&id) {
        let handle = view.read(cx).focus_handle(cx);
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
    panes: &HashMap<PaneId, Entity<TerminalView>>,
    w: f32,
    h: f32,
    cx: &mut Context<MainPane>,
) {
    match node {
        PaneTree::Leaf(id) => {
            let cols = ((w / CELL_WIDTH_PX).floor() as u16).max(MIN_COLS);
            let rows = ((h / CELL_HEIGHT_PX).floor() as u16).max(MIN_ROWS);
            if let Some(view) = panes.get(id) {
                view.update(cx, |view, _| view.set_target_grid(cols, rows));
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
        self.dispatch_grids(window, cx);
        let focus_handle = self.focus_handle.clone();

        div()
            .id("oximux-main-pane")
            .track_focus(&focus_handle)
            .size_full()
            .on_action(cx.listener(Self::on_split_horizontal))
            .on_action(cx.listener(Self::on_split_vertical))
            .on_action(cx.listener(Self::on_close_pane))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .child(build_node(&self.tree, &self.panes, &self.theme))
    }
}

fn build_node(
    node: &PaneTree,
    panes: &HashMap<PaneId, Entity<TerminalView>>,
    theme: &Theme,
) -> gpui::Div {
    match node {
        PaneTree::Leaf(id) => {
            // `overflow_hidden` is load-bearing: each `TerminalView` paints
            // rows with `whitespace_nowrap`, so if the alacritty grid still
            // holds content from before a resize (or the TUI hasn't repainted
            // on SIGWINCH), cells will overflow the leaf's flex slice and
            // bleed over the next pane + its separator. Clipping here keeps
            // every pane confined to its assigned slot regardless of grid
            // state.
            let mut leaf = div()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .overflow_hidden();
            if let Some(view) = panes.get(id) {
                leaf = leaf.child(view.clone());
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
            // Apply a 1px border on the leading edge of each non-first
            // child instead of inserting a separate separator flex item.
            // Borders are part of the element's box model so they render
            // reliably regardless of flex sizing quirks (the earlier
            // separator-div approach silently dropped the outermost
            // separator in some configurations).
            for (i, c) in children.iter().enumerate() {
                let mut child = build_node(c, panes, theme);
                if i > 0 {
                    child = match axis {
                        Axis::Horizontal => child.border_l_1().border_color(theme.border_inactive),
                        Axis::Vertical => child.border_t_1().border_color(theme.border_inactive),
                    };
                }
                row = row.child(child);
            }
            row
        }
    }
}
