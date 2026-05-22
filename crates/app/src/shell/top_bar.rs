//! Per-column top headers — 40px strip aligned with each body column.
//!
//! Per-column header pattern: instead of one full-width chrome row across
//! the entire window, each of the three body columns (left rail / center /
//! right sidebar) renders its OWN 40px header strip. The tab strip stays
//! confined to the center column's width; collapsing a side panel naturally
//! yanks its header along.
//!
//! When the left rail is collapsed, its chrome (traffic-light gutter,
//! wordmark, left toggle button) moves to the start of the center header so
//! the toggle stays reachable. Same idea on the right: when the panel is
//! closed, the activity-bar tabs + right toggle move to the end of the
//! center header.

use gpui::{
    AnyElement, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Styled, Window, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{ToggleLeftSidebar, ToggleRightSidebar};

/// Width reserved on the left for macOS traffic lights (12px inset +
/// 3 × ~14px buttons with ~6px gaps + comfortable breathing room before
/// the left-sidebar toggle button starts).
const TRAFFIC_LIGHT_GUTTER: f32 = 76.0;

/// Each toggle icon's hit target. Public so positioning code (e.g. the
/// Pane Actions dropdown anchor) can compute distances from the window
/// right edge without hardcoding the magic 36 in multiple places.
pub const TOGGLE_BUTTON_WIDTH: f32 = 36.0;

/// Toggle icon glyph size.
const ICON_SIZE: f32 = 16.0;

/// Header strip for the left-rail column (when open). Hosts traffic-light
/// gutter, wordmark, and the left-rail close toggle.
pub fn left_header(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    chrome_strip(theme, density).child(left_chrome_cluster(true, theme, typography))
}

/// Header strip for the center column. Hosts any chrome bits whose
/// owning column is currently collapsed (so toggles stay reachable);
/// the center zone is a spacer in the pane-groups layout — each group
/// carries its own tab strip beneath this header.
pub fn center_header(
    left_open: bool,
    right_open: bool,
    center_zone: Option<AnyElement>,
    right_tabs: Option<AnyElement>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let mut row = chrome_strip(theme, density);
    if !left_open {
        // Left rail is collapsed — host the chrome cluster (toggle now uses
        // the "open" icon since clicking it expands the rail).
        row = row.child(left_chrome_cluster(false, theme, typography));
    }
    let center: AnyElement = center_zone.unwrap_or_else(|| spacer_zone().into_any_element());
    row = row.child(center);
    if !right_open {
        row = row.child(right_chrome_cluster(false, right_tabs, theme));
    }
    row
}

/// Header strip for the right-sidebar column (when open). Hosts the
/// activity-bar tab buttons and the right-sidebar close toggle.
pub fn right_header(
    right_tabs: Option<AnyElement>,
    theme: Theme,
    density: Density,
) -> impl IntoElement {
    chrome_strip(theme, density).child(right_chrome_cluster(true, right_tabs, theme))
}

fn chrome_strip(theme: Theme, density: Density) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_top_bar))
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
}

fn left_chrome_cluster(left_open: bool, theme: Theme, typography: &Typography) -> impl IntoElement {
    // Order: traffic gutter → wordmark → left-rail toggle. Keeping the
    // wordmark anchored left mirrors macOS native chrome.
    let wordmark = div()
        .px(px(8.0))
        .text_size(px(typography.t_brand))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child("OxiMux");

    div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .flex_shrink_0()
        .child(div().w(px(TRAFFIC_LIGHT_GUTTER)))
        .child(wordmark)
        .child(toggle_button(
            left_toggle_icon(left_open),
            theme,
            ToggleSide::Left,
        ))
}

fn right_chrome_cluster(
    right_open: bool,
    right_tabs: Option<AnyElement>,
    theme: Theme,
) -> impl IntoElement {
    // When the right sidebar is open this cluster sits inside its own
    // `right_header` strip and should span the full strip width so the toggle
    // can anchor at the far right. When the sidebar is closed the cluster is
    // appended to `center_header`; the preceding `flex_1` spacer there
    // already shoves it to the right edge, so `flex_shrink_0` keeps it intact.
    let zone_base = div().flex().flex_row().items_center().h_full();
    let mut zone = if right_open {
        zone_base.w_full()
    } else {
        zone_base.flex_shrink_0()
    };
    if let Some(tabs) = right_tabs {
        zone = zone.child(tabs);
    }
    if right_open {
        zone = zone.child(div().flex_1().h_full());
    }
    zone.child(toggle_button(
        right_toggle_icon(right_open),
        theme,
        ToggleSide::Right,
    ))
}

fn spacer_zone() -> impl IntoElement {
    div().flex().flex_1().h_full().min_w(px(0.0))
}

#[derive(Clone, Copy)]
enum ToggleSide {
    Left,
    Right,
}

fn toggle_button(icon_path: &'static str, theme: Theme, side: ToggleSide) -> impl IntoElement {
    let glyph = svg()
        .path(icon_path)
        .size(px(ICON_SIZE))
        .text_color(theme.fg_muted);

    div()
        .w(px(TOGGLE_BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut gpui::App| match side {
                ToggleSide::Left => {
                    window.dispatch_action(Box::new(ToggleLeftSidebar), cx);
                }
                ToggleSide::Right => {
                    window.dispatch_action(Box::new(ToggleRightSidebar), cx);
                }
            },
        )
        .child(glyph)
}

/// Lucide `PanelLeftClose` when open (click collapses left); `PanelLeftOpen`
/// when collapsed (click expands left). Both ship in gpui-component bundle.
pub(crate) fn left_toggle_icon(left_open: bool) -> &'static str {
    if left_open {
        "icons/panel-left-close.svg"
    } else {
        "icons/panel-left-open.svg"
    }
}

/// Mirror of `left_toggle_icon` for the right edge.
pub(crate) fn right_toggle_icon(right_open: bool) -> &'static str {
    if right_open {
        "icons/panel-right-close.svg"
    } else {
        "icons/panel-right-open.svg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_open_uses_close_icon() {
        assert_eq!(left_toggle_icon(true), "icons/panel-left-close.svg");
    }

    #[test]
    fn left_closed_uses_open_icon() {
        assert_eq!(left_toggle_icon(false), "icons/panel-left-open.svg");
    }

    #[test]
    fn right_open_uses_close_icon() {
        assert_eq!(right_toggle_icon(true), "icons/panel-right-close.svg");
    }

    #[test]
    fn right_closed_uses_open_icon() {
        assert_eq!(right_toggle_icon(false), "icons/panel-right-open.svg");
    }
}
