//! Shared armed-divider state for direct mouse-capture resize.
//!
//! Split dividers (workspace groups and within-tab sub-panes) resize via
//! direct mouse capture rather than a drag-and-drop op: a MouseDown on the
//! divider hitbox arms an `ActiveDivider`, a topmost capture overlay's
//! MouseMove recomputes weights, and MouseUp disarms. Capturing the parent
//! split row's bounds at arm-time lets the move handler convert the cursor
//! position into a split fraction without re-deriving layout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{AnyElement, Bounds, IntoElement, Pixels, Point, Size, Styled, canvas};

use crate::shell::pane_tree::Axis;

/// The divider currently being resized by a held mouse button. Lives on the
/// owning entity (`ProjectPanes` for workspace dividers, `PaneGroup` for
/// within-tab sub-pane dividers) only while the button is down.
#[derive(Clone, Debug)]
pub struct ActiveDivider {
    /// Path to the split node in the layout tree.
    pub split_path: Vec<usize>,
    /// Which divider within that split (between `children[i]` / `children[i+1]`).
    pub divider_idx: usize,
    /// Split axis — selects the x or y component for the fraction math.
    pub axis: Axis,
    /// Weights captured at arm-time so each move recomputes from a stable
    /// baseline (no precision drift across many fast moves).
    pub initial_weights: Vec<f32>,
    /// Window-relative origin of the parent split row, captured at arm-time.
    pub container_origin: Point<Pixels>,
    /// Size of the parent split row, captured at arm-time.
    pub container_size: Size<Pixels>,
}

/// Per-render cache of split-row bounds keyed by `split_path`. Each split
/// row paints a zero-cost bounds-recording canvas into this map; the divider
/// MouseDown handler looks the bounds up by path to seed an [`ActiveDivider`].
/// `Rc<RefCell<…>>` so the render closure and the event closure share one map.
pub type DividerBoundsCache = Rc<RefCell<HashMap<Vec<usize>, Bounds<Pixels>>>>;

/// A zero-cost overlay that records its (== the parent split row's,
/// window-relative) bounds into the shared cache each paint, keyed by
/// `split_path`. The divider MouseDown reads it back to seed a mouse-capture
/// resize with the container geometry. Render it as the row's first child
/// (behind the content), absolute + `size_full`, non-occluding so it never
/// intercepts events. The parent row must be `.relative()`.
pub fn split_row_bounds_canvas(cache: DividerBoundsCache, split_path: Vec<usize>) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _: (), _window, _cx| {
            cache.borrow_mut().insert(split_path.clone(), bounds);
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}

/// Compute the split fraction (0.0 = left/top edge, 1.0 = right/bottom edge)
/// for a cursor at window-relative `pos` inside a container described by the
/// armed divider. Returns `None` when the container has zero extent along the
/// axis (degenerate layout — caller should skip the resize).
pub fn fraction_along(active: &ActiveDivider, pos: Point<Pixels>) -> Option<f32> {
    let (axis_pos, axis_size) = match active.axis {
        Axis::Horizontal => (
            f32::from(pos.x - active.container_origin.x),
            f32::from(active.container_size.width),
        ),
        Axis::Vertical => (
            f32::from(pos.y - active.container_origin.y),
            f32::from(active.container_size.height),
        ),
    };
    if axis_size <= 0.0 {
        return None;
    }
    Some((axis_pos / axis_size).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn divider(axis: Axis, origin: (f32, f32), wh: (f32, f32)) -> ActiveDivider {
        ActiveDivider {
            split_path: vec![],
            divider_idx: 0,
            axis,
            initial_weights: vec![1.0, 1.0],
            container_origin: point(px(origin.0), px(origin.1)),
            container_size: size(px(wh.0), px(wh.1)),
        }
    }

    #[test]
    fn horizontal_fraction_is_offset_over_width() {
        let d = divider(Axis::Horizontal, (100.0, 50.0), (200.0, 80.0));
        // Cursor at x = 100 + 50 → 50/200 = 0.25 of the row.
        let frac = fraction_along(&d, point(px(150.0), px(70.0))).unwrap();
        assert!((frac - 0.25).abs() < 1e-4, "got {frac}");
    }

    #[test]
    fn vertical_fraction_uses_y_axis() {
        let d = divider(Axis::Vertical, (0.0, 20.0), (300.0, 100.0));
        // Cursor at y = 20 + 75 → 75/100 = 0.75.
        let frac = fraction_along(&d, point(px(999.0), px(95.0))).unwrap();
        assert!((frac - 0.75).abs() < 1e-4, "got {frac}");
    }

    #[test]
    fn fraction_clamps_to_unit_interval() {
        let d = divider(Axis::Horizontal, (0.0, 0.0), (100.0, 100.0));
        // Past the right edge clamps to 1.0; left of origin clamps to 0.0.
        assert_eq!(fraction_along(&d, point(px(500.0), px(0.0))), Some(1.0));
        assert_eq!(fraction_along(&d, point(px(-50.0), px(0.0))), Some(0.0));
    }

    #[test]
    fn degenerate_zero_extent_returns_none() {
        let d = divider(Axis::Horizontal, (0.0, 0.0), (0.0, 100.0));
        assert_eq!(fraction_along(&d, point(px(10.0), px(10.0))), None);
    }
}
