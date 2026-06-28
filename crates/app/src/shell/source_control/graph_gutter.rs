//! Canvas renderer for the commit-graph gutter — the coloured DAG lines
//! and the node circle drawn to the left of each commit row.
//!
//! The lane geometry is computed up-front by [`graph_layout`]; this module
//! only paints it. Lines (pass-through lanes, the edges folding into the
//! node, and the edges fanning out below it) are stroked into a single
//! `canvas` via `PathBuilder`; the node circle is layered over the canvas
//! as a rounded `div` so it stays crisp without hand-rolling a filled-disc
//! path. The gutter renders at one fixed width for every row (the page's
//! widest lane count) so the commit text to its right never jitters.
//!
//! Geometry mirrors VS Code's swimlane renderer: 11px lane pitch, the node
//! at the row's vertical midpoint, ~5px curve radius on lane shifts.
//!
//! [`graph_layout`]: super::graph_layout

use gpui::{
    Bounds, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Styled, canvas, div, point, px,
};
use oximux_settings::Theme;

use crate::shell::source_control::graph_layout::RowLayout;

/// Horizontal pitch between lane centres (VS Code `SWIMLANE_WIDTH`).
const LANE_W: f32 = 11.0;
/// Stroke thickness for the lane lines. 1.5px reads crisp on retina
/// without the heavy look a 2px line gives in a dense list.
const LINE_W: f32 = 1.5;
/// Radius of a normal commit's filled node dot.
const NODE_R: f32 = 4.0;
/// Radius of the ring drawn for a merge or HEAD node — a touch larger so
/// the ring is legible around its hollow / accented centre.
const RING_R: f32 = 5.0;

/// Width the gutter occupies for `max_lanes` columns. The trailing
/// lane-width is right-padding that separates the rightmost line from the
/// commit subject.
pub fn gutter_width(max_lanes: usize) -> f32 {
    LANE_W * (max_lanes as f32 + 1.0)
}

/// Lane-centre X within the gutter (local coords). Mirrors VS Code's
/// `11*(col+1)`: lane 0 sits one pitch in from the left edge, leaving room
/// for a half-lane margin and any curve overshoot.
fn lane_x(col: usize) -> f32 {
    LANE_W * (col as f32 + 1.0)
}

/// Build the gutter element for one commit row. `is_head` styles the node
/// as the current checkout; `row_h` is the commit row height so the node
/// lands on its vertical centre.
pub fn graph_gutter(
    row: &RowLayout,
    max_lanes: usize,
    row_h: f32,
    is_head: bool,
    theme: Theme,
) -> impl IntoElement {
    let width = gutter_width(max_lanes);
    let mid = row_h / 2.0;
    let palette = theme.graph_lane_colors;
    let color_of = |idx: u8| palette[(idx as usize) % palette.len()];

    // Owned snapshots for the move-closure (paint runs after render).
    let passthrough = row.passthrough.clone();
    let node_in = row.node_in.clone();
    let node_out = row.node_out.clone();
    let node_lane = row.node_lane;

    let lines = canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _: (), window, _| {
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);
            let node_x = ox + lane_x(node_lane);

            // Pass-through lanes span the full row height; straight when the
            // column is unchanged, a smooth S when it shifts sideways.
            for e in &passthrough {
                smooth_v(
                    window,
                    ox + lane_x(e.top_col),
                    oy,
                    ox + lane_x(e.bottom_col),
                    oy + row_h,
                    palette[(e.color as usize) % palette.len()],
                );
            }
            // Lines folding into the node from above (top → mid).
            for e in &node_in {
                smooth_v(
                    window,
                    ox + lane_x(e.col),
                    oy,
                    node_x,
                    oy + mid,
                    palette[(e.color as usize) % palette.len()],
                );
            }
            // Lines fanning out of the node below (mid → bottom).
            for e in &node_out {
                smooth_v(
                    window,
                    node_x,
                    oy + mid,
                    ox + lane_x(e.col),
                    oy + row_h,
                    palette[(e.color as usize) % palette.len()],
                );
            }
        },
    )
    .absolute()
    .size_full();

    div()
        .relative()
        .flex_shrink_0()
        .w(px(width))
        .h(px(row_h))
        // Clip a pathological row whose lane count exceeds the clamp so a
        // node/line past the fixed gutter width can't paint over the
        // commit subject. Normal repos (1–4 lanes) never reach this.
        .overflow_hidden()
        .child(lines)
        .child(node_dot(
            lane_x(node_lane),
            mid,
            color_of(row.node_color),
            row.is_merge,
            is_head,
            theme,
        ))
}

/// Stroke a smooth vertically-flowing connector from `(x_from, y_from)` to
/// `(x_to, y_to)`. When the columns line up it's a straight vertical;
/// otherwise it's a cubic Bézier whose two control points sit directly
/// above/below the endpoints at the vertical midpoint. That places a
/// vertical tangent at *both* ends, so every segment leaves and re-enters
/// its lane perfectly straight — adjacent rows join seamlessly and lane
/// shifts read as the smooth "S" VS Code's graph draws, with no kink.
fn smooth_v(window: &mut gpui::Window, x_from: f32, y_from: f32, x_to: f32, y_to: f32, color: Hsla) {
    let mut b = PathBuilder::stroke(px(LINE_W));
    b.move_to(point(px(x_from), px(y_from)));
    if (x_from - x_to).abs() < 0.5 {
        b.line_to(point(px(x_to), px(y_to)));
    } else {
        let my = (y_from + y_to) / 2.0;
        // cubic_bezier_to(to, control_a, control_b)
        b.cubic_bezier_to(
            point(px(x_to), px(y_to)),
            point(px(x_from), px(my)),
            point(px(x_to), px(my)),
        );
    }
    paint(window, b, color);
}

/// Build and paint a stroked path, swallowing the rare degenerate-path
/// error (a zero-length segment) so a bad row can't crash the panel.
fn paint(window: &mut gpui::Window, builder: PathBuilder, color: Hsla) {
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// The node circle, absolutely positioned over the canvas at its lane
/// centre. Three looks: a solid dot for a normal commit, a hollow ring for
/// a merge, and an accent-ringed dot for the current checkout (HEAD).
fn node_dot(
    cx: f32,
    mid: f32,
    color: Hsla,
    is_merge: bool,
    is_head: bool,
    theme: Theme,
) -> impl IntoElement {
    // (diameter, fill, optional ring colour)
    let (d, fill, ring) = if is_head {
        (RING_R * 2.0, color, Some(theme.fg_base))
    } else if is_merge {
        (RING_R * 2.0, theme.bg_panel, Some(color))
    } else {
        (NODE_R * 2.0, color, None)
    };
    let mut dot = div()
        .absolute()
        .left(px(cx - d / 2.0))
        .top(px(mid - d / 2.0))
        .w(px(d))
        .h(px(d))
        .rounded_full()
        .bg(fill);
    if let Some(ring) = ring {
        dot = dot.border_2().border_color(ring);
    }
    dot
}
