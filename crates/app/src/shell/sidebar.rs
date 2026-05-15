//! Sidebar — fixed-width (240px) left column.
//!
//! Phase 0 ships only a header label. Phase 1 wires gpui-component's
//! virtualized list for workspaces / panes.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

pub fn view(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .h_full()
        .w(px(density.w_sidebar))
        .bg(theme.bg_panel)
        .border_r_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .flex()
                .items_center()
                .h(px(density.h_tab))
                .px(px(density.pad_panel))
                .text_size(px(typography.t_label_caps))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .child("WORKSPACES"),
        )
}
