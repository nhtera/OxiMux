//! Vector emission for box-drawing glyphs. Replaces the font's glyph with a
//! `PathBuilder` stroke so multi-cell spans render gap-free and corners snap
//! to integer pixels. Adapted from `gpui-terminal/src/box_drawing.rs` (the
//! standout reference for this technique).
//!
//! ## Drift / gap notes
//!
//! - Thickness is rounded to integer pixels; sub-pixel widths on HiDPI
//!   read fuzzy on horizontal/vertical runs.
//! - Each cell paints a 1-px overlap past its boundary so adjacent same-
//!   weight cells visually fuse without a hairline seam at the join.
//! - The merge pass (`draw_horizontal_span`) emits ONE path across many
//!   cells when they share weight + color, eliminating the per-cell joint
//!   entirely. Standalone cells use the overlap fallback.

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Window, point, px};

use super::segments::{BoxSegments, LineWeight, get_box_segments, is_rounded_corner};

/// Light and heavy stroke widths derived from the cell width. Round to
/// integer pixels — sub-pixel thicknesses read fuzzy on horizontal runs.
fn calculate_thickness(cell_width: Pixels) -> (Pixels, Pixels) {
    let w: f32 = cell_width.into();
    let light = (w * 0.15).round().max(1.0);
    let heavy = (w * 0.28).round().max(2.0);
    (px(light), px(heavy))
}

fn get_thickness(weight: LineWeight, light: Pixels, heavy: Pixels) -> Pixels {
    match weight {
        LineWeight::Light => light,
        LineWeight::Heavy => heavy,
        // Double draws two parallel light strokes; thickness picks the
        // light value per stroke.
        LineWeight::Double => light,
    }
}

/// Paint one box-drawing character into `bounds` in `color`. Returns
/// `false` if `ch` isn't a recognised code point so the caller can fall
/// back to the font glyph.
pub fn draw_box_character(
    ch: char,
    bounds: Bounds<Pixels>,
    color: Hsla,
    cell_width: Pixels,
    window: &mut Window,
) -> bool {
    let Some(segments) = get_box_segments(ch) else {
        return false;
    };

    let cx = bounds.origin.x + bounds.size.width / 2.0;
    let cy = bounds.origin.y + bounds.size.height / 2.0;
    let (light, heavy) = calculate_thickness(cell_width);

    let left = bounds.origin.x;
    let right = bounds.origin.x + bounds.size.width;
    let top = bounds.origin.y;
    let bottom = bounds.origin.y + bounds.size.height;

    if is_rounded_corner(ch) {
        draw_rounded_corner(ch, bounds, cx, cy, light, color, window);
        return true;
    }

    // 1-px overlap past cell edges so adjacent same-weight cells fuse with
    // no visible hairline at the boundary. The merge pass eliminates this
    // for horizontal runs of equal weight + color; this is the fallback.
    let overlap = px(1.0);

    paint_horizontal(
        segments, cx, cy, left, right, light, heavy, overlap, color, window,
    );
    paint_vertical(
        segments, cx, cy, top, bottom, light, heavy, overlap, color, window,
    );

    true
}

/// Paint ONLY the vertical components of `ch`. Use after the horizontal
/// merge pass has drawn the left/right strokes — avoids re-emitting the
/// horizontal at a junction (which would visually fatten the line).
pub fn draw_vertical_components(
    ch: char,
    bounds: Bounds<Pixels>,
    color: Hsla,
    cell_width: Pixels,
    window: &mut Window,
) {
    let Some(segments) = get_box_segments(ch) else {
        return;
    };
    if segments.top.is_none() && segments.bottom.is_none() {
        return;
    }

    let cx = bounds.origin.x + bounds.size.width / 2.0;
    let cy = bounds.origin.y + bounds.size.height / 2.0;
    let (light, heavy) = calculate_thickness(cell_width);

    let top = bounds.origin.y;
    let bottom = bounds.origin.y + bounds.size.height;

    paint_vertical(
        segments,
        cx,
        cy,
        top,
        bottom,
        light,
        heavy,
        px(0.0),
        color,
        window,
    );
}

