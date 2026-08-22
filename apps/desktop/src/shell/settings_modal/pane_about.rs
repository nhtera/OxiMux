//! Appearance + About panes: design tokens, build facts, and the update
//! controls.
//!
//! The About pane is where *all* update feedback lives. Checking, downloading
//! and failures never surface anywhere else in the app — a background check
//! that fails is the updater's problem, not an interruption to hand the user.
//! The one exception is a ready update, which earns a passive pill in the
//! status bar because it is the only state that asks anything of them.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, ClickEvent, Entity, IntoElement, ParentElement, SharedString, Styled,
    Window, div, px,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, DropdownButton};
use gpui_component::menu::PopupMenuItem;
#[cfg(target_os = "macos")]
use oximux_auto_update::{CheckTrigger, UpdateStatus};
#[cfg(target_os = "macos")]
use oximux_settings::AutoUpdateSettings;
use oximux_settings::{Density, DensityPreset, Theme, ThemeChoice, Typography};

use super::controls::info_row;
#[cfg(target_os = "macos")]
use super::controls::value_chip;
use super::layout::{SettingEntry, entries_card, entry};
use super::segmented::{Segment, segmented};
use super::controls::stepper;
use super::SettingsModal;
#[cfg(target_os = "macos")]
use crate::updater::UpdaterState;

/// Where a user goes when the app cannot update itself.
#[cfg(target_os = "macos")]
const RELEASES_URL: &str = "https://github.com/nhtera/OxiMux/releases/latest";

/// One face's picker: a dropdown labelled by the family in use, opening the
/// machine's font list with the platform default at the top.
///
/// Every row is drawn in its own family, which is the whole reason a font
/// picker is a picker and not a text field — a name means nothing until you see
/// it set. The list itself is enumerated once per launch, on the first render of
/// this pane; see `font_settings::families`.
///
/// The default is a row of its own rather than a checkbox beside the list,
/// spelled `"Consolas (default)"` so it names what you actually get. It has to
/// be separate from the identically-named family below it, because the two
/// differ: unset follows the machine, and a name does not.
fn font_control(
    id: &'static str,
    chosen: Option<String>,
    platform: &'static str,
    on_pick: fn(&mut gpui::App, Option<String>),
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let entity = cx.entity();
    let families = crate::font_settings::families(cx);
    let label = chosen
        .clone()
        .unwrap_or_else(|| format!("{platform} (default)"));
    DropdownButton::new(id)
        .button(
            Button::new(SharedString::from(format!("{id}-btn")))
                .label(label)
                .small()
                .outline(),
        )
        .small()
        // `TopRight` right-aligns the menu under the button so its rows grow
        // down-and-left rather than off the pane's right edge; `scrollable`
        // plus a capped height keeps a few hundred families on screen.
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            menu = menu.scrollable(true).max_h(px(320.0));
            menu = menu.item(font_item(
                window,
                &entity,
                format!("{platform} (default)"),
                None,
                chosen.is_none(),
                on_pick,
            ));
            for family in families {
                let selected = chosen.as_deref() == Some(family.as_str());
                menu = menu.item(font_item(
                    window,
                    &entity,
                    family.clone(),
                    Some(family.clone()),
                    selected,
                    on_pick,
                ));
            }
            menu
        })
        .into_any_element()
}

/// One font row — the family name set in that family, with a trailing check on
/// the active one. `value` is `None` for the "system default" row.
fn font_item(
    window: &mut Window,
    entity: &Entity<SettingsModal>,
    label: String,
    value: Option<String>,
    selected: bool,
    on_pick: fn(&mut gpui::App, Option<String>),
) -> PopupMenuItem {
    let face = value.clone();
    PopupMenuItem::element(move |_window, _cx| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(28.0))
            .min_w(px(240.0))
            .child(
                div()
                    .when_some(face.clone(), |d, family| d.font_family(family))
                    .child(label.clone()),
            )
            .child(div().w(px(16.0)).flex_none().flex().justify_center().when(
                selected,
                |d| d.child(Icon::default().path("icons/check.svg").size(px(14.0))),
            ))
    })
    .on_click(window.listener_for(
        entity,
        move |_m: &mut SettingsModal, _ev: &ClickEvent, _window, cx| {
            on_pick(cx, value.clone());
        },
    ))
}

