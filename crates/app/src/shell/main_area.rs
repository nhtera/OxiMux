//! Main area — fills remaining horizontal space.
//!
//! Phase 0 ships an empty placeholder with a quiet hint string. Phase 1
//! replaces this with the gpui-component dock that multiplexes terminal,
//! editor, git, and agent panes.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

pub fn view(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_1()
        .h_full()
        .items_center()
        .justify_center()
        .bg(theme.bg_base)
        .child(
            div()
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_subtle)
                .child(format!(
                    "OxiMux shell — dock empty (phase 0 · {}px gutters)",
                    density.pad_panel as i32
                )),
        )
}
