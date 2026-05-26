//! Pure drop-zone resolver for the tab drag-to-split overlay.
//!
//! Given a pane body's `Bounds` and the cursor point inside it, returns
//! one of five `Zone`s: `Center` (merge into target), or `Left` /
//! `Right` / `Up` / `Down` (split target along that edge).
//!
//! Algorithm ported from the reference editor's `useTabDragSplit.ts:142`:
//! a 10% perimeter buffer + 33% horizontal threshold for left/right
//! beats nearest-edge resolution once a workspace has nested splits.

use gpui::{Bounds, Pixels, Point};

/// Drop target inside a pane body, resolved from cursor position.
/// Edge zones request a split on that axis/side; `Center` merges the
/// dragged tab into the target group's strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Center,
    Left,
    Right,
    Up,
    Down,
}

/// Resolve which 5-zone region of `bounds` the `point` lands in.
/// `point` is expected in the same coordinate space as `bounds` (window
/// pixels); the function reduces both to a 0..1 local space internally.
pub fn resolve_drop_zone(bounds: Bounds<Pixels>, point: Point<Pixels>) -> Zone {
    let bx = f32::from(bounds.origin.x);
    let by = f32::from(bounds.origin.y);
    let bw = f32::from(bounds.size.width).max(1.0);
    let bh = f32::from(bounds.size.height).max(1.0);
    let local_x = f32::from(point.x) - bx;
    let local_y = f32::from(point.y) - by;
    let edge_w = bw * 0.1;
    let edge_h = bh * 0.1;
    let split_w_third = bw / 3.0;

    // 10% perimeter buffer carved out as Center, matching the reference.
    if local_x > edge_w && local_x < bw - edge_w && local_y > edge_h && local_y < bh - edge_h {
        return Zone::Center;
    }
    if local_x < split_w_third {
        return Zone::Left;
    }
    if local_x > split_w_third * 2.0 {
        return Zone::Right;
    }
    if local_y < bh / 2.0 {
        Zone::Up
    } else {
        Zone::Down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: gpui::px(0.0),
                y: gpui::px(0.0),
            },
            size: gpui::size(gpui::px(w), gpui::px(h)),
        }
    }

    fn pt(x: f32, y: f32) -> Point<Pixels> {
        Point {
            x: gpui::px(x),
            y: gpui::px(y),
        }
    }

    #[test]
    fn center_at_pane_middle() {
        let r = rect(900.0, 600.0);
        assert_eq!(resolve_drop_zone(r, pt(450.0, 300.0)), Zone::Center);
    }

    #[test]
    fn left_zone_under_third_threshold() {
        let r = rect(900.0, 600.0);
        // local_x = 100 < bw/3 (= 300) AND outside the 10% perimeter band
        // vertically — but 10% horizontal carve is 90px, so local_x=100
        // would still be inside the center vertically. Picking y on the
        // top edge keeps local_y inside the 10% band.
        assert_eq!(resolve_drop_zone(r, pt(100.0, 30.0)), Zone::Left);
    }

    #[test]
    fn right_zone_above_two_thirds_threshold() {
        let r = rect(900.0, 600.0);
        // local_x = 700 > 2/3 * 900 (= 600).
        assert_eq!(resolve_drop_zone(r, pt(700.0, 30.0)), Zone::Right);
    }

    #[test]
    fn up_zone_in_top_middle_band() {
        let r = rect(900.0, 600.0);
        // local_x = 450 sits in the middle third; local_y at the top
        // pushes the point out of Center via the 10% perimeter and into
        // Up (top half of the middle column).
        assert_eq!(resolve_drop_zone(r, pt(450.0, 20.0)), Zone::Up);
    }

    #[test]
    fn down_zone_in_bottom_middle_band() {
        let r = rect(900.0, 600.0);
        assert_eq!(resolve_drop_zone(r, pt(450.0, 580.0)), Zone::Down);
    }

    #[test]
    fn perimeter_corner_resolves_to_horizontal_edge() {
        // Top-left corner: local_x and local_y both inside the 10% band,
        // so Center is skipped. Falls through to the L/R/U/D ladder:
        // local_x < bw/3 → Left wins over Up.
        let r = rect(900.0, 600.0);
        assert_eq!(resolve_drop_zone(r, pt(20.0, 20.0)), Zone::Left);
    }
}
