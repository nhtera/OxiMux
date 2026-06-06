//! Read-only Appearance + About panes. Surfaces the active design tokens
//! and build/runtime facts; nothing here writes to disk.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Theme, Typography};

use super::controls::info_row;

pub(super) fn render_appearance(theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(info_row("Theme", "Charcoal (dark)", theme, typography))
        .child(info_row("Density", "Cockpit", theme, typography))
        .child(info_row(
            "UI font",
            typography.family_ui.clone(),
            theme,
            typography,
        ))
        .child(info_row(
            "Mono font",
            typography.family_mono.clone(),
            theme,
            typography,
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child("Light mode is a deferred decision. Dark-only for now."),
        )
        .into_any_element()
}

pub(super) fn render_about(theme: Theme, typography: &Typography) -> AnyElement {
    let version = env!("CARGO_PKG_VERSION");
    let data_dir = crate::terminal_settings::app_data_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "(unavailable)".to_string());

    div()
        .flex()
        .flex_col()
        .child(info_row("OxiMux version", version, theme, typography))
        .child(info_row(
            "App data dir",
            SharedString::from(data_dir),
            theme,
            typography,
        ))
        .child(info_row("Settings files", "terminal.toml · commit_message_ai.toml", theme, typography))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child("Agent cockpit — terminals, git review, and AI sessions in one window."),
        )
        .into_any_element()
}
