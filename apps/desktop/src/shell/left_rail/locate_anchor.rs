//! Where the active workspace row actually sits, and the scroll offset that
//! reveals it.
//!
//! The rail's scroll container is the workspace-list COLUMN, whose direct
//! children are project groups (or, in flat mode, workspace blocks). GPUI's
//! `ScrollHandle::scroll_to_item` only understands those direct children, so
//! it can bring a project group into view without bringing the active row
//! inside it anywhere near the viewport — a tall group scrolls to its header
//! and leaves the active card far below the fold. The active row therefore
//! records its own laid-out bounds each frame through a zero-cost canvas, and
//! the locate affordance scrolls to those bounds directly.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{AnyElement, Bounds, IntoElement, Pixels, Styled, canvas};

/// Sub-pixel slack: below this the row counts as already where we want it, and
/// a correcting scroll would be churn nobody can see.
const EPSILON: f32 = 0.5;

/// Shared slot holding the active workspace row's bounds from the last layout
/// pass. GPUI records child bounds with the scroll offset ALREADY APPLIED, so
/// these are on-screen bounds: `row.top() - viewport.top()` is the row's
/// distance below the viewport's top edge as painted, and it already moves
/// when the list scrolls. Adding the handle's offset to it double-counts.
///
/// `Rc<Cell<…>>` so the render tree and the rail's event handlers share one
/// slot. `None` means the active row was not laid out in the last frame — no
/// active workspace, its project group collapsed, or the agents page occupying
/// the rail body instead of the list.
pub type LocateAnchor = Rc<Cell<Option<Bounds<Pixels>>>>;

/// A fresh, empty anchor slot.
pub fn new_anchor() -> LocateAnchor {
    Rc::new(Cell::new(None))
}

/// Zero-cost overlay that records its host's bounds into `anchor` on every
/// prepaint. Render it as a child of the active row, absolute + `size_full`;
/// it paints nothing and registers no hitbox, so it never intercepts events.
/// The host must be positioned (`relative`) for `size_full` to mean the row.
pub fn locate_anchor_canvas(anchor: LocateAnchor) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _: (), _window, _cx| anchor.set(Some(bounds)),
    )
    .absolute()
    .size_full()
    .into_any_element()
}

