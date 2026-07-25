//! Read-only Appearance + About panes. Surfaces the active design tokens
//! and build/runtime facts; nothing here writes to disk.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_settings::{Theme, Typography};

use super::controls::info_row;
use super::layout::{SettingEntry, entry};

/// The Appearance pane's `label: value` facts.
fn appearance_pairs(typography: &Typography) -> Vec<(SharedString, SharedString)> {
    vec![
        ("Theme".into(), "Charcoal (dark)".into()),
        ("Density".into(), "Cockpit".into()),
        ("UI font".into(), typography.family_ui.clone()),
        ("Mono font".into(), typography.family_mono.clone()),
    ]
}

/// The About pane's build/runtime facts.
fn about_pairs() -> Vec<(SharedString, SharedString)> {
    let version = env!("CARGO_PKG_VERSION");
    let data_dir = crate::terminal_settings::app_data_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "(unavailable)".to_string());
    vec![
        ("OxiMux version".into(), SharedString::from(version)),
        ("App data dir".into(), SharedString::from(data_dir)),
        (
            "Settings files".into(),
            "terminal.toml · commit_message_ai.toml".into(),
        ),
    ]
}

/// Map `label: value` facts to search entries (the value is the right-hand
/// "control"). Used by global search.
fn pairs_to_entries(
    pairs: Vec<(SharedString, SharedString)>,
    theme: Theme,
    typography: &Typography,
) -> Vec<SettingEntry> {
    pairs
        .into_iter()
        .map(|(label, value)| {
            let val = div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(value);
            entry(label, "", val)
        })
        .collect()
}

pub(super) fn appearance_entries(theme: Theme, typography: &Typography) -> Vec<SettingEntry> {
    pairs_to_entries(appearance_pairs(typography), theme, typography)
}

pub(super) fn about_entries(theme: Theme, typography: &Typography) -> Vec<SettingEntry> {
    pairs_to_entries(about_pairs(), theme, typography)
}

/// Render `pairs` as `label: value` info rows, keeping only those that match
/// `query` (against label or value). Empty result → a muted placeholder so
/// the pane never looks blank while filtering. `footer` is the quiet caption
/// shown below, hidden while a filter is active.
fn info_pane(
    query: &str,
    pairs: &[(SharedString, SharedString)],
    footer: &'static str,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let q = query.to_lowercase();
    let mut col = div().flex().flex_col();
    let mut shown = 0usize;
    for (label, value) in pairs {
        if !q.is_empty()
            && !label.to_lowercase().contains(&q)
            && !value.to_lowercase().contains(&q)
        {
            continue;
        }
        col = col.child(info_row(label.clone(), value.clone(), theme, typography));
        shown += 1;
    }
    if shown == 0 {
        return col
            .child(
                div()
                    .py(px(6.0))
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child("No matching settings."),
            )
            .into_any_element();
    }
    if q.is_empty() {
        col = col.child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(footer),
        );
    }
    col.into_any_element()
}

pub(super) fn render_appearance(query: &str, theme: Theme, typography: &Typography) -> AnyElement {
    info_pane(
        query,
        &appearance_pairs(typography),
        "Light mode is a deferred decision. Dark-only for now.",
        theme,
        typography,
    )
}

pub(super) fn render_about(query: &str, theme: Theme, typography: &Typography) -> AnyElement {
    info_pane(
        query,
        &about_pairs(),
        "Agent cockpit — terminals, git review, and AI sessions in one window.",
        theme,
        typography,
    )
}
