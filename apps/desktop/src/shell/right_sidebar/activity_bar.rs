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
    StatefulInteractiveElement as _, Styled, Window, div, px, svg,
};
use gpui_component::tooltip::Tooltip;
use oximux_settings::Theme;

use crate::shell::right_sidebar::RightSidebar;
use crate::shell::right_sidebar::layout::{ACTIVE_INDICATOR_THICKNESS, TAB_BUTTON_WIDTH};
use crate::shell::right_sidebar::tab::RightTab;

/// Tab icon size — 16px, matches the icon grid used elsewhere in the
/// activity bar and gpui-component's small-button glyphs.
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
    // Active tab uses the base foreground rather than the accent ring: the
    // reference layout treats activity-bar selection as "this is current
    // chrome", not "this needs attention", so a high-contrast neutral reads
    // better than a saturated accent.
    let icon_color = if is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    let indicator_color = if is_active {
        theme.fg_base
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

    // Active-tab indicator sits 3px above the chrome strip's own bottom
    // border so the two lines don't visually merge into a doubled rule at
    // the seam between the top bar and the panel below.
    let indicator = div()
        .w_full()
        .h(ACTIVE_INDICATOR_THICKNESS)
        .bg(indicator_color);
    let indicator_row = div().w_full().pb(px(3.0)).child(indicator);

    // Hover tooltip: tab name + keyboard shortcut, resolved live from the
    // keymap registry so user overrides show up without a restart.
    let tooltip_text: gpui::SharedString = tooltip_label(tab).into();

    div()
        .id(("right-sidebar-tab", tab as usize))
        .w(TAB_BUTTON_WIDTH)
        .h_full()
        .flex()
        .flex_col()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                sidebar.update(cx, |s, cx| s.select_tab(tab, cx));
                // History's search field needs keyboard focus for typing/nav.
                // Focus set *inside* the mouse-down is clobbered by the post-click
                // focus dispatch, so defer it a tick (same fix as elsewhere).
                if tab == RightTab::History {
                    let sidebar = sidebar.clone();
                    window.defer(cx, move |window, cx| {
                        sidebar.update(cx, |s, cx| s.focus_history(window, cx));
                    });
                }
            },
        )
        .tooltip(move |window: &mut Window, cx| {
            Tooltip::new(tooltip_text.clone()).build(window, cx)
        })
        .child(inner)
        .child(indicator_row)
}

/// Keyboard shortcut hint for a tab, resolved live from the keymap
/// registry so a user override shows up in the tooltip. Empty when the
/// action is unbound (`Files` has no select action and is hidden from
/// `visible_tabs`, so its arm is unreachable at runtime).
fn shortcut_hint(tab: RightTab) -> String {
    let id = match tab {
        RightTab::Files => return String::new(),
        RightTab::Explorer => "select_explorer_tab",
        RightTab::Search => "select_search_tab",
        RightTab::SourceControl => "select_source_control_tab",
        RightTab::History => "select_history_tab",
    };
    crate::keymap_registry::display_chord_for(id).unwrap_or_default()
}

/// Format the tooltip label. Drops the parenthesized shortcut when the
/// hint is empty so we don't render "Files ()".
fn tooltip_label(tab: RightTab) -> String {
    let hint = shortcut_hint(tab);
    if hint.is_empty() {
        tab.title().to_string()
    } else {
        format!("{} ({hint})", tab.title())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap_registry::SECONDARY_GLYPH as SEC;

    #[test]
    fn shortcut_hint_reads_registry_defaults() {
        // Resolved from the keymap registry — no hand-mirrored strings. The
        // modifier glyph comes from the registry too, because these defaults are
        // `secondary-` chords and render as ⌃ off macOS.
        assert_eq!(shortcut_hint(RightTab::Files), "");
        assert_eq!(shortcut_hint(RightTab::Explorer), format!("{SEC}⇧E"));
        assert_eq!(shortcut_hint(RightTab::Search), format!("{SEC}⇧F"));
        assert_eq!(shortcut_hint(RightTab::SourceControl), format!("{SEC}⇧G"));
    }

    #[test]
    fn tooltip_label_drops_parens_when_unbound() {
        assert_eq!(tooltip_label(RightTab::Files), "Files");
        assert_eq!(
            tooltip_label(RightTab::Explorer),
            format!("Explorer ({SEC}⇧E)")
        );
        assert_eq!(tooltip_label(RightTab::Search), format!("Search ({SEC}⇧F)"));
        assert_eq!(
            tooltip_label(RightTab::SourceControl),
            format!("Source Control ({SEC}⇧G)")
        );
    }
}
