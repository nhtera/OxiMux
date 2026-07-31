//! Appearance + About panes: design tokens, build facts, and the update
//! controls.
//!
//! The About pane is where *all* update feedback lives. Checking, downloading
//! and failures never surface anywhere else in the app — a background check
//! that fails is the updater's problem, not an interruption to hand the user.
//! The one exception is a ready update, which earns a passive pill in the
//! status bar because it is the only state that asks anything of them.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
#[cfg(target_os = "macos")]
use oximux_auto_update::{CheckTrigger, UpdateStatus};
#[cfg(target_os = "macos")]
use oximux_settings::AutoUpdateSettings;
use oximux_settings::{Theme, Typography};

use super::controls::info_row;
#[cfg(target_os = "macos")]
use super::controls::value_chip;
use super::layout::{SettingEntry, entries_card, entry};
use super::SettingsModal;
#[cfg(target_os = "macos")]
use crate::updater::UpdaterState;

/// Where a user goes when the app cannot update itself.
#[cfg(target_os = "macos")]
const RELEASES_URL: &str = "https://github.com/nhtera/OxiMux/releases/latest";

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

/// One line describing where the updater currently stands.
///
/// A failed *background* check reads as an aside ("will retry"), because the
/// user did not ask and nothing is broken from where they sit. A failed
/// *manual* check names the actual error — they clicked, so they are owed the
/// reason.
#[cfg(target_os = "macos")]
fn status_line(status: &UpdateStatus) -> String {
    match status {
        UpdateStatus::Idle => "Checks automatically".into(),
        UpdateStatus::Checking { .. } => "Checking…".into(),
        UpdateStatus::Downloading {
            version,
            received,
            total,
        } => match total {
            Some(total) if *total > 0 => {
                format!("Downloading v{version} — {}%", received * 100 / total)
            }
            _ => format!("Downloading v{version}…"),
        },
        UpdateStatus::Installing { version } => format!("Preparing v{version}…"),
        UpdateStatus::Ready { version, .. } => {
            format!("v{version} ready — restart to apply")
        }
        UpdateStatus::UpToDate => "Up to date".into(),
        UpdateStatus::Unsupported(reason) => reason.describe().to_string(),
        UpdateStatus::Failed {
            error,
            trigger: CheckTrigger::Manual,
        } => format!("Couldn't check: {error}"),
        UpdateStatus::Failed {
            trigger: CheckTrigger::Background,
            ..
        } => "Couldn't check — will retry".into(),
    }
}

/// Colour the status text by whether it wants attention. Only a ready update
/// and a manual failure get a non-muted colour; everything else is chrome.
#[cfg(target_os = "macos")]
fn status_colour(status: &UpdateStatus, theme: Theme) -> gpui::Hsla {
    match status {
        UpdateStatus::Ready { .. } => theme.status_ok,
        UpdateStatus::Failed {
            trigger: CheckTrigger::Manual,
            ..
        } => theme.status_error,
        _ => theme.fg_muted,
    }
}

/// The update row's right-hand control: status text plus a check button.
///
/// The button stays available while a check runs — the pipeline's own guard
/// rejects a second one, so a click during a background check is harmless
/// rather than something to disable a control over.
#[cfg(target_os = "macos")]
fn update_control(
    theme: Theme,
    density: oximux_settings::Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let status = cx
        .try_global::<UpdaterState>()
        .map_or(UpdateStatus::Idle, |state| state.status.clone());
    let updatable = !matches!(status, UpdateStatus::Unsupported(_));

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(status_colour(&status, theme))
                .child(status_line(&status)),
        );

    if matches!(status, UpdateStatus::Ready { .. }) {
        row = row.child(value_chip(
            "auto-update-restart",
            "Restart now",
            theme,
            density,
            typography,
            |_this, _w, cx| {
                crate::updater::restart_to_update(cx);
            },
            cx,
        ));
    } else if updatable {
        row = row.child(value_chip(
            "auto-update-check",
            "Check now",
            theme,
            density,
            typography,
            |_this, _w, cx| {
                crate::updater::check_now(cx);
                cx.notify();
            },
            cx,
        ));
    } else {
        // Can't self-update, but the user is not stuck: a manual download
        // still works, and for a non-writable install root that is the whole
        // remedy. Without this the row states a problem and offers nothing.
        row = row.child(value_chip(
            "auto-update-download",
            "Download update",
            theme,
            density,
            typography,
            |_this, _w, cx| {
                cx.open_url(RELEASES_URL);
            },
            cx,
        ));
    }
    row.into_any_element()
}

/// The About pane's interactive rows. Kept separate from `about_pairs` so the
/// read-only facts stay a plain list.
pub(super) fn update_entries(
    theme: Theme,
    density: oximux_settings::Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    // Nothing to configure without an updater: the toggle would gate a
    // background check that does not run, and the status row would have no
    // status. Omit the section rather than render controls that do nothing.
    // Tail expression rather than an early `return`: with the macOS arms below
    // compiled out, this block *is* the function body, and `return` in tail
    // position is what `clippy::needless_return` fires on. Only reachable off
    // macOS, so only clippy running on a Windows host ever saw it.
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (theme, density, typography, &cx);
        Vec::new()
    }
    #[cfg(target_os = "macos")]
    let enabled = cx
        .try_global::<AutoUpdateSettings>()
        .is_none_or(|settings| settings.enabled);

    #[cfg(target_os = "macos")]
    vec![
        entry(
            "Automatic updates",
            "Check for new versions in the background. Updates apply when you \
             quit — OxiMux never restarts on its own.",
            super::controls::toggle_switch(
                "auto-update-enabled",
                enabled,
                theme,
                |_this, _w, cx| {
                    let mut settings = cx
                        .try_global::<AutoUpdateSettings>()
                        .cloned()
                        .unwrap_or_default();
                    settings.enabled = !settings.enabled;
                    if let Err(err) = crate::auto_update_settings::save(&settings, cx) {
                        tracing::warn!(%err, "could not persist auto-update settings");
                    }
                    cx.notify();
                },
                cx,
            ),
        ),
        entry(
            "Updates",
            "",
            update_control(theme, density, typography, cx),
        ),
    ]
}

pub(super) fn render_about(
    query: &str,
    theme: Theme,
    density: oximux_settings::Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let facts = info_pane(
        query,
        &about_pairs(),
        "Agent cockpit — terminals, git review, and AI sessions in one window.",
        theme,
        typography,
    );

    div()
        .flex()
        .flex_col()
        .gap(px(16.0))
        .child(entries_card(
            theme,
            density,
            typography,
            update_entries(theme, density, typography, cx),
        ))
        .child(facts)
        .into_any_element()
}