/// The vertical scroll offset that reveals `row` inside `viewport`, or `None`
/// when no scroll is warranted.
///
/// `current_y` is the handle's current offset and `max_offset_y` its scroll
/// extent, so the valid range is `[-max_offset_y, 0]` (more negative = further
/// down the list). `row` is the row's ON-SCREEN bounds as recorded by
/// [`locate_anchor_canvas`], so `row.top() - viewport.top()` is already where
/// the row sits in the viewport at `current_y`; the returned offset is
/// therefore `current_y` plus the correction needed, not an absolute position
/// derived from unscrolled layout.
///
/// A row already fully in view is left alone: the locate glow is the feedback
/// there, and a jump under a card the user is already looking at is worse than
/// no motion at all. Otherwise the row is centred, which lands it where the
/// eye goes first; a row taller than the viewport is top-aligned instead,
/// because centring one hides its card behind the top edge.
pub fn reveal_offset(
    row: Bounds<Pixels>,
    viewport: Bounds<Pixels>,
    current_y: f32,
    max_offset_y: f32,
) -> Option<f32> {
    let view_h = f32::from(viewport.size.height);
    // Never laid out (or collapsed to nothing): there is no viewport to
    // reveal anything in, and dividing by it would be meaningless.
    if view_h <= 0.0 {
        return None;
    }
    // Already on-screen-relative: the scroll offset is baked into these
    // bounds, so this is where the row sits right now.
    let visible_top = f32::from(row.top() - viewport.top());
    let row_h = f32::from(row.size.height);
    let fits = row_h <= view_h;
    if fits && visible_top >= -EPSILON && visible_top + row_h <= view_h + EPSILON {
        return None;
    }
    // How far the row must travel from where it is, applied on top of the
    // offset that put it there.
    let target = current_y
        + if fits {
            (view_h - row_h) / 2.0 - visible_top
        } else {
            -visible_top
        };
    // GPUI clamps the offset itself on the next prepaint; clamping here keeps
    // the "already there" comparison below honest at either extent.
    let target = target.clamp(-max_offset_y, 0.0);
    ((target - current_y).abs() >= EPSILON).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    /// Viewport 300px tall starting at y=100, matching a rail list that is not
    /// the first thing in the window.
    fn viewport() -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(100.)), size(px(200.), px(300.)))
    }

    /// `top` is an ON-SCREEN position: the canvas records bounds with the
    /// scroll offset already applied, so a row painted 900px below the
    /// viewport's top edge has `top = viewport.top() + 900` whatever the
    /// handle's offset happens to be.
    fn row(top: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(top)), size(px(200.), px(height)))
    }

    #[test]
    fn a_row_already_in_view_is_left_where_it_is() {
        // 150..190 on screen with no scroll — squarely inside 100..400.
        assert_eq!(reveal_offset(row(150., 40.), viewport(), 0.0, 2000.0), None);
    }

    #[test]
    fn a_row_below_the_fold_is_centred() {
        // Row at content y=1000 (900 past the viewport top), 40px tall.
        // Centring puts it at (300-40)/2 = 130 from the viewport top, so the
        // offset is 130 - 900 = -770.
        let got = reveal_offset(row(1000., 40.), viewport(), 0.0, 2000.0).expect("scrolls");
        assert!((got - -770.0).abs() < 0.01, "got {got}");
    }

    #[test]
    fn a_row_above_the_fold_is_pulled_back_down() {
        // Painted 500px ABOVE the viewport's top edge while the list sits at
        // -1200. Centring must move it down 630px (to +130), which means an
        // offset of -1200 + 630 = -570 — a correction applied to where the
        // list already is, not an absolute derived from layout.
        let got = reveal_offset(row(-400., 40.), viewport(), -1200.0, 4000.0).expect("scrolls");
        assert!((got - -570.0).abs() < 0.01, "got {got}");
    }

    #[test]
    fn the_correction_is_relative_to_the_current_offset() {
        // The same row, on screen in the same place, from two different
        // offsets: each must land the row centred, so the answers differ by
        // exactly the difference in starting offset. This is the property the
        // original "unscrolled bounds" reading got wrong — it returned the
        // same absolute offset for both and so under-scrolled by `current_y`.
        let a = reveal_offset(row(1000., 40.), viewport(), -100.0, 4000.0).expect("scrolls");
        let b = reveal_offset(row(1000., 40.), viewport(), -900.0, 4000.0).expect("scrolls");
        assert!((a - b - 800.0).abs() < 0.01, "a={a} b={b}");
    }

    #[test]
    fn the_offset_never_leaves_the_scrollable_range() {
        // A row near the very end of a short list would centre past the
        // bottom extent; the clamp keeps the list from scrolling into blank.
        let got = reveal_offset(row(1090., 40.), viewport(), 0.0, 800.0).expect("scrolls");
        assert!((got - -800.0).abs() < 0.01, "got {got}");
        // And near the start it can never go positive (blank above the list):
        // a row painted 80px above the top edge at offset -100 sits only 20px
        // into the content, so centring it would want a positive offset.
        let got = reveal_offset(row(20., 40.), viewport(), -100.0, 800.0).expect("scrolls");
        assert!((got - 0.0).abs() < 0.01, "got {got}");
    }

    #[test]
    fn a_row_taller_than_the_viewport_is_top_aligned() {
        // A multi-agent block 500px tall can never fit in a 300px viewport;
        // centring would hide its card above the top edge, so align the top.
        let got = reveal_offset(row(700., 500.), viewport(), 0.0, 2000.0).expect("scrolls");
        assert!((got - -600.0).abs() < 0.01, "got {got}");
    }

    #[test]
    fn a_viewport_that_has_never_been_laid_out_scrolls_nothing() {
        let unlaid = Bounds::new(point(px(0.), px(0.)), size(px(0.), px(0.)));
        assert_eq!(reveal_offset(row(500., 40.), unlaid, 0.0, 2000.0), None);
    }
}
