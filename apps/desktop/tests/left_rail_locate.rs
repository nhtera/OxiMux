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
}

impl RailFixture {
    fn new() -> Self {
        Self {
            scroll: ScrollHandle::new(),
            anchor: new_anchor(),
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
                // The active row: last row of the last group.
                if group_ix == GROUPS - 1 && row_ix == ROWS - 1 {
                    row = row.relative().child(locate_anchor_canvas(self.anchor.clone()));
                }
                group = group.child(row);
            }
            col = col.child(group);
        }
        col
    }
}

/// On-screen top of `row` relative to the viewport's top. The anchor records
/// the row as painted, so the scroll offset is already in these bounds.
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
