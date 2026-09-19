//! "Scroll to current workspace" against a REAL layout.
//!
//! The rail's scroll container is the workspace-list column, whose direct
//! children are project groups. `ScrollHandle::scroll_to_item` can only
//! address those, so on a group taller than the viewport it lands on the
//! group's top edge and leaves a row near the group's end off screen — which
//! is what the crosshair used to do. The fixture below reproduces that shape
//! (five 400px groups in a 300px viewport, the "active" row last in the last
//! group) and proves both halves: the group-index scroll leaves the row
//! invisible, and the row's own recorded bounds bring it into view.

use gpui::{
    Bounds, Context, InteractiveElement, IntoElement, ParentElement, Pixels, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, TestAppContext, Window, div, px,
};
use oximux_app::shell::left_rail::locate_anchor::{
    LocateAnchor, locate_anchor_canvas, new_anchor, reveal_offset,
};

/// Groups in the fixture list.
const GROUPS: usize = 5;
/// Rows per group. 10 * ROW_HEIGHT = 400px per group — taller than the
/// viewport, which is the case `scroll_to_item` cannot serve.
const ROWS: usize = 10;
const ROW_HEIGHT: f32 = 40.0;
const VIEWPORT_HEIGHT: f32 = 300.0;

struct RailFixture {
    scroll: ScrollHandle,
    anchor: LocateAnchor,
    /// Which (group, row) carries the anchor — i.e. which row is "active".
    active: (usize, usize),
}

impl RailFixture {
    /// Active row last in the last group: the shape `scroll_to_item` fails on.
    fn new() -> Self {
        Self::with_active(GROUPS - 1, ROWS - 1)
    }

    fn with_active(group_ix: usize, row_ix: usize) -> Self {
        Self {
            scroll: ScrollHandle::new(),
            anchor: new_anchor(),
            active: (group_ix, row_ix),
        }
    }
}

