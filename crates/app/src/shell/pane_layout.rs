//! Visual layout for the MainPane workspace grid.
//!
//! Pure view construction — given a `PaneTree` + pixel rect, returns a GPUI
//! `Div` containing nested leaves and resize-divider handles. Drag state
//! and tree mutation live in `super::main_pane` (`MainPane`); this module
//! only emits the elements and routes mouse events back via
//! `Entity<MainPane>::update`.

use std::collections::HashMap;

use gpui::{
    Context, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, div, px,
};
use oximux_settings::Theme;

use crate::shell::main_pane::{MainPane, PaneContent};
use crate::shell::pane_tree::{Axis, PaneId, PaneTree};

/// Width of the resize handle hit zone between adjacent panes. The outer
/// wrapper is transparent; only the 1 px center stripe (`DIVIDER_LINE_PX`)
/// is painted, so the visible line stays thin (the reference terminal-style) while the
/// drag target stays comfortably wide. Also consumed by
/// `dispatch_grids_inner` to subtract gutter space from PTY cols.
pub const DIVIDER_HIT_PX: f32 = 6.0;

/// Visible width of the divider line, painted at the center of the
/// `DIVIDER_HIT_PX` hit zone. 1 px lands at the same pixel scale as
/// terminal cell borders so the line reads as native chrome, not a
/// chunky bar.
const DIVIDER_LINE_PX: f32 = 1.0;

/// Snapshot stamped on divider mouse-down. Subsequent move events compute
/// `delta = current_cursor - start_position` and translate it into a
/// weight delta against `parent_size_px`. The mutation is applied by
/// `MainPane::apply_drag`.
#[derive(Clone, Debug)]
pub struct ActiveDrag {
    pub split_path: Vec<usize>,
    /// Left/top child index of the gap. The right/bottom sibling is
    /// `gap_idx + 1`.
    pub gap_idx: usize,
    pub axis: Axis,
    pub initial_weights: Vec<f32>,
    pub start_position: gpui::Point<gpui::Pixels>,
    /// Parent Split's axis-aligned pixel size at drag start.
    /// `delta_weight = (delta_px / parent_size_px) * sum(initial_weights)`.
    pub parent_size_px: f32,
}

/// Recursive view builder. Sizing is driven entirely by explicit pixel math
/// — each leaf and divider gets an absolute `.w()/.h()`, and the Split row
/// is a flex container only so the children stack in order. We do NOT use
/// `flex_grow` for distribution because GPUI's `flex_grow` only toggles
/// between 0 and 1 (no fractional grow factor), and mixing `flex_grow=1`
/// with explicit `.w(px)` lets panes expand past their target width and
/// crush the divider to 0 px. `(w, h)` is the pixel rect this node owns.
pub fn build_node(
    node: &PaneTree,
    panes: &HashMap<PaneId, PaneContent>,
    theme: &Theme,
    path: &[usize],
    w: f32,
    h: f32,
    main_pane: Entity<MainPane>,
) -> gpui::Div {
    match node {
        PaneTree::Leaf(id) => {
            // `overflow_hidden` is load-bearing: `TerminalView` paints rows
            // with `whitespace_nowrap`, so if the alacritty grid still holds
            // pre-resize content (or the TUI hasn't repainted on SIGWINCH),
            // cells will overflow the leaf's slot and bleed into the next
            // pane + its separator.
            let mut leaf = div().w(px(w)).h(px(h)).flex_shrink_0().overflow_hidden();
            if let Some(content) = panes.get(id) {
                match content {
                    PaneContent::Terminal(view) => leaf = leaf.child(view.clone()),
                    PaneContent::Editor(view) => leaf = leaf.child(view.clone()),
                }
            }
            leaf
        }
        PaneTree::Split {
            axis,
            children,
            weights,
        } => {
            // Invariant: `weights.len() == children.len()`. The zip below
            // would silently truncate to the shorter side, dropping a pane
            // from render (Codex flagged this). Panic in debug if violated.
            debug_assert_eq!(
                children.len(),
                weights.len(),
                "Split invariant violated in build_node"
            );
            let mut row = div()
                .flex()
                .w(px(w))
                .h(px(h))
                .min_w(px(0.))
                .min_h(px(0.))
                .overflow_hidden();
            row = match axis {
                Axis::Horizontal => row.flex_row(),
                Axis::Vertical => row.flex_col(),
            };
            let n = children.len();
            let gutter = DIVIDER_HIT_PX * (n.saturating_sub(1)) as f32;
            let parent_axis_size = match axis {
                Axis::Horizontal => (w - gutter).max(0.0),
                Axis::Vertical => (h - gutter).max(0.0),
            };
            let sum_w: f32 = weights.iter().sum();
            let sum_w = if sum_w > 0.0 { sum_w } else { 1.0 };
            for (i, (c, weight)) in children.iter().zip(weights.iter()).enumerate() {
                let mut child_path = path.to_vec();
                child_path.push(i);
                let frac = weight / sum_w;
                let (cw, ch) = match axis {
                    Axis::Horizontal => (parent_axis_size * frac, h),
                    Axis::Vertical => (w, parent_axis_size * frac),
                };
                row = row.child(build_node(
                    c,
                    panes,
                    theme,
                    &child_path,
                    cw,
                    ch,
                    main_pane.clone(),
                ));
                if i + 1 < n {
                    // For a Horizontal split the divider's cross axis is
                    // the row's height; for Vertical it's the row's width.
                    let cross_size = match axis {
                        Axis::Horizontal => h,
                        Axis::Vertical => w,
                    };
                    row = row.child(build_divider(
                        *axis,
                        path,
                        i,
                        weights.clone(),
                        parent_axis_size,
                        cross_size,
                        theme,
                        main_pane.clone(),
                    ));
                }
            }
            row
        }
    }
}

