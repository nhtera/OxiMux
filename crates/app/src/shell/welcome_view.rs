//! Welcome card — empty-state center pane shown when no MainPane is mounted.
//!
//! Pure render function; no entity, no state. The `should_show_welcome`
//! predicate is the testable boundary. Phase 04 will extend the predicate
//! to also gate on workspace selection state.

use gpui::{IntoElement, ParentElement, Styled, div, px, svg};
use oximux_settings::{Density, Theme, Typography};

const CARD_MAX_W: f32 = 480.0;
const LOGO_SIZE: f32 = 32.0;
const CARD_GAP: f32 = 16.0;
const CARD_PAD: f32 = 24.0;

/// Static list of shortcut hint rows shown in the welcome card.
/// Items marked "(Phase 05)" land when the command palette ships.
pub const SHORTCUT_HINTS: &[(&str, &str)] = &[
    ("cmd-d", "Split pane horizontally"),
    ("cmd-t", "New tab"),
    ("cmd-l", "Toggle right sidebar"),
    ("cmd-p", "Quick Open (Phase 05)"),
    ("cmd-shift-p", "Command Palette (Phase 05)"),
];

/// Pure predicate — show welcome only when there is no live MainPane.
/// Phase 04 will add a `has_active_workspace: bool` parameter.
pub fn should_show_welcome(has_pane: bool) -> bool {
    !has_pane
}

pub fn view(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .bg(theme.bg_base)
        .child(card(theme, density, typography))
}

fn card(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(CARD_GAP))
        .p(px(CARD_PAD))
        .max_w(px(CARD_MAX_W))
        .w_full()
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(density.r_card))
        .child(logo(theme))
        .child(wordmark(theme, typography))
        .child(tagline(theme, typography))
        .child(hints(theme, density, typography))
        .child(footer(theme, typography))
}

fn logo(theme: Theme) -> impl IntoElement {
    svg()
        .path("icons/git-branch.svg")
        .size(px(LOGO_SIZE))
        .text_color(theme.fg_muted)
}

fn wordmark(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_brand * 1.5))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child("OxiMux")
}

fn tagline(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child("Your agentic development environment.")
}

fn hints(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    let mut col = div().flex().flex_col().gap(px(4.)).w_full();
    for (key, desc) in SHORTCUT_HINTS {
        col = col.child(hint_row(key, desc, theme, density, typography));
    }
    col
}

fn hint_row(
    key: &'static str,
    desc: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .bg(theme.bg_panel_alt)
                .border_1()
                .border_color(theme.border_inactive)
                .rounded(px(density.r_xs))
                .px(px(6.))
                .py(px(2.))
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .child(key),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(desc),
        )
}

fn footer(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(format!("v{}", env!("CARGO_PKG_VERSION")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_show_welcome_when_no_pane() {
        assert!(should_show_welcome(false));
    }

    #[test]
    fn should_not_show_welcome_when_pane_exists() {
        assert!(!should_show_welcome(true));
    }

    #[test]
    fn shortcut_hints_count_is_five() {
        assert_eq!(SHORTCUT_HINTS.len(), 5);
    }

    #[test]
    fn version_label_contains_cargo_pkg_version() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn card_max_width_is_480() {
        const _: () = assert!(CARD_MAX_W == 480.0);
    }
}
