//! Voice-dictation settings pane — edits the `DictationSettings` working copy
//! and drives model downloads through the dictation service. Every control
//! applies immediately: it mutates the copy and writes `dictation.toml` (the
//! watcher re-applies), or calls the service for download/delete.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Anchor, AnyElement, ClickEvent, ClipboardItem, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, SharedString, Styled, Window, div, px,
};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, DropdownButton};
use gpui_component::input::Input;
use gpui_component::menu::PopupMenuItem;
use oximux_dictation::{
    DEFAULT_MODEL_ID, Family, ModelSpec, ModelStatus, catalog, list_input_devices,
    recommended_model_id, spec_for,
};
use oximux_settings::{
    Density, DictationMode, ModelUnloadTimeout, Theme, Typography, WHISPER_LANGUAGES,
    language_display_name,
};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry, section_card, section_title};
use super::segmented::{Segment, segmented};
use crate::shell::agent_chat::{
    HistoryEntry, cancel_model_download, clear_dictation_history, delete_model,
    dictation_history_entries, download_model, format_history_ts, model_status,
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
        .child(history_section(theme, density, typography, cx))
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

/// The "Dictation history" section: a titled list of recent transcripts (newest
/// first), each a timestamp + text + a copy button, with a Clear action in the
/// header. Recorded automatically on every completed dictation and stored on
/// this device only; empty until the first transcript.
fn history_section(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let items = dictation_history_entries();

    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .pt(px(16.0))
        .pb(px(4.0))
        .child(section_title(
            "Dictation history",
            "Recent transcripts, newest first. Stored on this device.",
            theme,
            typography,
        ));
    if !items.is_empty() {
        header = header.child(value_chip(
            "voice-history-clear",
            "Clear",
            theme,
            density,
            typography,
            |_this, _w, cx| {
                clear_dictation_history();
                cx.notify();
            },
            cx,
        ));
    }

    let body = if items.is_empty() {
        div()
            .py(px(12.0))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .child("No dictation yet — your transcripts will appear here.")
            .into_any_element()
    } else {
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(i, e)| history_row(i, e, theme, typography, cx))
            .collect();
        section_card(theme, density, rows)
    };

    div()
        .flex()
        .flex_col()
        .w_full()
        .child(header)
        .child(body)
        .into_any_element()
}

