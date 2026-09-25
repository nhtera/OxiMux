//! The phone outline every panel state renders into: a light rim, a thick
//! black bezel, the rounded screen, and the side buttons — sized by height
//! with a fixed aspect ratio so it reads as a device at any panel size. P6's
//! live stream fills the same screen rect.

use gpui::{AnyElement, Div, IntoElement, ParentElement as _, Styled as _, div, px, relative};
use oximux_settings::Theme;

use crate::shell::simulator::widths::PHONE_ASPECT;

/// Outer corner radius of the phone.
const RADIUS: f32 = 56.0;
/// Thickness of the black bezel between the rim and the screen.
const BEZEL: f32 = 12.0;
/// Width of the light rim around the bezel.
const RIM: f32 = 2.0;
/// Side-button thickness, and how far it sits proud of the rim.
const BUTTON_W: f32 = 3.0;

/// A side button: `(side, top as a fraction of the phone's height, height
/// fraction)`. Left: action + volume up/down; right: power.
const BUTTONS: [(Side, f32, f32); 4] =
    [(Side::Left, 0.17, 0.035), (Side::Left, 0.24, 0.065), (Side::Left, 0.32, 0.065), (Side::Right, 0.28, 0.1)];

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

/// The phone, centred in the available space and as tall as it allows, with
/// `screen` inside it.
pub(super) fn phone(theme: Theme, screen: impl IntoElement) -> AnyElement {
    // Physical hardware: the bezel is black in either theme; the rim takes
    // the theme's muted foreground so it catches light on both grounds.
    let rim = theme.fg_muted;
    let mut device = div()
        .relative()
        .h_full()
        .max_w_full()
        .aspect_ratio(PHONE_ASPECT)
        .rounded(px(RADIUS))
        .border(px(RIM))
        .border_color(rim)
        .bg(gpui::black())
        .p(px(BEZEL))
        .child(
            div()
                .size_full()
                .rounded(px(RADIUS - BEZEL - RIM))
                .overflow_hidden()
                .bg(theme.bg_panel)
                .child(screen),
        );
    for (side, top, height) in BUTTONS {
        let button: Div = div()
            .absolute()
            .top(relative(top))
            .h(relative(height))
            .w(px(BUTTON_W))
            .rounded(px(BUTTON_W))
            .bg(rim);
        device = device.child(match side {
            Side::Left => button.left(px(-(RIM + BUTTON_W))),
            Side::Right => button.right(px(-(RIM + BUTTON_W))),
        });
    }
    div()
        .flex()
        .flex_1()
        .min_h(px(0.))
        .w_full()
        .items_center()
        .justify_center()
        .child(device)
        .into_any_element()
}
