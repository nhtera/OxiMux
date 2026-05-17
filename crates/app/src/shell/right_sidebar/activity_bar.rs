//! Top tab bar — horizontal strip across the top of the right sidebar.
//!
//! the reference UX-style: SVG icon tabs flow left-to-right, active tab marked with a
//! 2px bottom border accent, toggle button pinned to the right edge. Click
//! handlers mutate the RightSidebar entity directly so they fire regardless
//! of which element currently holds focus (no action-dispatch routing
//! dependency).

use gpui::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, Window, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::right_sidebar::RightSidebar;
use crate::shell::right_sidebar::layout::{
    ACTIVE_INDICATOR_THICKNESS, ACTIVITY_BAR_HEIGHT, TAB_BUTTON_WIDTH,
};
use crate::shell::right_sidebar::tab::RightTab;

/// Tab/toggle icon size — matches lucide-react's default 16px in the reference UX.
const ICON_SIZE: f32 = 16.;

/// Render the horizontal top tab bar for the given set of `tabs`.
pub fn render_top_tab_bar(
    active: RightTab,
    tabs: &[RightTab],
    sidebar: &Entity<RightSidebar>,
    theme: Theme,
    _density: Density,
    _typography: &Typography,
) -> impl IntoElement {
    let tab_buttons: Vec<_> = tabs
        .iter()
        .map(|&tab| render_tab_button(tab, tab == active, sidebar.clone(), theme))
        .collect();

    div()
        .h(ACTIVITY_BAR_HEIGHT)
        .w_full()
        .flex()
        .flex_row()
        .items_stretch()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(div().flex().flex_row().children(tab_buttons))
        .child(div().flex_1())
        .child(render_toggle_button(sidebar.clone(), theme, true))
}

/// Render a minimal collapsed-state rail — just the expand toggle. Matches
/// the reference UX's pattern where the sidebar can always be re-opened without keyboard.
pub fn render_collapsed_rail(sidebar: &Entity<RightSidebar>, theme: Theme) -> impl IntoElement {
    div()
        .id("right-sidebar-collapsed")
        .w(TAB_BUTTON_WIDTH)
        .h_full()
        .flex()
        .flex_col()
        .bg(theme.bg_panel)
        .child(render_toggle_button(sidebar.clone(), theme, false))
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
        theme.bg_panel
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

    // 2px bottom-border indicator for the active tab; bg_panel (invisible) otherwise.
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

fn render_toggle_button(
    sidebar: Entity<RightSidebar>,
    theme: Theme,
    is_open: bool,
) -> impl IntoElement {
    // PanelRightClose when open (signals collapse-right); PanelRightOpen when
    // collapsed (signals expand-left). Both ship in gpui-component's bundle.
    let icon_path = if is_open {
        "icons/panel-right-close.svg"
    } else {
        "icons/panel-right-open.svg"
    };
    let icon = svg()
        .path(icon_path)
        .size(px(ICON_SIZE))
        .text_color(theme.fg_muted);

    div()
        .w(TAB_BUTTON_WIDTH)
        .h(ACTIVITY_BAR_HEIGHT)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                sidebar.update(cx, |s, cx| s.toggle(cx));
            },
        )
        .child(icon)
}