/// One resize divider between siblings `gap_idx` and `gap_idx + 1` of the
/// Split at `parent_path`. Visual: a `DIVIDER_LINE_PX` (1 px) center stripe
/// painted in `theme.border_active`, surrounded by a transparent gutter that
/// brings the hit zone up to `DIVIDER_HIT_PX` (6 px) — thin
/// line with a comfortable drag target. On left-press the divider asks
/// `MainPane` to begin a drag (or reset the split on double-click); the
/// MainPane root's `on_mouse_move`/`on_mouse_up` listeners drive the rest.
///
/// Clippy flags 8 args as `too_many_arguments`. Each one carries a distinct
/// concern (identity, drag state, layout dimension, style, callback target);
/// bundling into a struct would just add noise since the function is called
/// from exactly one place.
#[allow(clippy::too_many_arguments)]
fn build_divider(
    axis: Axis,
    parent_path: &[usize],
    gap_idx: usize,
    initial_weights: Vec<f32>,
    parent_axis_size: f32,
    cross_axis_size: f32,
    theme: &Theme,
    main_pane: Entity<MainPane>,
) -> impl IntoElement {
    let id_str = format!(
        "div-{}-{}-{:?}",
        parent_path
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("-"),
        gap_idx,
        axis,
    );
    let split_path = parent_path.to_vec();
    let parent_size = parent_axis_size.max(1.0);

    // Outer wrapper: full hit zone, transparent (no bg) so only the inner
    // line is visible. flex+center positions the inner line at the middle
    // of the gutter.
    //
    // NOTE: do NOT add `.occlude()` here. `BlockMouse` hitboxes break the
    // hit-test chain — once a BlockMouse hitbox is hit, ancestor hitboxes
    // are dropped from `is_hovered`, which silences `MainPane`'s
    // `on_mouse_up`/`on_mouse_move` listeners any frame the cursor is over
    // the divider. That strands the drag with no way to release.
    let base = div()
        .id(SharedString::from(id_str))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center();
    let (base, line) = match axis {
        Axis::Horizontal => (
            base.w(px(DIVIDER_HIT_PX))
                .h(px(cross_axis_size))
                .cursor_col_resize(),
            div()
                .w(px(DIVIDER_LINE_PX))
                .h(px(cross_axis_size))
                .bg(theme.border_active),
        ),
        Axis::Vertical => (
            base.h(px(DIVIDER_HIT_PX))
                .w(px(cross_axis_size))
                .cursor_row_resize(),
            div()
                .h(px(DIVIDER_LINE_PX))
                .w(px(cross_axis_size))
                .bg(theme.border_active),
        ),
    };
    let base = base.child(line);

    base.on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, _window, cx| {
        // Double-click resets this split to equal weights (the reference terminal / Zed
        // dock parity). Short-circuit before any drag setup so the user
        // perceives a single "snap to even" action.
        if e.click_count >= 2 {
            let path = split_path.clone();
            main_pane.update(cx, |this: &mut MainPane, cx: &mut Context<MainPane>| {
                this.reset_split_weights(&path, cx);
            });
            cx.stop_propagation();
            return;
        }
        // Single click → start drag. Write directly to MainPane via the
        // captured entity handle; `Context<MainPane>` isn't accessible
        // from a fluent `on_mouse_down` closure (signature is
        // `Fn(&E, &mut Window, &mut App)`).
        let snapshot = ActiveDrag {
            split_path: split_path.clone(),
            gap_idx,
            axis,
            initial_weights: initial_weights.clone(),
            start_position: e.position,
            parent_size_px: parent_size,
        };
        main_pane.update(cx, |this, cx| this.begin_divider_drag(snapshot, cx));
        cx.stop_propagation();
    })
}
