//! Status bar — 22px fixed-height bottom strip, three zones.
//!
//! Layout: `left | center | right` (flex 1 each). Left zone shows brand +
//! version; right zone shows pane count.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

pub fn view(
    theme: Theme,
    density: Density,
    typography: &Typography,
    pane_count: usize,
) -> impl IntoElement {
    let zone = |child| {
        div()
            .flex()
            .flex_1()
            .items_center()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child(child)
    };

    let pane_label = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
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
                .child(pane_label),
        )
}
