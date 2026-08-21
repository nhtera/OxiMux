//! Nav rows at the top of the left rail — Tasks / Automations / Agents / Search.
//!
//! Two of these open a PANE tab rather than a rail body (Tasks, Automations):
//! both are pages that need width, and the rail is 250px. Agents is a rail
//! body. Search is still a shell — clicking it sets `active_nav` and the body
//! falls through to the workspace list.

use gpui::{
    App, Entity, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, Window, div, prelude::FluentBuilder as _, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::LeftRail;

/// Top-level nav items rendered above the WORKSPACES section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Tasks,
    Automations,
    Agents,
    Search,
}

const NAV_ICON_SIZE: f32 = 16.0;

impl NavItem {
    pub const ALL: [NavItem; 4] =
        [NavItem::Tasks, NavItem::Automations, NavItem::Agents, NavItem::Search];

    /// Asset path for the row's leading icon. Resolved by `CompositeAssets`.
    pub fn icon_path(self) -> &'static str {
        match self {
            NavItem::Tasks => "icons/inbox.svg",
            NavItem::Automations => "icons/calendar.svg",
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

    /// Whether this row opens a pane tab instead of swapping the rail body.
    /// The rail keeps no highlight for these — the tab strip is the "you are
    /// here", and two competing highlights would disagree the moment the user
    /// switched tabs.
    pub fn opens_in_pane(self) -> bool {
        matches!(self, NavItem::Tasks | NavItem::Automations)
    }
}

/// Background for a nav row. `active` is `None` when the home (workspace list)
/// body is showing, so no nav row is highlighted. Pure — unit-testable.
pub fn nav_row_bg(item: NavItem, active: Option<NavItem>, theme: Theme) -> Hsla {
    if active == Some(item) {
        // One tier above the rail surface — `bg_panel_alt` sits below
        // `bg_rail` and would read pressed-in instead of lit.
        theme.bg_overlay
    } else {
        theme.bg_rail
    }
}

/// Foreground (label) color for a nav row. See [`nav_row_bg`] for the
/// `active == None` (home) case.
pub fn nav_row_fg(item: NavItem, active: Option<NavItem>, theme: Theme) -> Hsla {
    if active == Some(item) {
        theme.fg_base
    } else {
        theme.fg_muted
    }
}

/// Icon color for a nav row — one step QUIETER than the label when
/// inactive (`fg_subtle` vs the label's `fg_muted`), snapping to full
/// strength with the label when active. The extra icon step is what
/// telegraphs the active nav at a glance in a column of look-alike rows.
pub fn nav_row_icon_fg(item: NavItem, active: Option<NavItem>, theme: Theme) -> Hsla {
    if active == Some(item) {
        theme.fg_base
    } else {
        theme.fg_subtle
    }
}

pub fn render_nav_section(
    active: Option<NavItem>,
    agents_unread: u32,
    rail: &Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let mut col = div().flex().flex_col().w_full();
    for item in NavItem::ALL {
        let badge = if item == NavItem::Agents {
            agents_unread
        } else {
            0
        };
        col = col.child(render_nav_row(
            item,
            active,
            badge,
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
    active: Option<NavItem>,
    badge: u32,
    rail: Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let fg = nav_row_fg(item, active, theme);
    let icon_fg = nav_row_icon_fg(item, active, theme);
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
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                // Pass `window` explicitly so Tasks can call the pane opener,
                // which requires a Window context (RT-1).
                rail.update(cx, |r, cx| r.select_nav_in(item, window, cx));
            },
        )
        .child(
            svg()
                .path(item.icon_path())
                .size(px(NAV_ICON_SIZE))
                .text_color(icon_fg),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(fg)
                .child(item.label()),
        )
        .when(badge > 0, |row| {
            // Unread chip — sessions that hit an attention/terminal state
            // while this page was closed. Cleared when the page opens.
            row.child(
                div()
                    .px(px(5.0))
                    .rounded(px(density.r_chip))
                    .bg(theme.bg_overlay)
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_muted)
                    .child(if badge > 99 {
                        "99+".to_string()
                    } else {
                        badge.to_string()
                    }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_tasks_returns_bg_overlay() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Tasks, Some(NavItem::Tasks), t),
            t.bg_overlay
        );
    }

    #[test]
    fn inactive_tasks_returns_bg_rail() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Tasks, Some(NavItem::Agents), t),
            t.bg_rail
        );
    }

    #[test]
    fn home_state_highlights_no_row() {
        // active == None => home (workspace list) showing, nothing highlighted.
        let t = Theme::charcoal();
        assert_eq!(nav_row_bg(NavItem::Tasks, None, t), t.bg_rail);
        assert_eq!(nav_row_fg(NavItem::Tasks, None, t), t.fg_muted);
    }

    #[test]
    fn active_agents_returns_bg_overlay() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Agents, Some(NavItem::Agents), t),
            t.bg_overlay
        );
    }

    #[test]
    fn active_search_returns_bg_overlay() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Search, Some(NavItem::Search), t),
            t.bg_overlay
        );
    }

    #[test]
    fn inactive_search_returns_bg_rail() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_bg(NavItem::Search, Some(NavItem::Tasks), t),
            t.bg_rail
        );
    }

    #[test]
    fn active_row_uses_fg_base() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_fg(NavItem::Tasks, Some(NavItem::Tasks), t),
            t.fg_base
        );
    }

    #[test]
    fn inactive_row_uses_fg_muted() {
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_fg(NavItem::Tasks, Some(NavItem::Agents), t),
            t.fg_muted
        );
    }

    #[test]
    fn icon_sits_one_step_below_label_when_inactive() {
        // Inactive icon must be QUIETER than the label (subtle < muted) so
        // the active row's full-strength icon stands out in the column.
        let t = Theme::charcoal();
        assert_eq!(
            nav_row_icon_fg(NavItem::Tasks, Some(NavItem::Agents), t),
            t.fg_subtle
        );
        assert_eq!(nav_row_icon_fg(NavItem::Tasks, None, t), t.fg_subtle);
        assert_eq!(
            nav_row_icon_fg(NavItem::Tasks, Some(NavItem::Tasks), t),
            t.fg_base
        );
    }

    #[test]
    fn all_nav_items_covered() {
        assert_eq!(NavItem::ALL.len(), 4);
    }

    /// Every row needs a label and an icon: a nav row that renders as a blank
    /// strip is worse than a missing one, because it is clickable.
    #[test]
    fn every_nav_item_is_labelled_and_illustrated() {
        for item in NavItem::ALL {
            assert!(!item.label().is_empty(), "{item:?} has no label");
            assert!(
                item.icon_path().starts_with("icons/") && item.icon_path().ends_with(".svg"),
                "{item:?} has no icon: {}",
                item.icon_path()
            );
        }
    }

    /// The pane-opening rows are exactly Tasks and Automations. Getting this
    /// wrong is silent: a pane row that fell through to `select_nav` would set
    /// `active_nav` and swap the rail body to the workspace list, which reads
    /// as "the click did nothing".
    #[test]
    fn only_the_page_rows_open_in_a_pane() {
        assert!(NavItem::Tasks.opens_in_pane());
        assert!(NavItem::Automations.opens_in_pane());
        assert!(!NavItem::Agents.opens_in_pane());
        assert!(!NavItem::Search.opens_in_pane());
    }
}