impl Render for RailFixture {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Same shape as the rail: the scroll handle is tracked on the column,
        // whose children are groups, whose children are rows.
        let mut col = div()
            .id("list")
            .flex()
            .flex_col()
            .w(px(200.))
            .h(px(VIEWPORT_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        for group_ix in 0..GROUPS {
            let mut group = div().flex().flex_col().w_full();
            for row_ix in 0..ROWS {
                let mut row = div().w_full().h(px(ROW_HEIGHT));
                if (group_ix, row_ix) == self.active {
                    row = row.relative().child(locate_anchor_canvas(self.anchor.clone()));
                }
                group = group.child(row);
            }
            col = col.child(group);
        }
        col
    }
}

/// On-screen top of `row` relative to the viewport's top.
///
/// GPUI records child bounds with the scroll offset already applied, so this
/// is a straight subtraction — adding the handle's offset here would count it
/// twice, which is exactly the mistake `reveal_offset` used to make.
fn on_screen_top(row: Bounds<Pixels>, viewport: Bounds<Pixels>) -> f32 {
    f32::from(row.top() - viewport.top())
}

#[gpui::test]
async fn the_active_row_records_its_own_bounds(cx: &mut TestAppContext) {
    let window = cx.add_window(|_window, _cx| RailFixture::new());
    cx.run_until_parked();
    window
        .update(cx, |view, _window, _cx| {
            let row = view
                .anchor
                .get()
                .expect("the active row records its bounds during layout");
            assert_eq!(f32::from(row.size.height), ROW_HEIGHT);
            // Last row of the last group: 4 groups of 400px, then 9 rows.
            let viewport = view.scroll.bounds();
            let top = f32::from(row.top() - viewport.top());
            assert!(
                (top - 1960.0).abs() < 0.5,
                "row should be laid out at the end of the content, got {top}"
            );
        })
        .expect("window should be alive");
}

#[gpui::test]
async fn scrolling_to_the_group_leaves_the_row_off_screen(cx: &mut TestAppContext) {
    let window = cx.add_window(|_window, _cx| RailFixture::new());
    cx.run_until_parked();
    // What the crosshair used to do: address the active project's GROUP.
    window
        .update(cx, |view, _window, cx| {
            view.scroll.scroll_to_item(GROUPS - 1);
            cx.notify();
        })
        .expect("window should be alive");
    cx.run_until_parked();
    window
        .update(cx, |view, _window, _cx| {
            let row = view.anchor.get().expect("bounds recorded");
            let viewport = view.scroll.bounds();
            let top = on_screen_top(row, viewport);
            assert!(
                top >= VIEWPORT_HEIGHT,
                "the group-index scroll should leave the row below the fold, got {top}"
            );
        })
        .expect("window should be alive");
}

#[gpui::test]
async fn revealing_the_row_puts_it_inside_the_viewport(cx: &mut TestAppContext) {
    let window = cx.add_window(|_window, _cx| RailFixture::new());
    cx.run_until_parked();
    window
        .update(cx, |view, _window, cx| {
            let row = view.anchor.get().expect("bounds recorded");
            let viewport = view.scroll.bounds();
            let offset = view.scroll.offset();
            let target = reveal_offset(
                row,
                viewport,
                f32::from(offset.y),
                f32::from(view.scroll.max_offset().y),
            )
            .expect("a row below the fold must scroll");
            view.scroll.set_offset(gpui::point(offset.x, px(target)));
            cx.notify();
        })
        .expect("window should be alive");
    cx.run_until_parked();
    window
        .update(cx, |view, _window, _cx| {
            let row = view.anchor.get().expect("bounds recorded");
            let viewport = view.scroll.bounds();
            let top = on_screen_top(row, viewport);
            assert!(
                top >= -0.5 && top + ROW_HEIGHT <= VIEWPORT_HEIGHT + 0.5,
                "the row should be fully visible, got top {top}"
            );
            // And a second press is a no-op: nothing left to reveal.
            assert_eq!(
                reveal_offset(
                    row,
                    viewport,
                    f32::from(view.scroll.offset().y),
                    f32::from(view.scroll.max_offset().y)
                ),
                None
            );
        })
        .expect("window should be alive");
}

#[gpui::test]
async fn one_press_reveals_the_row_from_an_already_scrolled_list(cx: &mut TestAppContext) {
    // The case the affordance exists for: the user scrolled away, so the list
    // is NOT at offset 0 when they reach for the crosshair. Because the anchor
    // records on-screen bounds, the correction has to be applied relative to
    // the offset the list already carries. Reading those bounds as unscrolled
    // layout instead made the answer clamp to 0 here — one press flung the
    // list to the very top, with the active row still nowhere in view.
    let window = cx.add_window(|_window, _cx| RailFixture::with_active(2, 5));
    cx.run_until_parked();
    // Park at the bottom extent, with the active row far above the fold.
    window
        .update(cx, |view, _window, cx| {
            let max = view.scroll.max_offset().y;
            view.scroll.set_offset(gpui::point(px(0.), -max));
            cx.notify();
        })
        .expect("window should be alive");
    cx.run_until_parked();
    let before = window
        .update(cx, |view, _window, _cx| {
            on_screen_top(view.anchor.get().expect("bounds recorded"), view.scroll.bounds())
        })
        .expect("window should be alive");
    assert!(before < 0.0, "fixture should start with the row above the fold, got {before}");

    let target = window
        .update(cx, |view, _window, cx| {
            let row = view.anchor.get().expect("bounds recorded");
            let viewport = view.scroll.bounds();
            let offset = view.scroll.offset();
            let target = reveal_offset(
                row,
                viewport,
                f32::from(offset.y),
                f32::from(view.scroll.max_offset().y),
            )
            .expect("a row above the fold must scroll");
            view.scroll.set_offset(gpui::point(offset.x, px(target)));
            cx.notify();
            target
        })
        .expect("window should be alive");
    cx.run_until_parked();
    window
        .update(cx, |view, _window, _cx| {
            let row = view.anchor.get().expect("bounds recorded");
            let top = on_screen_top(row, view.scroll.bounds());
            assert!(
                top >= -0.5 && top + ROW_HEIGHT <= VIEWPORT_HEIGHT + 0.5,
                "one press must reveal the row, got top {top} (scrolled to {target})"
            );
            assert!(
                target < -0.5,
                "the list must not be flung to the top, got {target}"
            );
        })
        .expect("window should be alive");
}