/// The Appearance pane's controls, as entries.
///
/// Density and zoom are separate rows on purpose — they are easy to mistake
/// for one control, and the descriptions are where that distinction gets
/// made. See `oximux_settings::appearance`.
fn appearance_controls(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let current = crate::appearance_settings::active(cx);

    let theme_pick = segmented(
        "appearance-theme",
        ThemeChoice::ALL
            .iter()
            .map(|choice| {
                let choice = *choice;
                // "Charcoal (dark)" rather than "Charcoal": two material names
                // on their own make the reader guess which is which.
                let label = format!("{} ({})", choice.label(), choice.polarity());
                Segment::new(label, current.theme == choice, move |_this, _w, cx| {
                    crate::appearance_settings::set_theme(cx, choice);
                })
            })
            .collect(),
        theme,
        density,
        typography,
        cx,
    );

    let density_pick = segmented(
        "appearance-density",
        DensityPreset::ALL
            .iter()
            .map(|preset| {
                let preset = *preset;
                Segment::new(preset.label(), current.density == preset, move |_this, _w, cx| {
                    crate::appearance_settings::set_density(cx, preset);
                })
            })
            .collect(),
        theme,
        density,
        typography,
        cx,
    );

    let zoom = stepper(
        "appearance-zoom",
        current.scale.label(),
        theme,
        density,
        typography,
        |_this, _w, cx| crate::appearance_settings::zoom_out(cx),
        |_this, _w, cx| crate::appearance_settings::zoom_in(cx),
        cx,
    );

    let faces = crate::font_settings::active(cx);
    let ui_font = font_control(
        "appearance-ui-font",
        faces.ui.clone(),
        oximux_settings::fonts::platform::UI,
        crate::font_settings::set_ui,
        cx,
    );
    let mono_font = font_control(
        "appearance-mono-font",
        faces.mono.clone(),
        oximux_settings::fonts::platform::MONO,
        crate::font_settings::set_mono,
        cx,
    );

    // The terminal grid pins every glyph to one cell width, so a proportional
    // face there is not a taste question — the columns stop lining up. Say so
    // in the row rather than refusing the choice: near-monospace display faces
    // are a real thing to want, and it is their machine.
    let mono_note = if crate::font_settings::is_monospaced(cx, faces.resolved_mono()) {
        "The face terminals, diffs and code are drawn in.".to_string()
    } else {
        format!(
            "{} is not fixed-width — the terminal grid will space unevenly.",
            faces.resolved_mono()
        )
    };

    vec![
        entry(
            "Theme",
            "Charcoal is the original. Paper is the same cockpit drawn light.",
            theme_pick,
        ),
        entry(
            "Density",
            "How much space sits around the text. The text itself stays the same size.",
            density_pick,
        ),
        entry(
            "Interface zoom",
            "Scales the whole cockpit — text and chrome together.",
            zoom,
        ),
        entry(
            "UI font",
            "The face the chrome is drawn in — tabs, panels, menus.",
            ui_font,
        ),
        entry("Mono font", mono_note, mono_font),
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
            "terminal.toml · appearance.toml · commit_message_ai.toml".into(),
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

pub(super) fn appearance_entries(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    appearance_controls(theme, density, typography, cx)
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

pub(super) fn render_appearance(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(entries_card(
            theme,
            density,
            typography,
            appearance_entries(theme, density, typography, cx),
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Changes apply immediately and save to appearance.toml."),
        )
        .into_any_element()
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
