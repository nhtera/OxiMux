//! Voice-dictation settings pane — edits the `DictationSettings` working copy
//! and drives model downloads through the dictation service. Every control
//! applies immediately: it mutates the copy and writes `dictation.toml` (the
//! watcher re-applies), or calls the service for download/delete.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_dictation::{ModelStatus, catalog};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry};
use super::segmented::{Segment, segmented};
use crate::shell::agent_chat::{
    cancel_model_download, delete_model, download_model, model_status,
};

/// Render the Voice pane: enable toggle, per-model rows, language, and the mic
/// permission status.
pub(super) fn render(
    modal: &SettingsModal,
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
            entries(modal, theme, density, typography, cx),
        ))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(
                    "Models run fully on-device. A model change applies to the next recording. \
                     Changes save to dictation.toml.",
                ),
        )
        .into_any_element()
}

/// The Voice pane's rows (label + description + live control).
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut rows = Vec::new();

    // Master enable.
    let enable = toggle_switch(
        "voice-enabled",
        modal.dictation.enabled,
        theme,
        |this, _w, cx| {
            this.dictation.enabled = !this.dictation.enabled;
            this.persist_voice(cx);
        },
        cx,
    );
    rows.push(entry(
        "Enable dictation",
        "Show the mic button in the chat composer (⌘E).",
        enable,
    ));

    // One row per catalog model.
    for spec in catalog() {
        let control = model_control(modal, spec, theme, density, typography, cx);
        rows.push(entry(
            format!("{} · {} MB", spec.label, spec.size_mb),
            spec.langs,
            control,
        ));
    }

    // Language selector (whisper models; transducers ignore it).
    let lang = segmented(
        "voice-lang",
        vec![
            Segment::new("Auto", modal.dictation.language == "auto", |this, _w, cx| {
                set_language(this, "auto", cx)
            }),
            Segment::new("Tiếng Việt", modal.dictation.language == "vi", |this, _w, cx| {
                set_language(this, "vi", cx)
            }),
            Segment::new("English", modal.dictation.language == "en", |this, _w, cx| {
                set_language(this, "en", cx)
            }),
        ],
        theme,
        density,
        typography,
        cx,
    );
    rows.push(entry(
        "Language",
        "Whisper transcription language. Parakeet (English/EU) ignores this.",
        lang,
    ));

    // Microphone permission status + action.
    rows.push(entry(
        "Microphone",
        "Voice dictation needs microphone access.",
        permission_control(theme, density, typography, cx),
    ));

    rows
}

fn set_language(this: &mut SettingsModal, lang: &str, cx: &mut gpui::Context<SettingsModal>) {
    this.dictation.language = lang.to_string();
    this.persist_voice(cx);
}

/// The status-driven control cluster for one model row.
fn model_control(
    modal: &SettingsModal,
    spec: &oximux_dictation::ModelSpec,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let id = spec.id.to_string();
    let selected = modal.dictation.model_id == spec.id;
    let status = model_status(cx, spec.id);

    let mut row = div().flex().flex_row().items_center().gap(px(6.0));

    match status {
        ModelStatus::Ready => {
            // Select (radio-like). The selected model reads as a filled chip.
            if selected {
                row = row.child(static_chip("Selected", true, theme, density, typography));
            } else {
                let sid = id.clone();
                row = row.child(value_chip(
                    SharedString::from(format!("voice-select-{id}")),
                    "Select",
                    theme,
                    density,
                    typography,
                    move |this, _w, cx| {
                        this.dictation.model_id = sid.clone();
                        this.persist_voice(cx);
                    },
                    cx,
                ));
            }
            let did = id.clone();
            let was_selected = selected;
            row = row.child(value_chip(
                SharedString::from(format!("voice-delete-{id}")),
                "Delete",
                theme,
                density,
                typography,
                move |this, _w, cx| {
                    delete_model(cx, &did);
                    // Deleting the selected model falls back to the default id;
                    // the mic then gates on that model's readiness.
                    if was_selected {
                        this.dictation.model_id =
                            oximux_settings::dictation::DEFAULT_MODEL_ID.to_string();
                        this.persist_voice(cx);
                    }
                    cx.notify();
                },
                cx,
            ));
        }
        ModelStatus::NotDownloaded => {
            let did = id.clone();
            row = row.child(value_chip(
                SharedString::from(format!("voice-download-{id}")),
                format!("Download ({} MB)", spec.size_mb),
                theme,
                density,
                typography,
                move |_this, _w, cx| download_model(cx, &did),
                cx,
            ));
        }
        ModelStatus::Downloading(p) => {
            let pct = (p * 100.0).round() as u32;
            row = row.child(static_chip(
                format!("Downloading… {pct}%"),
                false,
                theme,
                density,
                typography,
            ));
            let did = id.clone();
            row = row.child(value_chip(
                SharedString::from(format!("voice-cancel-{id}")),
                "Cancel",
                theme,
                density,
                typography,
                move |_this, _w, cx| cancel_model_download(cx, &did),
                cx,
            ));
        }
        ModelStatus::Extracting => {
            row = row.child(static_chip("Installing…", false, theme, density, typography));
        }
        ModelStatus::Failed(msg) => {
            row = row.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.status_error)
                    .child(SharedString::from(truncate(&msg, 40))),
            );
            let did = id.clone();
            row = row.child(value_chip(
                SharedString::from(format!("voice-retry-{id}")),
                "Retry",
                theme,
                density,
                typography,
                move |_this, _w, cx| download_model(cx, &did),
                cx,
            ));
        }
    }
    row.into_any_element()
}

/// A non-clickable chip (status text / the selected marker).
fn static_chip(
    text: impl Into<SharedString>,
    accent: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let mut chip = div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_overlay_item))
        .px(px(10.0))
        .rounded(px(density.r_chip))
        .border_1()
        .text_size(px(typography.t_body_sm));
    chip = if accent {
        chip.border_color(theme.status_info)
            .text_color(theme.status_info)
    } else {
        chip.border_color(theme.border_inactive)
            .text_color(theme.fg_muted)
    };
    chip.child(text.into()).into_any_element()
}

/// The mic-permission status line plus an actionable button.
fn permission_control(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    use crate::mic_permission::{MicPermission, status};
    let st = status();
    let label = match st {
        MicPermission::Granted => "Granted",
        MicPermission::Denied => "Denied",
        MicPermission::Restricted => "Restricted",
        MicPermission::Undetermined => "Not requested",
    };
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(static_chip(label, matches!(st, MicPermission::Granted), theme, density, typography));

    match st {
        MicPermission::Undetermined => {
            row = row.child(value_chip(
                "voice-perm-request",
                "Request access",
                theme,
                density,
                typography,
                |_this, _w, _cx| crate::mic_permission::request(|_granted| {}),
                cx,
            ));
        }
        MicPermission::Denied | MicPermission::Restricted => {
            row = row.child(value_chip(
                "voice-perm-open",
                "Open System Settings",
                theme,
                density,
                typography,
                |_this, _w, _cx| open_mic_settings(),
                cx,
            ));
        }
        MicPermission::Granted => {}
    }
    row.into_any_element()
}

/// Open the macOS Privacy › Microphone settings pane.
fn open_mic_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        .spawn();
}

/// Truncate a message to `max` chars with an ellipsis (byte-safe for the ASCII
/// error strings the manager emits; falls back to the char boundary otherwise).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}
