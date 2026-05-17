//! Top tab bar — horizontal strip across the top of the right sidebar.
//!
//! the reference UX-style: SVG icon tabs flow left-to-right, active tab marked with a
//! 2px bottom border accent. The right-side collapse toggle lives in the
//! global top_bar (workspace_root.rs) — NOT here — so there is exactly one
//! show/hide affordance regardless of sidebar state. Click handlers mutate
//! the RightSidebar entity directly so they fire regardless of which
//! element currently holds focus (no action-dispatch routing dependency).

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

/// Tab icon size — matches lucide-react's default 16px in the reference UX.
const ICON_SIZE: f32 = 16.;

/// Render the horizontal top tab bar for the given set of `tabs`. Tabs
/// cluster at the left; the trailing `flex_1` spacer eats remaining width
/// so the top_bar's panel-right toggle (rendered separately above) stays
/// the only collapse affordance.
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