/// One history row: a fixed-width timestamp, the (wrapping) transcript text, and
/// a copy-to-clipboard button pinned right. `idx` keys the button so ids stay
/// unique even when two transcripts share the same second.
fn history_row(
    idx: usize,
    e: &HistoryEntry,
    theme: Theme,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let copy_text = e.text.clone();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(12.0))
        .w_full()
        .py(px(10.0))
        .child(
            div()
                .flex_none()
                .w(px(120.0))
                .pt(px(1.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(format_history_ts(e.ts))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(SharedString::from(e.text.clone())),
        )
        .child(
            div()
                .id(SharedString::from(format!("voice-history-copy-{idx}")))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .rounded(px(6.0))
                .text_color(theme.fg_muted)
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, _ev, _w, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    }),
                )
                .child(Icon::default().path("icons/copy.svg").size(px(14.0))),
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
        "Dictate into any focused pane — chat, terminal, or editor (⌘E).",
        enable,
    ));

    // Dictation mode: press once to toggle, or hold to record.
    let mode = segmented(
        "voice-mode",
        vec![
            Segment::new(
                "Toggle",
                !modal.dictation.mode.is_hold(),
                |this, _w, cx| set_mode(this, DictationMode::Toggle, cx),
            ),
            Segment::new("Hold", modal.dictation.mode.is_hold(), |this, _w, cx| {
                set_mode(this, DictationMode::Hold, cx)
            }),
        ],
        theme,
        density,
        typography,
        cx,
    );
    rows.push(entry(
        "Dictation mode",
        "Toggle: ⌘E starts, again stops. Hold: dictate while the mic is held.",
        mode,
    ));

    // Input device picker (system default + each enumerated microphone).
    rows.push(entry(
        "Microphone device",
        "Which microphone to capture from. Defaults to the system input.",
        device_control(modal, cx),
    ));

    // Speech model: a single dropdown replacing the per-model list — collapses
    // the whole catalog into one row. The trigger shows the active model; the
    // menu lists every model with its size + download state and a check on the
    // active one. The row description tracks the active model's coverage.
    let model_desc = spec_for(&modal.dictation.model_id)
        .map(|s| s.langs)
        .unwrap_or("On-device speech-to-text.");
    rows.push(entry(
        "Speech model",
        model_desc,
        model_control(modal, theme, typography, cx),
    ));

    // Language selector — only whisper models honor a language; Parakeet,
    // Zipformer and SenseVoice decode a fixed set, so the picker shows only for
    // whisper models. Non-whisper models get a read-only coverage blurb instead.
    let is_whisper = spec_for(&modal.dictation.model_id)
        .map(|s| s.family == Family::Whisper)
        .unwrap_or(false);
    if is_whisper {
        rows.push(entry(
            "Language",
            "Transcription language for whisper. Auto-detect, or pin one.",
            language_control(modal, cx),
        ));
    } else {
        let langs = spec_for(&modal.dictation.model_id)
            .map(|s| s.langs)
            .unwrap_or("Fixed language set.");
        rows.push(entry(
            "Language",
            "This model decodes a fixed language set (no selection).",
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(langs))
                .into_any_element(),
        ));
    }

    // Silence trimming via Silero VAD.
    let vad = toggle_switch(
        "voice-vad",
        modal.dictation.vad_enabled,
        theme,
        |this, _w, cx| {
            this.dictation.vad_enabled = !this.dictation.vad_enabled;
            this.persist_voice(cx);
        },
        cx,
    );
    rows.push(entry(
        "Trim silence (VAD)",
        "Detect speech and drop silent gaps before transcribing — cleaner results, \
         fewer phantom captions. Downloads a small model on first use.",
        vad,
    ));

    // Clean fillers / stutters / whisper hallucinations from the transcript.
    let filler = toggle_switch(
        "voice-filler",
        modal.dictation.filler_filter_enabled,
        theme,
        |this, _w, cx| {
            this.dictation.filler_filter_enabled = !this.dictation.filler_filter_enabled;
            this.persist_voice(cx);
        },
        cx,
    );
    rows.push(entry(
        "Clean transcript",
        "Remove filler words (um, uh, ừ), stutters, and phantom captions like \
         \"(sad music)\". Filler words are language-aware.",
        filler,
    ));

    // Custom words: a comma/space-separated dictionary the transcript is
    // fuzzy-corrected toward. The InputState lives on the modal (built on open);
    // its Change subscription parses + persists.
    if let Some(state) = modal.custom_words_input.as_ref() {
        rows.push(entry(
            "Custom words",
            "Brand, command, or proper names to correct toward (comma-separated).",
            Input::new(state)
                .small()
                .text_size(px(typography.t_body_sm))
                .into_any_element(),
        ));
    }

    // Insertion behaviour: a separator after the inserted transcript.
    let trailing = toggle_switch(
        "voice-trailing-space",
        modal.dictation.append_trailing_space,
        theme,
        |this, _w, cx| {
            this.dictation.append_trailing_space = !this.dictation.append_trailing_space;
            this.persist_voice(cx);
        },
        cx,
    );
    rows.push(entry(
        "Append trailing space",
        "Leave a space after each inserted transcript, so back-to-back dictations \
         don't run together.",
        trailing,
    ));

    // Auto-submit: composer-only, and off by default (it sends for you).
    let auto_submit = toggle_switch(
        "voice-auto-submit",
        modal.dictation.auto_submit,
        theme,
        |this, _w, cx| {
            this.dictation.auto_submit = !this.dictation.auto_submit;
            this.persist_voice(cx);
        },
        cx,
    );
    rows.push(entry(
        "Send after dictation",
        "Send the chat message as soon as a transcript lands in the composer. \
         Only applies to the composer — never the terminal or editor.",
        auto_submit,
    ));

    // Audible start/stop cue.
    let sound = toggle_switch(
        "voice-sound",
        modal.dictation.audio_feedback_enabled,
        theme,
        |this, _w, cx| {
            this.dictation.audio_feedback_enabled = !this.dictation.audio_feedback_enabled;
            this.persist_voice(cx);
        },
        cx,
    );
    rows.push(entry(
        "Sound feedback",
        "Play a short system sound when recording starts and stops.",
        sound,
    ));

    // Warm-engine lifetime — a memory/latency trade, so it sits with the
    // advanced controls rather than the everyday ones.
    rows.push(entry(
        "Unload model after",
        "How long the speech model stays in memory after a dictation. Shorter \
         frees memory sooner; longer keeps the next dictation instant.",
        unload_control(modal, cx),
    ));

    // Microphone permission status + action.
    rows.push(entry(
        "Microphone",
        "Voice dictation needs microphone access.",
        permission_control(theme, density, typography, cx),
    ));

    rows
}

