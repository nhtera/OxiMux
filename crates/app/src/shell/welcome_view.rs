//! Welcome screen — empty-state center pane shown when no MainPane is mounted.
//!
//! Borderless, centered directly on the workspace background. Logo tile,
//! wordmark, tagline, and a flat list of keyboard shortcuts — no card
//! surround. `should_show_welcome` is the pure predicate used in tests;
//! Phase 04 will extend it to also gate on workspace selection.

use gpui::{IntoElement, ParentElement, Styled, div, px, svg};
use oximux_settings::{Density, Theme, Typography};

const LOGO_TILE_SIZE: f32 = 96.0;
const LOGO_GLYPH_SIZE: f32 = 56.0;
const CONTENT_MAX_W: f32 = 520.0;
const SECTION_GAP: f32 = 20.0;
const TAGLINE_GAP: f32 = 8.0;

/// Static list of shortcut hint rows shown in the welcome screen. Each
/// shortcut is an ordered slice of key glyphs — rendered as one chip per
/// token with a small gap between, instead of a single "cmd-shift-p"
/// pill. Items marked "(Phase 05)" land when the command palette ships.
pub const SHORTCUT_HINTS: &[(&[&str], &str)] = &[
    (&["\u{2318}", "O"], "Open a project"),
    (&["\u{2318}", "T"], "New terminal"),
    (&["\u{2318}", "D"], "Split pane horizontally"),
    (&["\u{2318}", "L"], "Toggle right sidebar"),
    (&["\u{2318}", "P"], "Quick Open (Phase 05)"),
    (&["\u{2318}", "\u{21E7}", "P"], "Command Palette (Phase 05)"),
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
        .child(content(theme, density, typography))
}

fn content(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(SECTION_GAP))
        .max_w(px(CONTENT_MAX_W))
        .child(logo_tile(theme, density))
        .child(wordmark(theme, typography))
        .child(tagline(theme, typography))
        .child(hints(theme, density, typography))
}

/// Rounded logo tile — solid dark square with the brand glyph centered
/// inside. Sits ~96px tall, ~24px rounded corners.
fn logo_tile(theme: Theme, density: Density) -> impl IntoElement {
    div()
        .w(px(LOGO_TILE_SIZE))
        .h(px(LOGO_TILE_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(density.r_card * 2.0))
        .child(
            svg()
                .path("icons/square-terminal.svg")
                .size(px(LOGO_GLYPH_SIZE))
                .text_color(theme.fg_base),
        )
}

fn wordmark(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_brand * 2.0))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child("OxiMux")
}

fn tagline(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_md))
        .text_color(theme.fg_subtle)
        .child("Open a terminal to begin.")
}

fn hints(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    let mut col = div().flex().flex_col().gap(px(TAGLINE_GAP));
    for (keys, desc) in SHORTCUT_HINTS {
        col = col.child(hint_row(keys, desc, theme, density, typography));
    }
    col
}

fn hint_row(
    keys: &'static [&'static str],
    desc: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    // Compose one chip per key glyph with a small gap. The row uses
    // justify_between so the description hugs the left and the chip
    // cluster hugs the right within a fixed 320px frame.
    let mut chips = div().flex().flex_row().items_center().gap(px(4.));
    for key in keys.iter() {
        chips = chips.child(key_chip(key, theme, density, typography));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .w(px(320.))
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(desc),
        )
        .child(chips)
}

/// One key chip — small square with the glyph centered.
fn key_chip(
    glyph: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .min_w(px(24.))
        .h(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_inactive)
        .rounded(px(density.r_xs))
        .px(px(6.))
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_muted)
        .child(glyph)
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
    fn shortcut_hints_count_is_six() {
        assert_eq!(SHORTCUT_HINTS.len(), 6);
    }

    #[test]
    fn content_max_width_is_520() {
        const _: () = assert!(CONTENT_MAX_W == 520.0);
    }
}
