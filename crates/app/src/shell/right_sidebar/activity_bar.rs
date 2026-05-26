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

    // Hover tooltip: tab name + keyboard shortcut. Shortcut strings here
    // mirror the bindings registered in main.rs sidebar keybinds —
    // keep both sides in sync if those bindings move.
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
            move |_: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                sidebar.update(cx, |s, cx| s.select_tab(tab, cx));
            },
        )
        .tooltip(move |window: &mut Window, cx| {
            Tooltip::new(tooltip_text.clone()).build(window, cx)
        })
        .child(inner)
        .child(indicator_row)
}

/// Keyboard shortcut hint string for a tab, or empty when no binding is
/// registered. Pure function so it can be unit-tested without a GPUI
/// runtime; kept here (not in `tab.rs`) so the `tab` module remains free
/// of binding-string knowledge — that's a `main.rs` concern that the
/// activity bar mirrors for tooltip purposes.
///
/// `Files` is currently unbound at the keymap level (see `main.rs`
/// `cmd-shift-t` comment): the tab is hidden from `visible_tabs`, so
/// `render_tab_button` is never called with `Files` and this arm is
/// unreachable at runtime. The empty string is the correct value when
/// `Files` is reintroduced without a new keybinding.
fn shortcut_hint(tab: RightTab) -> &'static str {
    match tab {
        RightTab::Files => "",
        RightTab::Explorer => "⌘⇧E",
        RightTab::Search => "⌘⇧F",
        RightTab::SourceControl => "⌘⇧G",
    }
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

    #[test]
    fn shortcut_hint_matches_main_bindings() {
        // Mirror of crates/app/src/main.rs sidebar keybinds. Update both
        // at once. `Files` is intentionally empty — currently unbound.
        assert_eq!(shortcut_hint(RightTab::Files), "");
        assert_eq!(shortcut_hint(RightTab::Explorer), "⌘⇧E");
        assert_eq!(shortcut_hint(RightTab::Search), "⌘⇧F");
        assert_eq!(shortcut_hint(RightTab::SourceControl), "⌘⇧G");
    }

    #[test]
    fn tooltip_label_drops_parens_when_unbound() {
        assert_eq!(tooltip_label(RightTab::Files), "Files");
        assert_eq!(tooltip_label(RightTab::Explorer), "Explorer (⌘⇧E)");
        assert_eq!(tooltip_label(RightTab::Search), "Search (⌘⇧F)");
        assert_eq!(
            tooltip_label(RightTab::SourceControl),
            "Source Control (⌘⇧G)"
        );
    }
}
