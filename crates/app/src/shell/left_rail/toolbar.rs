//! Bottom toolbar of the left rail — "Add Project" button + settings icon.
//!
//! All actions are TODO stubs in Phase 02. Phase 04 (settings) wires the
//! settings cog; new-workspace flow lands in a future phase.

use gpui::{IntoElement, ParentElement, Styled, div, px, svg};
use oximux_settings::{Density, Theme, Typography};

const TOOLBAR_HEIGHT: f32 = 36.0;
const ICON_SIZE: f32 = 14.0;

pub fn render_toolbar(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(TOOLBAR_HEIGHT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .border_t_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel)
        // TODO(phase-04): open the new-workspace modal on click.
        .child(add_project_button(theme, density, typography))
        // TODO(phase-04): open the settings panel on click.
        .child(div().flex_1())
        .child(settings_icon(theme))
}

fn add_project_button(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted)
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .child("Add Project")
}

fn settings_icon(theme: Theme) -> impl IntoElement {
    div().cursor_pointer().child(
        svg()
            .path("icons/settings.svg")
            .size(px(ICON_SIZE))
            .text_color(theme.fg_muted),
    )
}
