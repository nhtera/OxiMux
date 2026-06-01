//! Bottom toolbar of the left rail — "Add Project" button + settings icon.
//!
//! Settings cog is still a TODO stub (lands with settings round-trip).

use gpui::{
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, div, px, svg,
};
use gpui_component::tooltip::Tooltip;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::OpenAddProjectDialog;

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
        .child(add_project_button(theme, density, typography))
        .child(div().flex_1())
        .child(settings_icon(theme))
}

fn add_project_button(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .id("left-rail-add-project")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted)
        .hover(|s| s.text_color(theme.fg_base))
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .child("Add Project")
        .tooltip(|window, cx| Tooltip::new("Add a project").build(window, cx))
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(OpenAddProjectDialog), cx);
        })
}

fn settings_icon(theme: Theme) -> impl IntoElement {
    div()
        .id("left-rail-settings")
        .cursor_pointer()
        .tooltip(|window, cx| Tooltip::new("Settings").build(window, cx))
        .child(
            svg()
                .path("icons/settings.svg")
                .size(px(ICON_SIZE))
                .text_color(theme.fg_muted),
        )
}