/// The idle-teardown picker: a dropdown labeled with the active policy, opening
/// the full [`ModelUnloadTimeout`] set. Mirrors the language picker's shape.
fn unload_control(modal: &SettingsModal, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    let entity = cx.entity();
    let current = modal.dictation.model_unload_timeout;
    DropdownButton::new("voice-unload")
        .button(
            Button::new("voice-unload-btn")
                .label(current.label())
                .small()
                .outline(),
        )
        .small()
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            for opt in ModelUnloadTimeout::ALL {
                menu = menu.item(unload_item(window, &entity, *opt, *opt == current));
            }
            menu
        })
        .into_any_element()
}

/// One idle-teardown row: label + right-aligned check on the active policy.
fn unload_item(
    window: &mut Window,
    entity: &Entity<SettingsModal>,
    opt: ModelUnloadTimeout,
    selected: bool,
) -> PopupMenuItem {
    PopupMenuItem::element(move |_window, _cx| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(28.0))
            .min_w(px(180.0))
            .child(div().child(opt.label()))
            .child(div().w(px(16.0)).flex_none().flex().justify_center().when(
                selected,
                |d| d.child(Icon::default().path("icons/check.svg").size(px(14.0))),
            ))
    })
    .on_click(window.listener_for(
        entity,
        move |m: &mut SettingsModal, _ev: &ClickEvent, _window, cx| {
            m.dictation.model_unload_timeout = opt;
            m.persist_voice(cx);
        },
    ))
}

fn set_language(this: &mut SettingsModal, lang: &str, cx: &mut gpui::Context<SettingsModal>) {
    this.dictation.language = lang.to_string();
    this.persist_voice(cx);
}

fn set_mode(this: &mut SettingsModal, mode: DictationMode, cx: &mut gpui::Context<SettingsModal>) {
    this.dictation.mode = mode;
    this.persist_voice(cx);
}

/// The input-device picker: a dropdown button labeled by the current device
/// ("System default" when unset), opening a menu of "System default" plus each
/// enumerated microphone. Picking a row writes `input_device` and persists. A
/// dropdown (not a wrapping chip group) keeps the row compact regardless of how
/// many devices are attached or how long their names are.
fn device_control(modal: &SettingsModal, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    let entity = cx.entity();
    let current = modal.dictation.device_name();
    let label = current
        .clone()
        .unwrap_or_else(|| "System default".to_string());
    DropdownButton::new("voice-device")
        .button(Button::new("voice-device-btn").label(label).small().outline())
        .small()
        // Enumerate devices only when the menu opens (a CoreAudio HAL call);
        // the trigger label needs only the persisted setting, not the scan.
        // Layout mirrors the ChatGPT desktop mic menu: "System default" first,
        // a divider, then each device — the active one trailing a right check.
        // `TopRight` right-aligns the menu under the button (grows down-and-left)
        // so it never overflows off the pane's right edge.
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            menu = menu.item(device_item(
                window,
                &entity,
                None,
                "System default".to_string(),
                current.is_none(),
            ));
            menu = menu.separator();
            for name in list_input_devices() {
                let selected = current.as_deref() == Some(name.as_str());
                menu = menu.item(device_item(window, &entity, Some(name.clone()), name, selected));
            }
            menu
        })
        .into_any_element()
}

/// One device row in the mic dropdown — a label with a trailing check on the
/// selected device (ChatGPT-style, check on the RIGHT), writing `input_device`
/// (None = system default) on click. A `min_w` keeps the check right-aligned
/// even for the short "System default" label.
fn device_item(
    window: &mut Window,
    entity: &Entity<SettingsModal>,
    device: Option<String>,
    label: String,
    selected: bool,
) -> PopupMenuItem {
    PopupMenuItem::element(move |_window, _cx| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(28.0))
            .min_w(px(210.0))
            .child(div().child(label.clone()))
            .child(div().w(px(16.0)).flex_none().flex().justify_center().when(
                selected,
                |d| d.child(Icon::default().path("icons/check.svg").size(px(14.0))),
            ))
    })
    .on_click(window.listener_for(
        entity,
        move |m: &mut SettingsModal, _ev: &ClickEvent, _window, cx| {
            m.dictation.input_device = device.clone();
            m.persist_voice(cx);
        },
    ))
}

