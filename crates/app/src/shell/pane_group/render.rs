//! Render for `PaneGroup`: per-leaf tab strip + active content area.
//!
//! Layout: a horizontal tab strip at the top + the active tab's content
//! filling the remainder. Each tab carries an icon (file vs terminal),
//! a label, and an "×" close button that appears on hover or when the
//! tab is active. Click a tab to activate; mouse-down on the × closes
//! the tab without activating it.
//!
//! Extracted from `mod.rs` to keep that file focused on data + API.
//! This module reads `PaneGroup` state through `&self` only; all
//! mutations go through `entity.update(cx, ...)` from event handlers.

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, svg,
};

use super::{PaneGroup, PaneGroupTabKind};
use crate::shell::main_pane::PaneContent;

const TAB_STRIP_HEIGHT_PX: f32 = 32.0;
const TAB_PAD_X_PX: f32 = 12.0;
const CLOSE_BUTTON_SIZE_PX: f32 = 14.0;
const ICON_SIZE_PX: f32 = 11.0;

impl Render for PaneGroup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active = self.active();
        let entity = cx.entity().clone();

        // Build the tab strip — one chip per open tab.
        let mut strip = div()
            .id("pane-group-tab-strip")
            .flex()
            .flex_row()
            .items_stretch()
            .h(px(TAB_STRIP_HEIGHT_PX))
            .w_full()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border_inactive)
            .overflow_x_scroll()
            .overflow_y_hidden();

        for (idx, tab) in self.tabs().iter().enumerate() {
            strip = strip.child(render_tab(
                idx,
                tab.label.clone(),
                &tab.kind,
                idx == active,
                theme,
                entity.clone(),
            ));
        }

        // Active content area.
        let active_content: Option<AnyElement> = self.active_tab().map(|tab| match &tab.content {
            PaneContent::Terminal(view) => view.clone().into_any_element(),
            PaneContent::Editor(view) => view.clone().into_any_element(),
        });

        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_hidden()
            .when_some(active_content, |s, child| s.child(child));

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(strip)
            .child(body)
    }
}

fn render_tab(
    ix: usize,
    label: SharedString,
    kind: &PaneGroupTabKind,
    is_active: bool,
    theme: oximux_settings::Theme,
    entity: Entity<PaneGroup>,
) -> impl IntoElement {
    let icon_path = match kind {
        PaneGroupTabKind::Editor { .. } => "icons/file.svg",
        PaneGroupTabKind::Terminal | PaneGroupTabKind::Agent { .. } => "icons/square-terminal.svg",
    };
    let icon_color = if is_active {
        theme.fg_muted
    } else {
        theme.fg_subtle
    };
    let text_color = if is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    let top_accent = if is_active {
        theme.focus_ring
    } else {
        gpui::transparent_black()
    };

    let group_name = SharedString::from(format!("pane-group-tab-{ix}"));
    let activate_entity = entity.clone();

    div()
        .id(SharedString::from(format!("pane-group-tab-{ix}")))
        .group(group_name.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h_full()
        .px(px(TAB_PAD_X_PX))
        .border_t_2()
        .border_color(top_accent)
        .text_size(px(11.0))
        .text_color(text_color)
        .flex_shrink_0()
        .cursor_pointer()
        .when(!is_active, |s| {
            s.hover(|s| s.text_color(theme.fg_base).bg(theme.bg_panel_alt))
        })
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = activate_entity.clone();
            entity.update(cx, |this, cx| this.set_active(ix, window, cx));
        })
        .child(svg().path(icon_path).size(px(ICON_SIZE_PX)).text_color(icon_color))
        .child(div().child(label))
        .child(close_button(ix, is_active, entity, group_name, theme))
}

fn close_button(
    ix: usize,
    is_active: bool,
    entity: Entity<PaneGroup>,
    group_name: SharedString,
    theme: oximux_settings::Theme,
) -> impl IntoElement {
    let glyph = svg()
        .path("icons/close.svg")
        .size(px(9.0))
        .text_color(theme.fg_muted);
    // Hide the X by default; show on tab hover OR when the tab is
    // active. Matches industry-standard editor convention.
    let initial_opacity = if is_active { 1.0 } else { 0.0 };
    div()
        .id(SharedString::from(format!("pane-group-tab-close-{ix}")))
        .w(px(CLOSE_BUTTON_SIZE_PX))
        .h(px(CLOSE_BUTTON_SIZE_PX))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .opacity(initial_opacity)
        .group_hover(group_name, |s| s.opacity(1.0))
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = entity.clone();
            entity.update(cx, |this, cx| this.close_tab(ix, window, cx));
            cx.stop_propagation();
        })
        .child(glyph)
}
