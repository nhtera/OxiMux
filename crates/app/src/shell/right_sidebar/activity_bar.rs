//! Activity-bar tab buttons — icon strip hosted by the global top bar.
//!
//! Each tab is a 36px-wide button with a centered SVG icon plus a 2px bottom
//! accent on the active tab. Click handlers mutate the `RightSidebar` entity
//! directly so they fire regardless of focus.
//!
//! Layout note: these buttons used to live inside a dedicated 40px strip at the
//! top of `RightSidebar`. Now they're embedded into `top_bar`'s right zone so
//! the chrome reads as one continuous row. The right sidebar column below
//! contains only the panel body.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, Window, div, px, svg,
};
use oximux_settings::Theme;

use crate::shell::right_sidebar::RightSidebar;
use crate::shell::right_sidebar::layout::{ACTIVE_INDICATOR_THICKNESS, TAB_BUTTON_WIDTH};
use crate::shell::right_sidebar::tab::RightTab;

/// Tab icon size — matches lucide-react's default 16px.
const ICON_SIZE: f32 = 16.;

/// Render the horizontal row of activity-bar tab buttons for the given
/// `tabs`. Returns just the buttons; the caller is responsible for the
/// surrounding container (height, background, borders).
pub fn render_tab_buttons(
    active: RightTab,
    tabs: &[RightTab],
    sidebar: &Entity<RightSidebar>,
    theme: Theme,
) -> impl IntoElement {
    let buttons: Vec<_> = tabs
        .iter()
        .map(|&tab| render_tab_button(tab, tab == active, sidebar.clone(), theme))
        .collect();

    div().flex().flex_row().h_full().children(buttons)
}

fn render_tab_button(
    tab: RightTab,
    is_active: bool,
    sidebar: Entity<RightSidebar>,
    theme: Theme,
) -> impl IntoElement {
    let icon_color = if is_active {
        theme.focus_ring
    } else {
        theme.fg_muted
    };
    let indicator_color = if is_active {
        theme.focus_ring
    } else {
        gpui::transparent_black()
    };

    let icon = svg()
        .path(tab.icon_path())
        .size(px(ICON_SIZE))
        .text_color(icon_color);

    let inner = div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .child(icon);

    // 2px bottom-border indicator for the active tab; transparent otherwise.
    let indicator = div()
        .w_full()
        .h(ACTIVE_INDICATOR_THICKNESS)
        .bg(indicator_color);

    div()
        .w(TAB_BUTTON_WIDTH)
        .h_full()
        .flex()
        .flex_col()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                sidebar.update(cx, |s, cx| s.select_tab(tab, cx));
            },
        )
        .child(inner)
        .child(indicator)
}
