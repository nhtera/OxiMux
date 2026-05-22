//! MainPane — workspace grid of pane leaves (terminal or editor).
//!
//! Owns a `panes: HashMap<PaneId, PaneContent>` entity store paired with a
//! pure-data [`crate::shell::pane_tree::PaneTree`] describing the split
//! layout. Render walks the tree top-down, looks up each leaf's content in
//! the HashMap, divides the available rect along each Split axis, and for
//! terminal leaves stages per-leaf `(cols, rows)` via
//! `TerminalView::set_target_grid`. Editor leaves participate in the layout
//! but ignore grid dispatch — they size to their assigned rect via GPUI
//! flex.
//!
//! Action handlers (`SplitHorizontal`, `SplitVertical`, `FocusNextPane`,
//! `FocusPrevPane`) are wired on the root render `div`. GPUI dispatches
//! actions up the element tree from the focused leaf, so ordinary keystrokes
//! still reach the focused view while `Cmd-*` combos bubble up here.
//!
//! Per-leaf tabs were removed when the workspace switched to a single-tab-
//! per-leaf model: each leaf hosts a single content entity, and the
//! workspace-level [`crate::shell::workspace_tabs::WorkspaceTabs`] handles
//! tab creation / switching above this struct.

pub mod pane_content;

pub use pane_content::PaneContent;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render, Styled, Subscription, Window,
    div,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    FocusNextPane, FocusPrevPane, SplitDown, SplitHorizontal, SplitLeft, SplitRight, SplitUp,
    SplitVertical,
};
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::pane_layout::{ActiveDrag, DIVIDER_HIT_PX, build_node};
use crate::shell::pane_tree::{Axis, PaneId, PaneTree, SplitInsert};
use crate::shell::terminal_view::{TerminalView, spawn_local_pty};
use oximux_editor::EditorView;

/// Vertical chrome subtracted from viewport when computing terminal area
/// (top bar + status bar). Density tokens are 40 + 24 = 64.
const CHROME_H_PX: f32 = 40.0 + 24.0;

const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;

pub struct MainPane {
    tree: PaneTree,
    panes: HashMap<PaneId, PaneContent>,
    focused: PaneId,
    next_id: AtomicU64,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    /// Horizontal chrome reserved by surrounding panels (left rail + right
    /// sidebar). Live value; updated by `WorkspaceTabs` on every render so the
    /// PTY grid shrinks when the right sidebar opens. Defaults to the left
    /// rail width so initial mount before the first sync still produces a
    /// sane terminal grid.
    chrome_w_px: f32,
    /// Spawn cwd for new panes created by split actions. Inherited from the
    /// owning `WorkspaceTabs` (= active project's `root_path`).
    cwd: PathBuf,
    /// Bumped on every topology change (split, weight set, leaf removal).
    /// `WorkspaceTabs` reads this in its `cx.observe(&pane)` callback to
    /// debounce persistence — PTY-output `cx.notify()` calls would
    /// otherwise trigger a settings.set per 16ms tick.
    topology_version: u64,
    /// One observer per leaf so the focus ring repaints on click / PTY output.
    _pane_observers: HashMap<PaneId, Subscription>,
    /// Set on divider `on_mouse_down`, read by the element-level
    /// `on_mouse_move` listener on the MainPane root.
    active_drag: Option<ActiveDrag>,
}

