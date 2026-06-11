//! Right-sidebar drag-resize handle (Phase 13).
//!
//! The handle lives on the LEFT edge of the panel column. Dragging
//! it towards the main pane (cursor moves left) GROWS the sidebar;
//! dragging it back towards the right edge SHRINKS it. The width is
//! clamped via `scm_layout_settings::clamp_panel_width` and persisted
//! to the global `SettingsRepo` on every drag tick — the repo's flat
//! key/value store coalesces writes cheaply enough that throttling
//! isn't worth the complexity.
//!
//! Wiring split:
//! - The handle (and its `on_drag` start) lives here.
//! - The matching `on_drag_move` listener lives on the workspace
//!   root's outer row, so the cursor remains inside the listener's
//!   bounds for the full drag (a handle-local `on_drag_move` would
//!   stop firing the instant the cursor leaves its 7px-wide hitbox).

use gpui::{
    AnyElement, AppContext, Context, ElementId, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::scm_layout_settings;
use crate::shell::right_sidebar::RightSidebar;

/// Visible width of the drag stripe, in pixels. Matches the divider
/// chrome used elsewhere in the workspace (`pane_group` dividers).
const HANDLE_STRIPE_PX: f32 = 1.0;

/// Hit-area padding on each side of the visible stripe. Total
/// clickable width = `HANDLE_STRIPE_PX + 2 * HANDLE_HIT_PAD_PX` ≈ 7px.
const HANDLE_HIT_PAD_PX: f32 = 3.0;

/// Width of the hover/drag highlight bar, flush with the sidebar edge.
/// Wider than the resting hairline so the grabbable edge is obvious.
const HANDLE_HOVER_BAR_PX: f32 = 3.0;

/// Drag payload for a sidebar resize. Carries a fallback window
/// width captured at drag-start; the move handler prefers the live
/// `DragMoveEvent::bounds.size.width` and only reads this when the
/// live bounds are zero (rare pre-layout frame). The actual width
/// derives per-tick from the cursor position; no need to snapshot
/// it here.
#[derive(Clone, Debug)]
pub struct SidebarResizePayload {
    /// Window width in pixels at the moment the drag began. Used as
    /// a cold-frame fallback inside the move handler when the
    /// listener-div's live bounds report a zero width — typical
    /// boot-race defense, not the hot path.
    pub window_width: f32,
}

/// Zero-sized placeholder for the drag visual. GPUI requires `on_drag`
/// to return an Entity that renders the floating cursor preview; for
/// an edge resize-handle there's no preview — the sidebar itself
/// reflows on every `on_drag_move` tick.
pub struct SidebarResizeGhost;

impl Render for SidebarResizeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(0.0)).h(px(0.0))
    }
}

/// Build the vertical drag handle for the left edge of the sidebar.
/// The hairline stripe IS the sidebar's left border (flush against the
/// edge, `border_inactive`, the sidebar root draws no border of its
/// own). Hovering the `HANDLE_STRIPE_PX + 2 * HANDLE_HIT_PAD_PX`
/// hitbox paints a wider `border_active` bar over it — an absolute
/// overlay so the highlight widens without any layout shift, and the
/// edge never reads as two offset lines.
pub fn build_handle(window_width: f32, theme: oximux_settings::Theme) -> AnyElement {
    let stripe = div()
        .h_full()
        .w(px(HANDLE_STRIPE_PX))
        .flex_shrink_0()
        .bg(theme.border_inactive);
    let hover_bar = div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .w(px(HANDLE_HOVER_BAR_PX))
        .group_hover("right-sidebar-resize", move |s| s.bg(theme.border_active));
    div()
        .id(ElementId::Name(SharedString::from(
            "right-sidebar-resize-handle",
        )))
        .group("right-sidebar-resize")
        .flex()
        .flex_row()
        .items_center()
        .justify_start()
        .h_full()
        .w(px(HANDLE_STRIPE_PX + 2.0 * HANDLE_HIT_PAD_PX))
        .flex_shrink_0()
        .cursor_col_resize()
        .occlude()
        .child(stripe)
        .child(hover_bar)
        .on_drag(
            SidebarResizePayload { window_width },
            |_payload, _offset, _window, cx| cx.new(|_| SidebarResizeGhost),
        )
        .into_any_element()
}

/// Apply a cursor position (absolute window coords) to the sidebar
/// width during a drag-move tick. Called from `WorkspaceRoot`'s
/// `on_drag_move::<SidebarResizePayload>` listener.
///
/// Cursor → width rule: new width = window_width − cursor.x. This
/// makes the cursor stay glued to the handle (rather than drifting
/// off as the panel grows from the LEFT edge it owns).
pub fn apply_drag_move(
    sidebar: &mut RightSidebar,
    cursor_x: f32,
    window_width: f32,
    cx: &mut Context<RightSidebar>,
) {
    let candidate = window_width - cursor_x;
    let clamped = scm_layout_settings::clamp_panel_width(candidate, window_width);
    sidebar.set_panel_width(px(clamped), cx);
}

/// Width in pixels of the full hitbox (visible stripe + padding on
/// each side). Exported for layout math in callers that need to
/// subtract the handle's footprint from a width budget.
#[allow(dead_code)]
pub const HANDLE_HITBOX_WIDTH: Pixels = px(HANDLE_STRIPE_PX + 2.0 * HANDLE_HIT_PAD_PX);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_hitbox_width_constant_matches_padding_math() {
        // Belt-and-braces: if HANDLE_STRIPE_PX or HANDLE_HIT_PAD_PX
        // ever drift, the const won't.
        let expected = HANDLE_STRIPE_PX + 2.0 * HANDLE_HIT_PAD_PX;
        assert_eq!(f32::from(HANDLE_HITBOX_WIDTH), expected);
    }
}
