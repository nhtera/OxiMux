//! Top bar — 40px horizontal strip across the window head.
//!
//! chrome: 56px gutter (traffic-light inset on macOS), left-rail
//! toggle, centered wordmark, right-sidebar toggle. Click handlers dispatch
//! `ToggleLeftSidebar` / `ToggleRightSidebar` actions via `window.dispatch_action`
//! since this is a pure render function with no owning entity handle.

use gpui::{
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled, Window,
    div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{ToggleLeftSidebar, ToggleRightSidebar};

/// Width reserved on the left for macOS traffic lights (12px inset +
/// 3 buttons × 12px + 8px breathing room).
const TRAFFIC_LIGHT_GUTTER: f32 = 56.0;

/// Each toggle icon's hit target.
const TOGGLE_BUTTON_WIDTH: f32 = 36.0;

/// Toggle icon glyph size.
const ICON_SIZE: f32 = 16.0;

pub fn view(
    left_open: bool,
    right_open: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_top_bar))
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(left_zone(left_open, theme))
        .child(center_zone(theme, typography))
        .child(right_zone(right_open, theme))
}

fn left_zone(left_open: bool, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .child(div().w(px(TRAFFIC_LIGHT_GUTTER)))
        .child(toggle_button(
            left_toggle_icon(left_open),
            theme,
            ToggleSide::Left,
        ))
}

fn center_zone(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_size(px(typography.t_brand))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child("OxiMux")
}

fn right_zone(right_open: bool, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .child(toggle_button(
            right_toggle_icon(right_open),
            theme,
            ToggleSide::Right,
        ))
}

#[derive(Clone, Copy)]
enum ToggleSide {
    Left,
    Right,
}

fn toggle_button(icon_path: &'static str, theme: Theme, side: ToggleSide) -> impl IntoElement {
    let glyph = svg()
        .path(icon_path)
        .size(px(ICON_SIZE))
        .text_color(theme.fg_muted);

    div()
        .w(px(TOGGLE_BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut gpui::App| match side {
                ToggleSide::Left => {
                    window.dispatch_action(Box::new(ToggleLeftSidebar), cx);
                }
                ToggleSide::Right => {
                    window.dispatch_action(Box::new(ToggleRightSidebar), cx);
                }
            },
        )
        .child(glyph)
}

/// Lucide `PanelLeftClose` when open (click collapses left); `PanelLeftOpen`
/// when collapsed (click expands left). Both ship in gpui-component bundle.
pub(crate) fn left_toggle_icon(left_open: bool) -> &'static str {
    if left_open {
        "icons/panel-left-close.svg"
    } else {
        "icons/panel-left-open.svg"
    }
}

/// Mirror of `left_toggle_icon` for the right edge.
pub(crate) fn right_toggle_icon(right_open: bool) -> &'static str {
    if right_open {
        "icons/panel-right-close.svg"
    } else {
        "icons/panel-right-open.svg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_open_uses_close_icon() {
        assert_eq!(left_toggle_icon(true), "icons/panel-left-close.svg");
    }

    #[test]
    fn left_closed_uses_open_icon() {
        assert_eq!(left_toggle_icon(false), "icons/panel-left-open.svg");
    }

    #[test]
    fn right_open_uses_close_icon() {
        assert_eq!(right_toggle_icon(true), "icons/panel-right-close.svg");
    }

    #[test]
    fn right_closed_uses_open_icon() {
        assert_eq!(right_toggle_icon(false), "icons/panel-right-open.svg");
    }
}