impl MainPane {
    /// Build a `MainPane` around a pre-spawned initial TerminalView.
    pub fn new(
        initial_view: Entity<TerminalView>,
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = PaneId(0);
        let next_id = AtomicU64::new(1);
        let content = PaneContent::Terminal(initial_view);
        let sub = observe_pane_focus(&content, id, cx);
        let mut panes = HashMap::new();
        panes.insert(id, content);
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
            cwd,
            topology_version: 0,
            _pane_observers: pane_observers,
            active_drag: None,
        }
    }

    /// Build a `MainPane` whose single leaf hosts an editor on `path`.
    /// Used by the workspace tab strip when opening a file as a new tab
    /// — the resulting `MainPane` has no terminal leaf, so no PTY is
    /// spawned. Splitting (Cmd+D/etc.) inside this pane will create
    /// fresh terminal siblings as usual; only the seed leaf is an editor.
    pub fn new_with_editor(
        initial_editor: Entity<EditorView>,
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = PaneId(0);
        let next_id = AtomicU64::new(1);
        let content = PaneContent::Editor(initial_editor);
        let sub = observe_pane_focus(&content, id, cx);
        let mut panes = HashMap::new();
        panes.insert(id, content);
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
            cwd,
            topology_version: 0,
            _pane_observers: pane_observers,
            active_drag: None,
        }
    }

    /// Construct a MainPane around a pre-built pane tree + content map. Used
    /// by the persistence restore path: `build_workspace_tabs` spawns N
    /// `TerminalView`s in DFS-leaf order, wraps each in `PaneContent::Terminal`,
    /// assembles them into a `PaneTree` via `persisted_terminals::restore_tree`,
    /// then hands the pair here.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_tree(
        tree: PaneTree,
        panes: HashMap<PaneId, PaneContent>,
        focused: PaneId,
        next_id_seed: u64,
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut pane_observers = HashMap::with_capacity(panes.len());
        for (id, content) in &panes {
            pane_observers.insert(*id, observe_pane_focus(content, *id, cx));
        }
        Self {
            tree,
            panes,
            focused,
            next_id: AtomicU64::new(next_id_seed),
            theme,
            density,
            typography,
            focus_handle: cx.focus_handle(),
            chrome_w_px: density.w_left_rail,
            cwd,
            topology_version: 0,
            _pane_observers: pane_observers,
            active_drag: None,
        }
    }

    /// Read accessor for the topology-version dedup counter.
    pub fn topology_version(&self) -> u64 {
        self.topology_version
    }

    /// Read accessor for the pane tree — `WorkspaceTabs` snapshots this for
    /// persistence via `persisted_terminals::snapshot_tree`.
    pub fn tree(&self) -> &PaneTree {
        &self.tree
    }

    /// Update the surrounding chrome width so PTY grids match the actual
    /// MainPane rect. No-op if the value matches to avoid a render → update →
    /// render loop.
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

    /// Walk the tree in DFS leaf order and return one byte buffer per
    /// TERMINAL leaf via `TerminalBackend::serialize_buffer`. Editor leaves
    /// are silently skipped — their content lives on disk, not in a
    /// scrollback. Used by the scrollback persistence path (Phase 4 step 16);
    /// ordinal index in the returned Vec lines up with the restore-time
    /// terminal-leaf order produced by `restore_tree` so capture and restore
    /// agree without an explicit map.
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &gpui::App) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.panes.len());
        for leaf_id in self.tree.in_order_leaves() {
            let Some(PaneContent::Terminal(view)) = self.panes.get(&leaf_id) else {
                continue;
            };
            let bytes = view.read(cx).serialize_buffer(max_bytes);
            out.push(bytes);
        }
        out
    }

    /// Walk the tree in DFS leaf order and return one external id per
    /// TERMINAL leaf (relay PTY id for relay-backed backends, `None` for
    /// in-process). Editor leaves are silently skipped. Same ordering as
    /// `collect_pane_buffers` so the two captures stay aligned in the
    /// persisted `pane_relay_ids` table.
    pub fn collect_pane_external_ids(&self, cx: &gpui::App) -> Vec<Option<String>> {
        let mut out = Vec::with_capacity(self.panes.len());
        for leaf_id in self.tree.in_order_leaves() {
            let Some(PaneContent::Terminal(view)) = self.panes.get(&leaf_id) else {
                continue;
            };
            out.push(view.read(cx).external_id());
        }
        out
    }

    /// Inverse of `collect_pane_buffers`: feed previously-captured bytes
    /// into each TERMINAL leaf's grid BEFORE the live PTY produces any
    /// output. Buffers are paired with terminal leaves in DFS order; an
    /// empty buffer means "no capture for this leaf" and is skipped.
    /// Editor leaves are silently skipped.
    pub fn prefill_leaves(&self, buffers: &[Vec<u8>], cx: &gpui::App) {
        let mut buf_iter = buffers.iter();
        for leaf_id in self.tree.in_order_leaves() {
            let Some(PaneContent::Terminal(view)) = self.panes.get(&leaf_id) else {
                continue;
            };
            let Some(bytes) = buf_iter.next() else {
                break;
            };
            if bytes.is_empty() {
                continue;
            }
            view.read(cx).prefill_grid(bytes);
        }
    }

    /// Focus handle for the currently focused leaf. Used by
    /// `WorkspaceTabs::focus_active` so switching tabs re-homes focus inside
    /// the destination tab's last-focused pane.
    pub fn active_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.panes.get(&self.focused) {
            Some(content) => content.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }

    /// File path of the editor currently shown in the focused leaf
    /// (`None` if the focused leaf is a terminal). Drives the Files-tab
    /// active-row highlight when a workspace tab is rendering this pane.
    pub fn active_editor_path(&self, cx: &App) -> Option<PathBuf> {
        self.panes
            .get(&self.focused)?
            .editor_path(cx)
            .map(|p| p.to_path_buf())
    }

    fn alloc_id(&self) -> PaneId {
        PaneId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }


    /// Sync `self.focused` from the window's currently focused element.
    /// Click-to-focus moves platform focus into a leaf view directly, but
    /// `MainPane.focused` only changes when we ourselves issue a focus call.
    /// Without this sync the user could click pane A, press Cmd-D, and end up
    /// splitting pane B (whichever was last action-focused).
    fn sync_focused_from_window(&mut self, window: &Window, cx: &App) {
        let Some(active) = window.focused(cx) else {
            return;
        };
        for (id, content) in &self.panes {
            if content.focus_handle(cx) == active {
                self.focused = *id;
                return;
            }
        }
    }

    fn split_focused(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = spawn_terminal_view(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            window,
            cx,
        ) else {
            return;
        };
        let new_id = self.alloc_id();
        if !self.tree.split_leaf_at(self.focused, axis, new_id, insert) {
            tracing::warn!("split target not in tree; dropping new pane");
            return;
        }
        let content = PaneContent::Terminal(view);
        let sub = observe_pane_focus(&content, new_id, cx);
        self._pane_observers.insert(new_id, sub);
        self.panes.insert(new_id, content);
        self.focused = new_id;
        focus_pane(&self.panes, new_id, window, cx);
        self.topology_version += 1;
        cx.notify();
    }

    fn on_split_horizontal(
        &mut self,
        _: &SplitHorizontal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Horizontal, SplitInsert::After, window, cx);
    }

    fn on_split_vertical(
        &mut self,
        _: &SplitVertical,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Vertical, SplitInsert::After, window, cx);
    }

    fn on_split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Horizontal, SplitInsert::After, window, cx);
    }

    fn on_split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Vertical, SplitInsert::After, window, cx);
    }

    fn on_split_left(&mut self, _: &SplitLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Horizontal, SplitInsert::Before, window, cx);
    }

    fn on_split_up(&mut self, _: &SplitUp, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_focused_from_window(window, cx);
        self.split_focused(Axis::Vertical, SplitInsert::Before, window, cx);
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

    fn dispatch_grids(&self, window: &Window, cx: &mut Context<Self>) {
        let metrics = CellMetrics::measure(&self.typography, window);
        let (w, h) = available_area(window, self.chrome_w_px, self.density.pad_panel, &metrics);
        // Defer PTY resize during an active divider drag (the reference terminal / iTerm
        // pattern). The visible layout still updates via build_node's pixel
        // math; only the PTY backend resize is suppressed until mouse-up.
        let dragging = self.active_drag.is_some();
        dispatch_grids_inner(&self.tree, &self.panes, w, h, &metrics, cx, dragging);
    }

    /// Called by `pane_layout::build_divider` on left mouse-down to stamp the
    /// drag snapshot into `self.active_drag`.
    pub(super) fn begin_divider_drag(&mut self, drag: ActiveDrag, cx: &mut Context<Self>) {
        self.active_drag = Some(drag);
        cx.notify();
    }

    /// Reset the addressed split to equal weights (the reference terminal / Zed parity).
    pub(super) fn reset_split_weights(&mut self, path: &[usize], cx: &mut Context<Self>) {
        if let Some(current) = self.tree.split_weights(path) {
            let equal = vec![1.0; current.len()];
            if self.tree.set_split_weights(path, equal) {
                self.topology_version += 1;
                cx.notify();
            }
        }
    }

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
            self.topology_version += 1;
            cx.notify();
        }
    }
}

