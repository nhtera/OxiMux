//! Welcome screen — empty-state center pane shown when no project panes
//! are mounted.
//!
//! Borderless, centered directly on the workspace background. Logo tile,
//! wordmark, launch actions, and a compact keyboard strip — no full-screen
//! marketing hero. `should_show_welcome` is the pure predicate used in tests.

use gpui::{IntoElement, ParentElement, Styled, div, px, svg};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::welcome_actions;

const LOGO_TILE_SIZE: f32 = 72.0;
const LOGO_GLYPH_SIZE: f32 = 40.0;
const CONTENT_MAX_W: f32 = 560.0;
const SECTION_GAP: f32 = 16.0;
const TAGLINE_GAP: f32 = 8.0;

/// Shortcut hint rows shown in the welcome screen, as (registry action id,
/// description). Glyphs resolve live from the keymap registry — one chip
/// per key token — so user overrides show up here too. The welcome state
/// keeps only project-entry shortcuts because terminal and split-pane
/// commands need an active workspace.
pub const SHORTCUT_HINTS: &[(&str, &str)] = &[
    ("open_project_picker", "Open a project"),
    ("open_workspace_create", "New workspace"),
    ("open_quick_open", "Quick Open"),
    ("open_command_palette", "Command Palette"),
];

/// Pure predicate — show welcome only when there are no live project panes.
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
        .child(welcome_actions::launch_panel(theme, density, typography))
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
        // 2× r_card (16px) reads as a "feature card" radius — distinct from
        // the panel chrome that uses plain r_card (8px). Used only on the
        // welcome-view logo tile; the multiplier stays inline rather than
        // promoting to a density token until a second site appears.
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
        .child("Open a project, create a workspace, then start an agent.")
}

fn hints(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    let mut col = div().flex().flex_col().gap(px(TAGLINE_GAP));
    for (action_id, desc) in SHORTCUT_HINTS {
        // An unbound action (user cleared the chord) drops its hint row.
        let Some(keys) = crate::keymap_registry::display_tokens_for(action_id) else {
            continue;
        };
        col = col.child(hint_row(keys, desc, theme, density, typography));
    }
    col
}

fn hint_row(
    keys: Vec<String>,
    desc: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    // Compose one chip per key glyph with a small gap. The row uses
    // justify_between so the description hugs the left and the chip
    // cluster hugs the right within a fixed 320px frame.
    let mut chips = div().flex().flex_row().items_center().gap(px(4.));
    for key in keys {
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
    glyph: String,
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
    fn shortcut_hints_resolve_from_registry() {
        assert_eq!(SHORTCUT_HINTS.len(), 4);
        for (id, _) in SHORTCUT_HINTS {
            assert!(
                crate::keymap_registry::display_tokens_for(id).is_some(),
                "welcome hint `{id}` should resolve a default chord"
            );
        }
        // `open_project_picker`'s default is a `secondary-` chord, so the leading
        // token is ⌘ on macOS and ⌃ elsewhere — it was pinned to ⌘ here.
        assert_eq!(
            crate::keymap_registry::display_tokens_for("open_project_picker").unwrap(),
            vec![
                crate::keymap_registry::SECONDARY_GLYPH.to_string(),
                "O".to_string()
            ]
        );
    }

    #[test]
    fn content_max_width_is_560() {
        const _: () = assert!(CONTENT_MAX_W == 560.0);
    }
}
