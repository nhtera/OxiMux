//! Remote-control pane — one master switch that turns on exposing running agent
//! sessions to the OxiMux mobile app. The toggle flips the shared
//! [`RemoteControl`] global's `enabled` flag (effective immediately: gated
//! registration + the per-event tee in the chat view start/stop from it).
//!
//! Session-scoped for now — enabling is not persisted across restarts, and pairing
//! a device (QR) + the live host bind land in a later step; until then this is the
//! gate that makes running sessions register into the in-app registry.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::toggle_switch;
use super::layout::{SettingEntry, entries_card, entry};
use crate::remote_control::RemoteControl;

/// Is remote control currently enabled? `false` if the global is somehow absent
/// (so the pane renders rather than panicking).
fn enabled(cx: &mut gpui::Context<SettingsModal>) -> bool {
    cx.try_global::<RemoteControl>().is_some_and(|rc| rc.enabled())
}

/// How many agent sessions are exposed right now (a snapshot at render time).
fn exposed_count(cx: &mut gpui::Context<SettingsModal>) -> usize {
    cx.try_global::<RemoteControl>().map(|rc| rc.registry().len()).unwrap_or(0)
}

/// The status line under the toggle: what enabling does (off), or how many
/// sessions are exposed right now (on). Pure so it can be unit-tested.
fn hint_text(enabled: bool, exposed: usize) -> String {
    if !enabled {
        return "Turn on to expose your running agent sessions to the OxiMux mobile app. \
                Applies to sessions started after it's enabled."
            .to_string();
    }
    match exposed {
        0 => "Remote access is on. No agent sessions are running yet — start one and it will \
              be exposed."
            .to_string(),
        1 => "Remote access is on. 1 running agent session is exposed.".to_string(),
        n => format!("Remote access is on. {n} running agent sessions are exposed."),
    }
}

pub(super) fn render(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let hint = hint_text(enabled(cx), exposed_count(cx));

    div()
        .flex()
        .flex_col()
        .child(entries_card(theme, density, typography, entries(modal, theme, density, typography, cx)))
        .child(
            div()
                .pt(px(12.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(hint),
        )
        .into_any_element()
}

pub(super) fn entries(
    _modal: &SettingsModal,
    theme: Theme,
    _density: Density,
    _typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![entry(
        "Allow remote access",
        "Expose running agent sessions so the OxiMux mobile app can view and drive them.",
        remote_toggle(theme, cx),
    )]
}

/// The master switch. Reads the live global flag; clicking flips it (effective
/// immediately — new sessions register/tee from it) and repaints.
fn remote_toggle(theme: Theme, cx: &mut gpui::Context<SettingsModal>) -> AnyElement {
    toggle_switch(
        "remote-enabled",
        enabled(cx),
        theme,
        |_this, _window, cx| {
            if let Some(rc) = cx.try_global::<RemoteControl>() {
                rc.set_enabled(!rc.enabled());
            }
            cx.notify();
        },
        cx,
    )
}

#[cfg(test)]
mod tests {
    use super::hint_text;

    #[test]
    fn hint_reflects_enabled_state_and_count() {
        assert!(hint_text(false, 3).starts_with("Turn on"), "disabled explains what enabling does");
        assert!(hint_text(true, 0).contains("No agent sessions"), "on with none running");
        assert_eq!(
            hint_text(true, 1),
            "Remote access is on. 1 running agent session is exposed.",
            "singular",
        );
        assert!(hint_text(true, 4).contains("4 running agent sessions"), "plural count");
    }
}