fn focus_pane(
    panes: &HashMap<PaneId, PaneContent>,
    id: PaneId,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(content) = panes.get(&id) {
        let handle = content.focus_handle(cx);
        handle.focus(window, cx);
    }
}

/// Observe a pane's content entity and mirror its focus state into
/// `MainPane.focused`. Click-to-focus moves platform focus into the leaf
/// view directly; without this observer, `MainPane.focused` would lag
/// until the next action handler ran `sync_focused_from_window`. That lag
/// broke "restore last-interacted pane on project switch" because the
/// field was stale by capture time.
///
/// Each PaneContent variant gets its own typed observer because GPUI's
/// `cx.observe` is generic over the watched entity type — there is no
/// way to subscribe to "either Terminal or Editor" with one call.
fn observe_pane_focus(
    content: &PaneContent,
    pane_id: PaneId,
    cx: &mut Context<MainPane>,
) -> Subscription {
    match content {
        PaneContent::Terminal(view) => cx.observe(view, move |this, view, cx| {
            if view.read(cx).focused() && this.focused != pane_id {
                this.focused = pane_id;
            }
            cx.notify();
        }),
        PaneContent::Editor(view) => cx.observe(view, move |this, view, cx| {
            if view.read(cx).focused() && this.focused != pane_id {
                this.focused = pane_id;
            }
            cx.notify();
        }),
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
    panes: &HashMap<PaneId, PaneContent>,
    w: f32,
    h: f32,
    metrics: &CellMetrics,
    cx: &mut Context<MainPane>,
    dragging: bool,
) {
    match node {
        PaneTree::Leaf(id) => {
            // While a divider drag is in progress we deliberately do NOT call
            // `set_target_grid` — see `MainPane::dispatch_grids` for the
            // rationale (PTY resize coalescing).
            if dragging {
                return;
            }
            let cols = metrics.cols_in(w).max(MIN_COLS);
            let rows = metrics.rows_in(h).max(MIN_ROWS);
            // Editor leaves participate in the layout but do not own a PTY
            // grid — skip them. Terminal leaves stage the resize on the
            // backend.
            if let Some(PaneContent::Terminal(view)) = panes.get(id) {
                view.update(cx, |v, _| v.set_target_grid(cols, rows));
            }
        }
        PaneTree::Split {
            axis,
            children,
            weights,
        } if !children.is_empty() => {
            debug_assert_eq!(
                children.len(),
                weights.len(),
                "Split invariant violated in dispatch_grids_inner"
            );
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
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<MainPane>,
) -> Option<Entity<TerminalView>> {
    let (backend, session_id) = spawn_local_pty(cwd)?;
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
        // action.
        self.sync_focused_from_window(window, cx);
        self.dispatch_grids(window, cx);
        let focus_handle = self.focus_handle.clone();
        let metrics = CellMetrics::measure(&self.typography, window);
        let (w, h) = available_area(window, self.chrome_w_px, self.density.pad_panel, &metrics);

        div()
            .id("oximux-main-pane")
            .track_focus(&focus_handle)
            .size_full()
            .on_action(cx.listener(Self::on_split_horizontal))
            .on_action(cx.listener(Self::on_split_vertical))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_split_down))
            .on_action(cx.listener(Self::on_split_left))
            .on_action(cx.listener(Self::on_split_up))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .on_action(cx.listener(Self::on_focus_prev_pane))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _window, cx| {
                if this.active_drag.is_none() {
                    return;
                }
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
