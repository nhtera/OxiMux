//! Activity bar — the 40px vertical icon strip on the far right edge.
//!
//! Renders one button per visible tab. The active tab gets a 2px right-edge
//! accent line. Click handlers call the RightSidebar entity directly via a
//! captured entity handle so tab switches work regardless of which element
//! currently holds focus (no action-dispatch routing dependency).

use gpui::{
    App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::right_sidebar::RightSidebar;
use crate::shell::right_sidebar::layout::{ACTIVE_INDICATOR_WIDTH, ACTIVITY_BAR_WIDTH};
use crate::shell::right_sidebar::tab::RightTab;

/// Render the vertical activity bar for the given set of `tabs`.
///
/// Click handlers mutate `sidebar` directly — bypassing GPUI action dispatch
/// so tab switches fire regardless of which element currently holds focus.
pub fn render_activity_bar(
    active: RightTab,
    tabs: &[RightTab],
    sidebar: &Entity<RightSidebar>,
    theme: Theme,
    _density: Density,
    _typography: &Typography,
) -> impl IntoElement {
    let buttons: Vec<_> = tabs
        .iter()
        .map(|&tab| render_tab_button(tab, tab == active, sidebar.clone(), theme))
        .collect();

    div()
        .w(ACTIVITY_BAR_WIDTH)
        .h_full()
        .flex()
        .flex_col()
        .bg(theme.bg_panel)
        .children(buttons)
}

fn render_tab_button(
    tab: RightTab,
    is_active: bool,
    sidebar: Entity<RightSidebar>,
    theme: Theme,
) -> impl IntoElement {
    let label = tab.label();
    // Use focus_ring as the active accent color (closest semantic match in Theme).
    let fg = if is_active {
        theme.focus_ring
    } else {
        theme.fg_base
    };
    let indicator_color = if is_active {
        theme.focus_ring
    } else {
        theme.bg_panel
    };

    // Right-edge 2px indicator for the active tab; bg_panel color for inactive.
    let indicator = div().w(ACTIVE_INDICATOR_WIDTH).h_full().bg(indicator_color);

    let inner = div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_color(fg)
        .text_size(px(13.))
        .font_weight(gpui::FontWeight::BOLD)
        .child(label);

    div()
        .w_full()
        .h(px(40.))
        .flex()
        .flex_row()
        .items_center()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                // Mutate the sidebar entity directly — avoids routing through
                // GPUI action dispatch (which would target the focused element,
                // not the sidebar).
                sidebar.update(cx, |s, cx| s.select_tab(tab, cx));
            },
        )
        .child(inner)
        .child(indicator)
}
