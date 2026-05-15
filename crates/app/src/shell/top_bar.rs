//! Top bar — fixed-height (36px) horizontal strip across the window head.
//!
//! Phase 0 content:
//! - Brand wordmark "OxiMux" (left)
//! - Empty middle (workspace switcher lands in Phase 4)
//! - Empty right (window controls live in the OS title bar in v1)

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

pub fn view(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_top_bar))
        .px(px(density.pad_panel))
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .flex()
                .items_center()
                .text_size(px(typography.t_brand))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child("OxiMux"),
        )
}