/// Paint a continuous horizontal stroke spanning the cells from `start_x`
/// to `end_x` at vertical center `cy`. ONE path across the whole span —
/// the seam-free fix for box-drawing runs.
pub fn draw_horizontal_span(
    start_x: Pixels,
    end_x: Pixels,
    cy: Pixels,
    weight: LineWeight,
    cell_width: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let (light, heavy) = calculate_thickness(cell_width);
    let thickness = get_thickness(weight, light, heavy);
    draw_continuous_line(
        point(start_x, cy),
        point(end_x, cy),
        weight,
        thickness,
        true,
        color,
        window,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_horizontal(
    s: BoxSegments,
    cx: Pixels,
    cy: Pixels,
    left: Pixels,
    right: Pixels,
    light: Pixels,
    heavy: Pixels,
    overlap: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    // If both halves share the same weight, draw ONE continuous stroke
    // from edge to edge so the cross-point at the cell center isn't
    // broken. Different weights need separate draws.
    let common = match (s.left, s.right) {
        (Some(l), Some(r)) if l == r => Some(l),
        (Some(l), None) => Some(l),
        (None, Some(r)) => Some(r),
        _ => None,
    };
    if let Some(weight) = common {
        let thickness = get_thickness(weight, light, heavy);
        let start_x = if s.left.is_some() { left - overlap } else { cx };
        let end_x = if s.right.is_some() {
            right + overlap
        } else {
            cx
        };
        draw_continuous_line(
            point(start_x, cy),
            point(end_x, cy),
            weight,
            thickness,
            true,
            color,
            window,
        );
        return;
    }
    if let Some(weight) = s.left {
        let thickness = get_thickness(weight, light, heavy);
        draw_continuous_line(
            point(left - overlap, cy),
            point(cx, cy),
            weight,
            thickness,
            true,
            color,
            window,
        );
    }
    if let Some(weight) = s.right {
        let thickness = get_thickness(weight, light, heavy);
        draw_continuous_line(
            point(cx, cy),
            point(right + overlap, cy),
            weight,
            thickness,
            true,
            color,
            window,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_vertical(
    s: BoxSegments,
    cx: Pixels,
    cy: Pixels,
    top: Pixels,
    bottom: Pixels,
    light: Pixels,
    heavy: Pixels,
    overlap: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let common = match (s.top, s.bottom) {
        (Some(t), Some(b)) if t == b => Some(t),
        (Some(t), None) => Some(t),
        (None, Some(b)) => Some(b),
        _ => None,
    };
    if let Some(weight) = common {
        let thickness = get_thickness(weight, light, heavy);
        let start_y = if s.top.is_some() { top - overlap } else { cy };
        let end_y = if s.bottom.is_some() {
            bottom + overlap
        } else {
            cy
        };
        draw_continuous_line(
            point(cx, start_y),
            point(cx, end_y),
            weight,
            thickness,
            false,
            color,
            window,
        );
        return;
    }
    if let Some(weight) = s.top {
        let thickness = get_thickness(weight, light, heavy);
        draw_continuous_line(
            point(cx, top - overlap),
            point(cx, cy),
            weight,
            thickness,
            false,
            color,
            window,
        );
    }
    if let Some(weight) = s.bottom {
        let thickness = get_thickness(weight, light, heavy);
        draw_continuous_line(
            point(cx, cy),
            point(cx, bottom + overlap),
            weight,
            thickness,
            false,
            color,
            window,
        );
    }
}

fn draw_continuous_line(
    from: Point<Pixels>,
    to: Point<Pixels>,
    weight: LineWeight,
    thickness: Pixels,
    is_horizontal: bool,
    color: Hsla,
    window: &mut Window,
) {
    match weight {
        LineWeight::Light | LineWeight::Heavy => {
            draw_line(from, to, thickness, color, window);
        }
        LineWeight::Double => {
            // Two parallel strokes offset by `thickness` on each side of
            // the centerline — gives the canonical ═/║/╬ rail look.
            let gap = thickness;
            let offset = (thickness + gap) / 2.0;
            if is_horizontal {
                draw_line(
                    point(from.x, from.y - offset),
                    point(to.x, to.y - offset),
                    thickness,
                    color,
                    window,
                );
                draw_line(
                    point(from.x, from.y + offset),
                    point(to.x, to.y + offset),
                    thickness,
                    color,
                    window,
                );
            } else {
                draw_line(
                    point(from.x - offset, from.y),
                    point(to.x - offset, to.y),
                    thickness,
                    color,
                    window,
                );
                draw_line(
                    point(from.x + offset, from.y),
                    point(to.x + offset, to.y),
                    thickness,
                    color,
                    window,
                );
            }
        }
    }
}

fn draw_line(
    from: Point<Pixels>,
    to: Point<Pixels>,
    thickness: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut builder = PathBuilder::stroke(thickness);
    builder.move_to(from);
    builder.line_to(to);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn draw_rounded_corner(
    ch: char,
    bounds: Bounds<Pixels>,
    cx: Pixels,
    cy: Pixels,
    thickness: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let overlap = px(1.0);
    let left = bounds.origin.x - overlap;
    let right = bounds.origin.x + bounds.size.width + overlap;
    let top = bounds.origin.y - overlap;
    let bottom = bounds.origin.y + bounds.size.height + overlap;

    // Radius scales with cell size — 80 % of the half-cell gives a curve
    // that visually reads as a rounded corner without being so tight it
    // looks like a right angle.
    let radius_x = bounds.size.width / 2.0 * 0.8;
    let radius_y = bounds.size.height / 2.0 * 0.8;

    let mut b = PathBuilder::stroke(thickness);
    match ch {
        // ╭ — from bottom edge, curve into right edge.
        '\u{256D}' => {
            b.move_to(point(cx, bottom));
            b.line_to(point(cx, cy + radius_y));
            b.curve_to(point(cx + radius_x, cy), point(cx, cy));
            b.line_to(point(right, cy));
        }
        // ╮ — from left edge, curve into bottom edge.
        '\u{256E}' => {
            b.move_to(point(left, cy));
            b.line_to(point(cx - radius_x, cy));
            b.curve_to(point(cx, cy + radius_y), point(cx, cy));
            b.line_to(point(cx, bottom));
        }
        // ╯ — from top edge, curve into left edge.
        '\u{256F}' => {
            b.move_to(point(cx, top));
            b.line_to(point(cx, cy - radius_y));
            b.curve_to(point(cx - radius_x, cy), point(cx, cy));
            b.line_to(point(left, cy));
        }
        // ╰ — from right edge, curve into top edge.
        '\u{2570}' => {
            b.move_to(point(right, cy));
            b.line_to(point(cx + radius_x, cy));
            b.curve_to(point(cx, cy - radius_y), point(cx, cy));
            b.line_to(point(cx, top));
        }
        _ => return,
    }
    if let Ok(path) = b.build() {
        window.paint_path(path, color);
    }
}
