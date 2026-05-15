//! MainPane — workspace grid of terminal panes (Phase 1 step 5).
//!
//! Two parallel structures: [`PaneTree`] is pure layout shape with
//! [`PaneId`] leaves; `panes: HashMap<PaneId, Entity<TerminalView>>` holds the GPUI
//! entities keyed by id. Keeping the tree payload-free makes split / close
//! / focus ops easy to unit-test without a real GPUI app, and turns
//! "find the view for id X" into an O(1) hash lookup instead of a tree walk.
//!
//! Splits arrange children along an axis with equal `flex_1` ratios — no
//! drag-resize in this slice. Action handlers (`SplitHorizontal`,
//! `SplitVertical`, `ClosePane`, `FocusNextPane`) are wired on the root
//! render `div`; GPUI dispatches actions up the element tree from the
//! focused leaf, so ordinary keystrokes still reach the focused
//! `TerminalView` while `Cmd-*` combos bubble up here.
//!
//! Each render walks the tree top-down with the available rect, computes
//! each leaf's `(cols, rows)` from its slice of the area divided by
//! hardcoded cell metrics, and stages the target on the leaf via
//! `TerminalView::set_target_grid`. The leaf applies the resize on its
//! next paint tick.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window, div, px,
};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{ClosePane, FocusNextPane, SplitHorizontal, SplitVertical};
use crate::shell::terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView};

/// Stable identifier for a pane leaf in the workspace tree. Issued
/// monotonically by `MainPane`; never reused after a pane closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTree {
    Leaf(PaneId),
    Split { axis: Axis, children: Vec<PaneTree> },
}

impl PaneTree {
    pub fn leaf_count(&self) -> usize {
        match self {
            PaneTree::Leaf(_) => 1,
            PaneTree::Split { children, .. } => children.iter().map(Self::leaf_count).sum(),
        }
    }

    pub fn in_order_leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            PaneTree::Leaf(id) => out.push(*id),
            PaneTree::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }

    /// Path of child indices from root to the named leaf, or `None` if the
    /// id isn't in the tree. Empty `Vec` means root is the leaf.
    fn path_to(&self, target: PaneId) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        if self.path_to_inner(target, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn path_to_inner(&self, target: PaneId, path: &mut Vec<usize>) -> bool {
        match self {
            PaneTree::Leaf(id) => *id == target,
            PaneTree::Split { children, .. } => {
                for (i, c) in children.iter().enumerate() {
                    path.push(i);
                    if c.path_to_inner(target, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
        }
    }

    /// Replace the leaf matching `target` with a `Split { axis, [old, new] }`.
    /// Returns true on success.
    pub fn split_leaf(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        let Some(path) = self.path_to(target) else {
            return false;
        };
        let node = descend_mut(self, &path);
        // Placeholder: any valid PaneTree works; we overwrite immediately.
        let placeholder = PaneTree::Split {
            axis: Axis::Horizontal,
            children: Vec::new(),
        };
        let old = std::mem::replace(node, placeholder);
        *node = PaneTree::Split {
            axis,
            children: vec![old, PaneTree::Leaf(new_id)],
        };
        true
    }

    /// Remove the leaf matching `target` and collapse any single-child
    /// Splits. Returns false when the target is the root leaf (caller must
    /// guard "only one pane remains") or not in the tree.
    pub fn remove_leaf(&mut self, target: PaneId) -> bool {
        let Some(path) = self.path_to(target) else {
            return false;
        };
        if path.is_empty() {
            return false;
        }
        let parent = descend_mut(self, &path[..path.len() - 1]);
        let last = *path.last().unwrap();
        match parent {
            PaneTree::Split { children, .. } => {
                children.remove(last);
            }
            PaneTree::Leaf(_) => unreachable!("path leads through Split nodes"),
        }
        self.collapse_singletons();
        true
    }

    /// Collapse any `Split` node with exactly one child into that child,
    /// recursively. Splits with zero children should not occur in a
    /// well-formed tree.
    fn collapse_singletons(&mut self) {
        if let PaneTree::Split { children, .. } = self {
            for c in children.iter_mut() {
                c.collapse_singletons();
            }
        }
        let demote = matches!(self, PaneTree::Split { children, .. } if children.len() == 1);
        if demote {
            let only = match self {
                PaneTree::Split { children, .. } => children.remove(0),
                _ => unreachable!(),
            };
            *self = only;
        }
    }
}

fn descend_mut<'a>(root: &'a mut PaneTree, path: &[usize]) -> &'a mut PaneTree {
    let mut node = root;
    for &i in path {
        node = match node {
            PaneTree::Split { children, .. } => &mut children[i],
            PaneTree::Leaf(_) => unreachable!("path is invariant on internal Split nodes"),
        };
    }
    node
}

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
        self.split_focused(Axis::Horizontal, window, cx);
    }

    fn on_split_vertical(
        &mut self,
        _: &SplitVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_focused(Axis::Vertical, window, cx);
    }

    fn on_close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
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
            .child(build_node(&self.tree, &self.panes))
    }
}

