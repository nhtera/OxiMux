//! Nav rows at the top of the left rail — Tasks / Automations / Agents / Search.
//!
//! Shells only in Phase 02 — clicking a row sets `active_nav` but the bodies
//! show placeholder text until v1-build Phase 07 wires the real entities.

use gpui::{
    App, Entity, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, Window, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::LeftRail;

/// Top-level nav items rendered above the WORKSPACES section. Order matches
/// the reference UX's `SidebarNav.tsx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Tasks,
    Automations,
    Agents,
    Search,
}

const NAV_ICON_SIZE: f32 = 16.0;

impl NavItem {
    pub const ALL: [NavItem; 4] = [
        NavItem::Tasks,
        NavItem::Automations,
        NavItem::Agents,
        NavItem::Search,
    ];

    /// Asset path for the row's leading icon. Resolved by `CompositeAssets`.
    pub fn icon_path(self) -> &'static str {
        match self {
            NavItem::Tasks => "icons/inbox.svg",
            NavItem::Automations => "icons/bell.svg",
            NavItem::Agents => "icons/bot.svg",
            NavItem::Search => "icons/search.svg",
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            NavItem::Tasks => "Tasks",
            NavItem::Automations => "Automations",
            NavItem::Agents => "Agents",
            NavItem::Search => "Search",
        }
    }
}

/// Background for an active vs inactive nav row. Pure — unit-testable.
pub fn nav_row_bg(item: NavItem, active: NavItem, theme: Theme) -> Hsla {
    if item == active {
        theme.bg_panel_alt
    } else {
        theme.bg_panel
    }
}

/// Foreground (icon + label) color for an active vs inactive nav row.
pub fn nav_row_fg(item: NavItem, active: NavItem, theme: Theme) -> Hsla {
    if item == active {
        theme.fg_base
    } else {
        theme.fg_muted
    }
}

pub fn render_nav_section(
    active: NavItem,
    rail: &Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let mut col = div().flex().flex_col().w_full();
    for item in NavItem::ALL {
        col = col.child(render_nav_row(
            item,
            active,
            rail.clone(),
            theme,
            density,
            typography,
        ));
    }
    col
}

fn render_nav_row(
    item: NavItem,
    active: NavItem,
    rail: Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let fg = nav_row_fg(item, active, theme);
    let bg = nav_row_bg(item, active, theme);

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row + 4.))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .bg(bg)
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                rail.update(cx, |r, cx| r.select_nav(item, cx));
            },
        )
        .child(
            svg()
                .path(item.icon_path())
                .size(px(NAV_ICON_SIZE))
                .text_color(fg),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(fg)
                .child(item.label()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_tasks_returns_bg_panel_alt() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Tasks, NavItem::Tasks, t),
            t.bg_panel_alt
        );
    }

    #[test]
    fn inactive_tasks_returns_bg_panel() {
        let t = Theme::charcoal();
        assert_eq!(nav_row_bg(NavItem::Tasks, NavItem::Agents, t), t.bg_panel);
    }

    #[test]
    fn active_automations_returns_bg_panel_alt() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Automations, NavItem::Automations, t),
            t.bg_panel_alt
        );
    }

    #[test]
    fn active_agents_returns_bg_panel_alt() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Agents, NavItem::Agents, t),
            t.bg_panel_alt
        );
    }

    #[test]
    fn active_search_returns_bg_panel_alt() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Search, NavItem::Search, t),
            t.bg_panel_alt
        );
    }

    #[test]
    fn inactive_search_returns_bg_panel() {
        let t = Theme::charcoal();
        assert_eq!(nav_row_bg(NavItem::Search, NavItem::Tasks, t), t.bg_panel);
    }

    #[test]
    fn active_row_uses_fg_base() {
        let t = Theme::charcoal();
        assert_eq!(nav_row_fg(NavItem::Tasks, NavItem::Tasks, t), t.fg_base);
    }

    #[test]
    fn inactive_row_uses_fg_muted() {
        let t = Theme::charcoal();
        assert_eq!(nav_row_fg(NavItem::Tasks, NavItem::Agents, t), t.fg_muted);
    }

    #[test]
    fn all_nav_items_covered() {
        assert_eq!(NavItem::ALL.len(), 4);
    }
}
