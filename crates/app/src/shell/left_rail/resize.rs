//! Left-rail drag-resize handle.
//!
//! The handle lives on the RIGHT edge of the rail column. Because the
//! rail's left edge is pinned at window x=0, the new width during a
//! drag is simply the cursor's window-relative x. Width is clamped via
//! `left_rail_layout::clamp_left_rail_width` and persisted on every
//! drag tick (the flat key/value store coalesces writes cheaply).
//!
//! Wiring split mirrors the right-sidebar handle:
//! - The handle (and its `on_drag` start) lives here.
//! - The matching `on_drag_move` listener lives on the workspace
//!   root's outer row, wide enough to keep the cursor inside its
//!   bounds for the full drag.

use gpui::{
    AnyElement, AppContext, Context, ElementId, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::shell::left_rail::LeftRail;

/// Visible width of the drag stripe, in pixels.
const HANDLE_STRIPE_PX: f32 = 1.0;
/// Hit-area padding on each side of the visible stripe (~7px total).
const HANDLE_HIT_PAD_PX: f32 = 3.0;

/// Drag payload for a left-rail resize. The new width derives from the
/// cursor's window x each tick, so no snapshot data is needed.
#[derive(Clone, Debug)]
pub struct LeftRailResizePayload;

/// Zero-sized drag-preview placeholder (the rail reflows live; no
/// floating cursor preview is wanted).
pub struct LeftRailResizeGhost;

impl Render for LeftRailResizeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

/// Build the vertical drag handle for the right edge of the rail.
pub fn build_handle() -> AnyElement {
    let stripe = div().h_full().w(px(HANDLE_STRIPE_PX)).flex_shrink_0();
    div()
        .id(ElementId::Name(SharedString::from("left-rail-resize-handle")))
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h_full()
        .w(px(HANDLE_STRIPE_PX + 2.0 * HANDLE_HIT_PAD_PX))
        .flex_shrink_0()
        .cursor_col_resize()
        .occlude()
        .child(stripe)
        .on_drag(LeftRailResizePayload, |_payload, _offset, _window, cx| {
            cx.new(|_| LeftRailResizeGhost)
        })
        .into_any_element()
}

/// Apply a cursor position (absolute window coords) to the rail width
/// during a drag-move tick. Called from `WorkspaceRoot`'s
/// `on_drag_move::<LeftRailResizePayload>` listener.
///
/// Cursor → width rule: the rail's left edge is at x=0, so the new
/// width equals the cursor's x. Clamp + persist live inside
/// `LeftRail::set_width`.
pub fn apply_drag_move(rail: &mut LeftRail, cursor_x: f32, cx: &mut Context<LeftRail>) {
    rail.set_width(px(cursor_x), cx);
}