fn build_node(node: &PaneTree, panes: &HashMap<PaneId, Entity<TerminalView>>) -> gpui::Div {
    match node {
        PaneTree::Leaf(id) => {
            let mut leaf = div().flex().flex_1().min_w(px(0.)).min_h(px(0.));
            if let Some(view) = panes.get(id) {
                leaf = leaf.child(view.clone());
            }
            leaf
        }
        PaneTree::Split { axis, children } => {
            let mut row = div().flex().flex_1().min_w(px(0.)).min_h(px(0.));
            row = match axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            for c in children {
                row = row.child(build_node(c, panes));
            }
            row
        }
    }
}

#[cfg(test)]
mod tests {
    //! Pure-structure smoke tests for `PaneTree`. No GPUI, no entities —
    //! these exercise the split / remove / focus-order math that drives the
    //! action handlers.

    use super::*;

    fn id(n: u64) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn single_leaf_has_count_one() {
        let t = PaneTree::Leaf(id(0));
        assert_eq!(t.leaf_count(), 1);
        assert_eq!(t.in_order_leaves(), vec![id(0)]);
    }

    #[test]
    fn split_horizontal_creates_two_leaves() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf(id(0), Axis::Horizontal, id(1)));
        assert_eq!(t.leaf_count(), 2);
        assert_eq!(t.in_order_leaves(), vec![id(0), id(1)]);
        match &t {
            PaneTree::Split { axis, .. } => assert_eq!(*axis, Axis::Horizontal),
            _ => panic!("expected split root"),
        }
    }

    #[test]
    fn nested_split_2x2() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf(id(0), Axis::Horizontal, id(1)));
        assert!(t.split_leaf(id(1), Axis::Vertical, id(2)));
        assert_eq!(t.leaf_count(), 3);
        assert_eq!(t.in_order_leaves(), vec![id(0), id(1), id(2)]);
    }

    #[test]
    fn split_missing_target_returns_false() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(!t.split_leaf(id(99), Axis::Horizontal, id(1)));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_collapses_single_child_split() {
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        assert!(t.remove_leaf(id(1)));
        // After removal the Split has one child (id 0); collapse demotes it
        // back to a bare Leaf so the tree stays normalized.
        assert_eq!(t, PaneTree::Leaf(id(0)));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_root_leaf_returns_false() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(!t.remove_leaf(id(0)));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_nested_leaf_preserves_siblings() {
        // Start: Split-H [0, Split-V [1, 2]]. Remove 1 → Split-H [0, 2].
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        t.split_leaf(id(1), Axis::Vertical, id(2));
        assert!(t.remove_leaf(id(1)));
        assert_eq!(t.in_order_leaves(), vec![id(0), id(2)]);
        match &t {
            PaneTree::Split { axis, children } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected horizontal split root"),
        }
    }
}