/// The language picker (whisper models only): a dropdown button labeled by the
/// active language's display name, opening a scrollable menu of the full whisper
/// language set — `Auto-detect` first, then a common cluster (Vietnamese-first),
/// then the rest alphabetically. A scrollable menu (no search box) keeps the
/// ~99-item list compact without adding a filter-input entity. Picking a row
/// writes `language` and persists via the settings watcher.
fn language_control(modal: &SettingsModal, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    let entity = cx.entity();
    let current = modal.dictation.language.clone();
    let label = language_display_name(&current).to_string();
    DropdownButton::new("voice-lang")
        .button(Button::new("voice-lang-btn").label(label).small().outline())
        .small()
        // `TopRight` right-aligns the menu under the button; `scrollable` + a
        // capped height keep the long language list on-screen.
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            menu = menu.scrollable(true).max_h(px(320.0));
            for (code, name) in WHISPER_LANGUAGES {
                let selected = current == *code;
                menu = menu.item(language_item(window, &entity, code, name, selected));
            }
            menu
        })
        .into_any_element()
}

/// One language row — a display name with a trailing check on the active
/// language (right-aligned, ChatGPT-style), writing `language` on click. The
/// `code` is `&'static` so the click closure needs no owned clone.
fn language_item(
    window: &mut Window,
    entity: &Entity<SettingsModal>,
    code: &'static str,
    name: &'static str,
    selected: bool,
) -> PopupMenuItem {
    PopupMenuItem::element(move |_window, _cx| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(28.0))
            .min_w(px(200.0))
            .child(div().child(name))
            .child(div().w(px(16.0)).flex_none().flex().justify_center().when(
                selected,
                |d| d.child(Icon::default().path("icons/check.svg").size(px(14.0))),
            ))
    })
    .on_click(window.listener_for(
        entity,
        move |m: &mut SettingsModal, _ev: &ClickEvent, _window, cx| {
            set_language(m, code, cx);
        },
    ))
}

/// The Speech-model picker: a dropdown button labeled by the active model,
/// opening a menu of every catalog model. Each menu row shows the model name +
/// a `recommended` badge on the default, its download state (size / progress /
/// "Download") and coverage blurb, with a check on the active model and a trash
/// affordance on downloaded ones. Clicking a downloaded model selects it;
/// clicking one that isn't downloaded selects AND starts its download, so it
/// activates the moment it's ready. Collapsing the catalog into one row keeps
/// the pane compact no matter how many models ship.
fn model_control(
    modal: &SettingsModal,
    theme: Theme,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let entity = cx.entity();
    let current_id = modal.dictation.model_id.clone();
    let label = spec_for(&current_id)
        .map(|s| s.label.to_string())
        .unwrap_or_else(|| "Select model".to_string());
    let typo = typography.clone();
    // Which model the badge points at depends on the pinned language: a pinned
    // `vi`/`en` gets steered to the dedicated (much more accurate) model, while
    // `auto` keeps the multilingual default. Computed per menu build — changing
    // the language closes this menu, so it can never go stale while open.
    let recommended = recommended_model_id(&modal.dictation.language);
    DropdownButton::new("voice-model")
        .button(Button::new("voice-model-btn").label(label).small().outline())
        .small()
        // Build rows when the menu opens: `model_status` is pulled per row so the
        // Each row pulls `model_status` live inside its render closure (not a
        // snapshot here), so an open menu reflects download progress + delete in
        // real time as the model-download drain repaints the window. `TopRight`
        // right-aligns the menu under the button (grows down-and-left) so its
        // wide rows never overflow off the pane's right edge.
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            menu = menu.max_w(px(420.0));
            for spec in catalog() {
                let selected = current_id == spec.id;
                menu = menu.item(model_item(
                    window,
                    &entity,
                    spec,
                    selected,
                    spec.id == recommended,
                    theme,
                    &typo,
                ));
            }
            menu
        })
        .into_any_element()
}

