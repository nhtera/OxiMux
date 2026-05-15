//! Status bar — 22px fixed-height bottom strip, three zones.
//!
//! Layout: `left | center | right` (flex 1 each). Phase 0 ships static labels:
//! left = brand+version, center = blank, right = STATUS_MUTED "idle".

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

pub fn view(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    let zone = |child| {
        div()
            .flex()
            .flex_1()
            .items_center()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child(child)
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_status_bar))
        .px(px(density.pad_panel))
        .bg(theme.bg_panel)
        .border_t_1()
        .border_color(theme.border_inactive)
        .child(zone(format!("OxiMux v{}", env!("CARGO_PKG_VERSION"))))
        .child(div().flex().flex_1())
        .child(
            div()
                .flex()
                .flex_1()
                .justify_end()
                .items_center()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.status_muted)
                .child("idle"),
        )
}
