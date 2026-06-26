//! Compact project -> workspace -> agent flow strip for the welcome view.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

const FLOW_DOT_SIZE: f32 = 6.0;
const FLOW_LABELS: &[&str] = &["Project", "Workspace", "Agent", "Review"];

pub fn flow_strip(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(density.gap_inline))
        .pt(px(2.0));

    for (index, label) in FLOW_LABELS.iter().enumerate() {
        if index > 0 {
            row = row.child(div().w(px(24.0)).h(px(1.0)).bg(theme.border_inactive));
        }
        row = row.child(flow_node(label, theme, density, typography));
    }
    row
}

fn flow_node(
    label: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0))
        .h(px(22.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_panel)
        .child(
            div()
                .size(px(FLOW_DOT_SIZE))
                .rounded_full()
                .bg(theme.status_info),
        )
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(label),
        )
}
