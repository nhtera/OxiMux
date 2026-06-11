//! Notifications pane — master switch, per-source + per-kind banner
//! toggles, sound, focus gate, agent-awake, and a test button that posts
//! through the real OS pipeline. Toggles flip the shared
//! [`AgentNotifySettings`] atomics (effective immediately) and persist
//! via the flat settings store.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry};
use crate::notifier::{
    AgentNotifySettings, NotificationKind, NotificationRequest, NotificationSource,
    NotifierAvailability, TabId, keys,
};

/// Toggle rows: label, description, settings key, and a selector resolving
/// the matching atomic in [`AgentNotifySettings`]. Driven as data so the
/// rows stay in sync with the struct without near-identical blocks.
type NotifySelect = fn(&AgentNotifySettings) -> &AtomicBool;
const NOTIFY_ROWS: [(&str, &str, &str, NotifySelect); 9] = [
    (
        "Enable notifications",
        "Master switch for every desktop banner OxiMux posts.",
        keys::ENABLED,
        |s| &s.enabled,
    ),
    (
        "Agent status banners",
        "Banner when an agent needs attention or finishes.",
        keys::SOURCE_AGENT_STATE,
        |s| &s.source_agent_state,
    ),
    (
        "Approval needed",
        "Notify when an agent pauses for a dangerous-action approval.",
        keys::NEEDS_APPROVAL,
        |s| &s.needs_approval,
    ),
    (
        "Waiting for input",
        "Notify when an agent pauses waiting for a reply.",
        keys::WAITING_INPUT,
        |s| &s.waiting_input,
    ),
    (
        "Agent finished",
        "Notify when an agent completes successfully.",
        keys::DONE,
        |s| &s.done,
    ),
    (
        "Agent failed",
        "Notify when an agent exits with an error.",
        keys::FAILED,
        |s| &s.failed,
    ),
    (
        "Play sound",
        "Play the system notification sound with each banner.",
        keys::SOUND,
        |s| &s.sound,
    ),
    (
        "Only when unfocused",
        "Suppress notifications while the OxiMux window is focused.",
        keys::ONLY_WHEN_UNFOCUSED,
        |s| &s.only_when_unfocused,
    ),
    (
        "Keep Mac awake while agents run",
        "Prevent idle sleep while any agent session is running.",
        keys::AGENT_AWAKE,
        |s| &s.agent_awake,
    ),
];

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
                .child(availability_hint(modal.notifier.availability())),
        )
        .into_any_element()
}

pub(super) fn entries(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let mut entries: Vec<SettingEntry> = NOTIFY_ROWS
        .iter()
        .enumerate()
        .map(|(idx, (label, description, key, select))| {
            entry(
                *label,
                *description,
                notify_toggle(idx, key, *select, modal, theme, cx),
            )
        })
        .collect();

    entries.push(entry(
        "Test notification",
        "Post a sample banner through the OS pipeline.",
        value_chip(
            "notify-test",
            "Send".to_string(),
            theme,
            density,
            typography,
            send_test_notification,
            cx,
        ),
    ));
    entries
}

/// One notification-pref toggle. Reads the live atomic for its current value;
/// clicking flips the atomic (effective immediately) and persists the new
/// value so it survives a restart. The agent-awake row additionally pushes
/// the new state into the process-global assertion holder.
fn notify_toggle(
    idx: usize,
    key: &'static str,
    select: NotifySelect,
    modal: &SettingsModal,
    theme: Theme,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let current = select(&modal.notify).load(Ordering::Relaxed);
    toggle_switch(
        ("notify-toggle", idx),
        current,
        theme,
        move |this, _w, cx| {
            let flag = select(&this.notify);
            let next = !flag.load(Ordering::Relaxed);
            flag.store(next, Ordering::Relaxed);
            if key == keys::AGENT_AWAKE {
                crate::agent_awake::global().set_enabled(next);
            }
            this.persist_notify(key, next, cx);
        },
        cx,
    )
}

/// Test-button click: post through the real pipeline when it can deliver,
/// otherwise explain why it can't in a toast.
fn send_test_notification(
    this: &mut SettingsModal,
    _w: &mut gpui::Window,
    cx: &mut gpui::Context<SettingsModal>,
) {
    use crate::shell::toast::{ToastKind, toast};
    match this.notifier.availability() {
        NotifierAvailability::Available => {
            this.notifier.notify(NotificationRequest {
                source: NotificationSource::Test,
                kind: NotificationKind::Done,
                tab_id: TabId(0),
                workspace_key: "test".to_string(),
                label: "Test".to_string(),
                body: "Notifications are working.".to_string(),
                window_active: false,
                pane_visible: false,
            });
            toast(cx, ToastKind::Success, "Test notification posted");
        }
        NotifierAvailability::Unbundled => toast(
            cx,
            ToastKind::Error,
            "Notifications need the bundled app — build with scripts/bundle-macos.sh",
        ),
        NotifierAvailability::PermissionDenied => toast(
            cx,
            ToastKind::Error,
            "Notifications are denied — allow OxiMux in System Settings → Notifications",
        ),
    }
}

fn availability_hint(availability: NotifierAvailability) -> &'static str {
    match availability {
        NotifierAvailability::Available => {
            "Banners post through Notification Center. A visible pane never banners."
        }
        NotifierAvailability::Unbundled => {
            "Running unbundled — banners are disabled. Build the .app with scripts/bundle-macos.sh."
        }
        NotifierAvailability::PermissionDenied => {
            "Notification permission denied. Allow OxiMux in System Settings → Notifications."
        }
    }
}
