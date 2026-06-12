//! Editable keybindings pane — every registry action grouped by category,
//! with record-a-chord editing, conflict badges, and per-row + global
//! reset. The registry inventory is the single source of truth; this pane
//! renders its effective map (defaults ⊕ the modal's working overrides).
//!
//! Recording uses a gpui keystroke interceptor (fires before action
//! dispatch + key listeners), so pressing a chord that is currently bound
//! records it instead of firing the action. Esc cancels; Backspace or
//! Delete unbinds.

use gpui::{AnyElement, Focusable as _, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::value_chip;
use super::layout::{SettingEntry, entries_card, entry};
use crate::keymap_registry::{self, ActionSpec, Category};

impl SettingsModal {
    /// Begin capturing a chord for `id`. Focus moves to the modal's own
    /// handle so the search input can't swallow typed characters, and an
    /// app-wide interceptor claims every keystroke until the capture ends.
    fn start_recording(
        &mut self,
        id: &'static str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.recording_action = Some(id);
        window.focus(&self.focus_handle, cx);
        let weak = cx.weak_entity();
        self.recording_sub = Some(cx.intercept_keystrokes(move |ev, window, cx| {
            cx.stop_propagation();
            let keystroke = ev.keystroke.clone();
            if let Some(modal) = weak.upgrade() {
                modal.update(cx, |this, cx| this.finish_recording(&keystroke, window, cx));
            }
        }));
        cx.notify();
    }

    /// Resolve one captured keystroke: Esc cancels, Backspace/Delete (bare)
    /// unbinds, anything else becomes the new chord. Always ends recording
    /// and hands focus back to the search field (recording stole it).
    fn finish_recording(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(id) = self.recording_action.take() else {
            self.recording_sub = None;
            return;
        };
        self.recording_sub = None;
        let bare = keystroke.modifiers.number_of_modifiers() == 0;
        match keystroke.key.as_str() {
            "escape" if bare => {}
            "backspace" | "delete" if bare => self.set_keybinding(id, None, cx),
            _ => self.set_keybinding(id, Some(keystroke.unparse()), cx),
        }
        if let Some(input) = &self.search_input {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// Set (or with `None`, unbind) the chord for `id`, then persist and
    /// rebind live. A chord equal to the default removes the override line.
    fn set_keybinding(
        &mut self,
        id: &'static str,
        chord: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(spec) = keymap_registry::spec(id) else {
            return;
        };
        let default = keymap_registry::normalize_chord(spec.default_chord);
        match chord {
            Some(raw) => {
                // A raw chord that fails normalization is dropped, not
                // applied — possible only if the OS reports a key name
                // outside the registry's allowlist (e.g. an exotic media
                // key), never from ordinary recorder input.
                let Some(normalized) = keymap_registry::normalize_chord(&raw) else {
                    tracing::debug!(%raw, id, "recorded chord failed normalization; ignored");
                    return;
                };
                if Some(&normalized) == default.as_ref() {
                    self.keybind_overrides.remove(id);
                } else {
                    self.keybind_overrides.insert(id.to_string(), normalized);
                }
            }
            // Unbind: a default-unbound action needs no override line.
            None if default.is_none() => {
                self.keybind_overrides.remove(id);
            }
            None => {
                self.keybind_overrides.insert(id.to_string(), String::new());
            }
        }
        self.persist_keybinds(cx);
    }

    fn reset_keybinding(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        self.keybind_overrides.remove(id);
        self.persist_keybinds(cx);
    }

    fn reset_all_keybindings(&mut self, cx: &mut gpui::Context<Self>) {
        self.keybind_overrides.clear();
        self.persist_keybinds(cx);
    }

    /// Write `keybindings.toml` and rebind the live keymap. A write error
    /// is toasted but the live rebind still proceeds, so the session keeps
    /// matching what the pane shows; disk catches up on the next save.
    fn persist_keybinds(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(err) = crate::keybindings_settings::save_overrides(&self.keybind_overrides) {
            crate::shell::toast::toast_op_error(cx, "Save keybindings", &err.to_string());
        }
        let warnings = keymap_registry::apply_live(cx, &self.keybind_overrides);
        for warning in warnings {
            tracing::warn!(%warning, "keybinding override problem");
        }
        cx.notify();
    }
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let outcome = keymap_registry::resolve(&modal.keybind_overrides);
    let conflicts = keymap_registry::conflicting_chords(&outcome.effective);

    let mut col = div().flex().flex_col();
    for category in Category::ALL {
        let rows: Vec<SettingEntry> = keymap_registry::ACTIONS
            .iter()
            .enumerate()
            .filter(|(_, s)| s.category == category)
            .map(|(idx, spec)| binding_entry(modal, spec, idx, &outcome, &conflicts, theme, density, typography, cx))
            .collect();
        col = col
            .child(
                div()
                    .pt(px(14.0))
                    .pb(px(2.0))
                    .text_size(px(typography.t_sub_label))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_subtle)
                    .child(category.label()),
            )
            .child(entries_card(theme, density, typography, rows));
    }

    col.child(footer(modal, theme, density, typography, cx))
        .into_any_element()
}

/// Keybindings as searchable entries for the global settings finder.
pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let outcome = keymap_registry::resolve(&modal.keybind_overrides);
    let conflicts = keymap_registry::conflicting_chords(&outcome.effective);
    keymap_registry::ACTIONS
        .iter()
        .enumerate()
        .map(|(idx, spec)| binding_entry(modal, spec, idx, &outcome, &conflicts, theme, density, typography, cx))
        .collect()
}

/// One binding row: label + id on the left; conflict badge, reset (when
/// overridden), and the chord chip on the right. Clicking the chord chip
/// starts recording.
#[allow(clippy::too_many_arguments)]
fn binding_entry(
    modal: &SettingsModal,
    spec: &'static ActionSpec,
    idx: usize,
    outcome: &keymap_registry::ResolveOutcome,
    conflicts: &std::collections::HashSet<String>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> SettingEntry {
    let chord = outcome.effective.get(spec.id).cloned().flatten();
    let recording = modal.recording_action == Some(spec.id);
    let overridden = modal.keybind_overrides.contains_key(spec.id);
    let conflicted = chord.as_deref().is_some_and(|c| conflicts.contains(c));

    let chip_text = if recording {
        "Press keys…".to_string()
    } else {
        chord
            .as_deref()
            .map(keymap_registry::format_chord)
            .unwrap_or_else(|| "—".to_string())
    };

    let mut control = div().flex().flex_row().items_center().gap(px(8.0));
    if conflicted {
        control = control.child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.status_error)
                .child("conflict"),
        );
    }
    if overridden && !recording {
        let id = spec.id;
        control = control.child(value_chip(
            ("kb-reset", idx),
            "Reset",
            theme,
            density,
            typography,
            move |this, _w, cx| this.reset_keybinding(id, cx),
            cx,
        ));
    }
    control = control.child(value_chip(
        ("kb-chord", idx),
        chip_text,
        theme,
        density,
        typography,
        move |this, window, cx| this.start_recording(spec.id, window, cx),
        cx,
    ));

    entry(
        spec.label,
        format!("keybindings.toml: {}", spec.id),
        control,
    )
}

fn footer(
    _modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .pt(px(16.0))
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Click a shortcut, then press the new keys. Esc cancels, ⌫ unbinds."),
        )
        .child(value_chip(
            "kb-reset-all",
            "Reset all to defaults",
            theme,
            density,
            typography,
            |this, _w, cx| this.reset_all_keybindings(cx),
            cx,
        ))
        .into_any_element()
}