/// One rich model row in the Speech-model dropdown: a two-line cell (name +
/// badge + status on top, coverage blurb below) with a leading check on the
/// active model and a trailing trash on downloaded ones. The row's `on_click`
/// selects-or-downloads; the trash intercepts its own click (via
/// `stop_propagation`) so deleting never also selects.
fn model_item(
    window: &mut Window,
    entity: &Entity<SettingsModal>,
    spec: &'static ModelSpec,
    selected: bool,
    recommended: bool,
    theme: Theme,
    typography: &Typography,
) -> PopupMenuItem {
    let id = spec.id.to_string();
    let label = spec.label.to_string();
    let langs = spec.langs.to_string();
    let typo = typography.clone();
    let del_entity = entity.clone();

    PopupMenuItem::element(move |_window, cx| {
        // Pull status EVERY paint (not a snapshot at menu-open) so download
        // progress and post-delete state update live in the open menu — the
        // model-download drain's `refresh_windows` repaints this row each tick.
        let status = model_status(cx, spec.id);
        let is_ready = matches!(status, ModelStatus::Ready);
        let (status_text, status_color) = match &status {
            ModelStatus::Ready => (format!("{} MB", spec.size_mb), theme.fg_subtle),
            ModelStatus::NotDownloaded => {
                (format!("Download · {} MB", spec.size_mb), theme.status_info)
            }
            ModelStatus::Downloading(p) => {
                (format!("Downloading… {}%", (p * 100.0).round() as u32), theme.status_info)
            }
            ModelStatus::Extracting => ("Installing…".to_string(), theme.status_info),
            ModelStatus::Failed(_) => ("Failed · retry".to_string(), theme.status_error),
        };

        let mut line1 = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .child(
                div()
                    .flex_none()
                    .w(px(16.0))
                    .flex()
                    .justify_center()
                    .when(selected, |d| {
                        d.child(Icon::default().path("icons/check.svg").size(px(13.0)))
                    }),
            )
            .child(
                div()
                    .font_weight(typo.w_semibold)
                    .text_color(theme.fg_base)
                    .child(label.clone()),
            );
        if recommended {
            line1 = line1.child(recommended_badge(theme, &typo));
        }
        line1 = line1.child(div().flex_1()).child(
            div()
                .text_size(px(typo.t_sub_label))
                .text_color(status_color)
                .child(SharedString::from(status_text.clone())),
        );
        if is_ready {
            let del_entity = del_entity.clone();
            line1 = line1.child(
                div()
                    .id(SharedString::from(format!("voice-model-del-{}", spec.id)))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(20.0))
                    .rounded(px(5.0))
                    .text_color(theme.fg_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover_overlay).text_color(theme.status_error))
                    // Consume the press so the row's select handler never fires.
                    .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                        cx.stop_propagation();
                        del_entity.update(cx, |m, cx| {
                            delete_model(cx, spec.id);
                            // Deleting the active model falls back to the default;
                            // the mic then gates on that model's readiness.
                            if m.dictation.model_id.as_str() == spec.id {
                                m.dictation.model_id = DEFAULT_MODEL_ID.to_string();
                                m.persist_voice(cx);
                            }
                            cx.notify();
                            // Repaint so the row reflects the delete immediately in
                            // the still-open menu (the element pulls status live).
                            cx.refresh_windows();
                        });
                    })
                    .child(Icon::default().path("icons/trash.svg").size(px(13.0))),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .min_w(px(340.0))
            .py(px(3.0))
            .child(line1)
            .child(
                div()
                    .pl(px(24.0))
                    .text_size(px(typo.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .child(langs.clone()),
            )
    })
    .on_click(window.listener_for(
        entity,
        move |m: &mut SettingsModal, _ev: &ClickEvent, _window, cx| {
            // Clicking a downloading model cancels it (the `.partial` is kept for
            // a later resume); otherwise select it, kicking off a download when
            // it isn't ready so it activates the moment it lands (the mic gates
            // on readiness in the meantime).
            if matches!(model_status(cx, &id), ModelStatus::Downloading(_)) {
                cancel_model_download(cx, &id);
                cx.notify();
                return;
            }
            m.dictation.model_id = id.clone();
            m.persist_voice(cx);
            if !matches!(model_status(cx, &id), ModelStatus::Ready) {
                download_model(cx, &id);
            }
            cx.notify();
        },
    ))
}

/// A small green "recommended" pill for the default model row.
fn recommended_badge(theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(theme.status_ok.opacity(0.15))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.status_ok)
        .child("recommended")
        .into_any_element()
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
