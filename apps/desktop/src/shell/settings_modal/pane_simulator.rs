//! Mobile Emulator pane (Beta): the feature switch, whether this Mac can run
//! iOS simulators and Android emulators ([`availability`]), the default
//! device, then sections for agents (with [`commands`]), approved devices,
//! the stream and the rest.
//!
//! Every value lives in `simulator.toml` ([`SimulatorSettings`]): an edit here
//! writes the file and installs the global at once, so the tab, auto-open, the
//! agent gate and the stream apply it live. The approved devices are the
//! exception — they live in the app's database, and this pane only reads and
//! revokes them, through the hub so the grant in memory goes too. Granting
//! happens only in the panel's consent banner.

use gpui::{Anchor, AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Subscription, div, prelude::FluentBuilder, px};
use gpui_component::button::Button;
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::Sizable as _;
use oximux_settings::{Density, Theme, Typography};
use oximux_simulator::DeviceId;

mod availability;
mod commands;

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry, entry_row, section_card, section_title};
use super::segmented::{Segment, segmented};
use crate::app_settings::simulator_settings::{self, ALLOWED_FPS, Resolution, SimulatorSettings};
use crate::shell::simulator::{HubEvent, SimulatorHub, hub};

/// The agent guide, as published with the source.
const AGENT_GUIDE_URL: &str = "https://github.com/nhtera/OxiMux/blob/main/docs/skills/oximux-simulator.md";

/// Idle-shutdown choices, in minutes (`0`: never).
const IDLE_CHOICES: [(u32, &str); 4] = [(5, "5 min"), (10, "10 min"), (30, "30 min"), (0, "Never")];

/// Repaint the pane when the hub's availability, device list or approvals
/// change (the pane reads them from the hub, never the disk).
pub(super) fn watch_hub(cx: &mut Context<SettingsModal>) -> Option<Subscription> {
    let hub = hub(cx)?;
    Some(cx.subscribe(&hub, |_, _, event: &HubEvent, cx| {
        if matches!(event, HubEvent::Availability | HubEvent::Devices | HubEvent::Consent) {
            cx.notify();
        }
    }))
}

pub(super) fn render(
    _modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let s = crate::shell::simulator::panel::settings(cx);
    let hub = hub(cx);
    let first = vec![
        entry_row(enable_entry(&s, theme, cx), theme, typography),
        availability::block(s.android_sdk.as_deref(), theme, density, typography, cx),
        entry_row(default_device_entry(hub.as_ref(), s.default_device.as_deref(), cx), theme, typography),
    ];
    let approvals = approval_rows(theme, density, typography, cx);
    div()
        .flex()
        .flex_col()
        .gap(px(16.0))
        .child(beta_note(theme, density, typography))
        .child(section_card(theme, density, first))
        .child(section_title("Agents", "How coding agents open, find and drive a device.", theme, typography))
        .child(entries_card(theme, density, typography, agent_entries(&s, theme, density, typography, cx)))
        .child(section_title(
            "Approved devices",
            if approvals.is_empty() {
                "None yet. Choosing Allow when an agent asks adds the device here."
            } else {
                "Agents may drive these without asking. Revoke one to be asked again."
            },
            theme,
            typography,
        ))
        // No card at all when empty: it would draw a bare frame under the line.
        .when(!approvals.is_empty(), |d| d.child(entries_card(theme, density, typography, approvals)))
        .child(section_title("Stream", "The picture in the panel.", theme, typography))
        .child(entries_card(theme, density, typography, stream_entries(&s, theme, density, typography, cx)))
        .child(section_title("Advanced", "", theme, typography))
        .child(entries_card(theme, density, typography, advanced_entries(&s, hub.as_ref(), theme, density, typography, cx)))
        .into_any_element()
}

/// Every row of the pane, for the settings search: the platform lines of the
/// Availability card are rows of their own here.
// Only the settings search reads this, and it lists the pane only where the
// pane can run (Apple silicon).
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub(super) fn entries(
    _modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let s = crate::shell::simulator::panel::settings(cx);
    let hub = hub(cx);
    let mut rows = vec![enable_entry(&s, theme, cx)];
    rows.extend(availability::platform_entries(s.android_sdk.as_deref(), theme, density, typography, cx));
    rows.push(default_device_entry(hub.as_ref(), s.default_device.as_deref(), cx));
    rows.extend(agent_entries(&s, theme, density, typography, cx));
    rows.extend(stream_entries(&s, theme, density, typography, cx));
    rows.extend(advanced_entries(&s, hub.as_ref(), theme, density, typography, cx));
    rows
}

fn enable_entry(s: &SimulatorSettings, theme: Theme, cx: &mut Context<SettingsModal>) -> SettingEntry {
    entry(
        "Enable Mobile Emulator",
        "Shows the Mobile Emulator tab in the right sidebar and lets agents attach to a device.",
        toggle_switch("sim-enabled", s.enabled, theme, |_, _, cx| change(cx, |s| s.enabled = !s.enabled), cx),
    )
}

fn default_device_entry(hub: Option<&Entity<SimulatorHub>>, current: Option<&str>, cx: &mut Context<SettingsModal>) -> SettingEntry {
    entry(
        "Default device",
        "The device a worktree gets when it has none yet. Automatic prefers one already running.",
        device_menu(hub, current, cx),
    )
}

fn agent_entries(
    s: &SimulatorSettings,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![
        entry(
            "Open automatically",
            "Show the panel when an agent builds, boots or drives a device.",
            toggle_switch("sim-auto-open", s.auto_open, theme, |_, _, cx| change(cx, |s| s.auto_open = !s.auto_open), cx),
        ),
        entry(
            "Agent control",
            "Let agents drive a device with `oximux sim`. Each device still asks you first.",
            toggle_switch(
                "sim-agent-control",
                s.agent_control,
                theme,
                |_, _, cx| change(cx, |s| s.agent_control = !s.agent_control),
                cx,
            ),
        ),
        commands::entry(theme, density, typography, cx),
        entry(
            "What agents can do",
            "The guide agents read: the `oximux sim` verbs and how consent works.",
            value_chip(
                "sim-guide",
                "Open",
                theme,
                density,
                typography,
                |_, _, cx| crate::shell::open_url::open_url(AGENT_GUIDE_URL, cx),
                cx,
            ),
        ),
    ]
}

fn stream_entries(
    s: &SimulatorSettings,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![
        entry(
            "Frame rate",
            "How often the panel's picture refreshes.",
            segmented(
                "sim-fps",
                ALLOWED_FPS
                    .iter()
                    .map(|&fps| Segment::new(format!("{fps}"), s.stream.fps == fps, move |_, _, cx| change(cx, |s| s.stream.fps = fps)))
                    .collect(),
                theme,
                density,
                typography,
                cx,
            ),
        ),
        entry(
            "Resolution",
            "Half costs less to stream; Full is sharper.",
            segmented(
                "sim-resolution",
                [(Resolution::Half, "Half"), (Resolution::Full, "Full")]
                    .into_iter()
                    .map(|(res, label)| {
                        Segment::new(label, s.stream.resolution == res, move |_, _, cx| change(cx, |s| s.stream.resolution = res))
                    })
                    .collect(),
                theme,
                density,
                typography,
                cx,
            ),
        ),
        entry(
            "Show frame rate",
            "A live FPS readout in the panel's toolbar.",
            toggle_switch(
                "sim-show-fps",
                s.stream.show_fps,
                theme,
                |_, _, cx| change(cx, |s| s.stream.show_fps = !s.stream.show_fps),
                cx,
            ),
        ),
    ]
}

fn advanced_entries(
    s: &SimulatorSettings,
    hub: Option<&Entity<SimulatorHub>>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    vec![
        entry(
            "Shut down idle devices",
            "Simulators and emulators OxiMux booted shut down after this long unused.",
            segmented(
                "sim-idle",
                IDLE_CHOICES
                    .iter()
                    .map(|&(minutes, label)| {
                        Segment::new(label, s.idle_shutdown_minutes == minutes, move |_, _, cx| {
                            change(cx, |s| s.idle_shutdown_minutes = minutes)
                        })
                    })
                    .collect(),
                theme,
                density,
                typography,
                cx,
            ),
        ),
        entry("Helper", helper_summary(hub, cx), div()),
        entry(
            "Device logs",
            "CoreSimulator's log folder, in Finder.",
            value_chip("sim-logs", "Open", theme, density, typography, |_, _, cx| open_logs_folder(cx), cx),
        ),
    ]
}

/// Apply `edit` to the settings: write `simulator.toml` and install the
/// result now (the file watcher re-reads the same value a moment later).
fn change(cx: &mut Context<SettingsModal>, edit: impl FnOnce(&mut SimulatorSettings)) {
    let mut next = crate::shell::simulator::panel::settings(cx);
    edit(&mut next);
    let next = next.sanitized();
    if let Err(err) = simulator_settings::save(&next) {
        tracing::warn!(%err, "settings modal: failed to write simulator.toml");
    }
    cx.set_global(next);
    cx.refresh_windows();
}

fn helper_summary(hub: Option<&Entity<SimulatorHub>>, cx: &Context<SettingsModal>) -> SharedString {
    let Some(hub) = hub.map(|h| h.read(cx)) else { return "Not available on this Mac.".into() };
    let version = hub.helper_version().map_or_else(|| "version shown once a device streams".to_owned(), |v| format!("v{v}"));
    match hub.availability().map(|a| &a.helper) {
        Some(oximux_simulator::availability::HelperStatus::Found(path)) => format!("{version} · {}", path.display()).into(),
        Some(oximux_simulator::availability::HelperStatus::Missing(why)) => why.clone().into(),
        None => version.into(),
    }
}

/// Automatic, or one of the listed devices, grouped like the panel's own
/// menu (iOS then Android, running first). The list is the hub's last
/// device listing (Refresh fetches one). Never a real device: the default
/// device attaches automatically, and a phone or an iPhone never does.
fn device_menu(hub: Option<&Entity<SimulatorHub>>, current: Option<&str>, cx: &mut Context<SettingsModal>) -> AnyElement {
    use crate::shell::simulator::panel::{PHYSICAL_GROUP, device_groups, os_label};
    let groups: Vec<(&'static str, Vec<(String, String)>)> = hub
        .map(|h| {
            device_groups(h.read(cx).devices())
                .into_iter()
                .filter(|(title, _)| *title != PHYSICAL_GROUP)
                .map(|(title, group)| (title, group.into_iter().map(|d| (d.udid.to_string(), format!("{} — {}", d.name, os_label(d)))).collect()))
                .collect()
        })
        .unwrap_or_default();
    // The saved device by name even when the menu leaves it out (an
    // unavailable runtime, say); "Unlisted" only when no listing has it.
    let label = match current {
        None => "Automatic".to_owned(),
        Some(udid) => hub
            .and_then(|h| h.read(cx).devices().iter().find(|d| d.udid.to_string() == udid).map(|d| format!("{} — {}", d.name, os_label(d))))
            .unwrap_or_else(|| "Unlisted device".to_owned()),
    };
    let current = current.map(str::to_owned);
    let entity = cx.entity();
    Button::new("sim-default-device")
        .label(label)
        .small()
        .outline()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            let pick = |menu: gpui_component::menu::PopupMenu, udid: Option<String>, name: String| {
                let checked = udid == current;
                menu.item(PopupMenuItem::new(name).checked(checked).on_click(window.listener_for(
                    &entity,
                    move |_: &mut SettingsModal, _, _, cx| {
                        let udid = udid.clone();
                        change(cx, move |s| s.default_device = udid);
                    },
                )))
            };
            menu = pick(menu, None, "Automatic".to_owned());
            for (title, group) in groups.clone() {
                menu = menu.label(title);
                for (udid, name) in group {
                    menu = pick(menu, Some(udid), name);
                }
            }
            menu
        })
        .into_any_element()
}

/// One row per approved device, with Revoke, then "Revoke all" when there
/// is more than one.
fn approval_rows(theme: Theme, density: Density, typography: &Typography, cx: &mut Context<SettingsModal>) -> Vec<SettingEntry> {
    let Some(hub) = hub(cx) else { return Vec::new() };
    let approvals = hub.read(cx).approvals().to_vec();
    let many = approvals.len() > 1;
    let mut rows: Vec<SettingEntry> = approvals
        .into_iter()
        .enumerate()
        .map(|(idx, approval)| {
            let hub = hub.clone();
            let udid = DeviceId(approval.udid.clone());
            let granted = approval.granted_at.get(..10).unwrap_or(&approval.granted_at).to_owned();
            entry(
                approval.device_name,
                format!("Allowed {granted} · {}", approval.udid),
                value_chip(
                    ("sim-revoke", idx),
                    "Revoke",
                    theme,
                    density,
                    typography,
                    move |_, _, cx| hub.update(cx, |hub, cx| hub.revoke_agents(&udid, cx)),
                    cx,
                ),
            )
        })
        .collect();
    if many {
        rows.push(entry(
            "Revoke all",
            "Every device asks again the next time an agent wants it.",
            value_chip(
                "sim-revoke-all",
                "Revoke all",
                theme,
                density,
                typography,
                move |_, _, cx| hub.update(cx, |hub, cx| hub.revoke_all_agents(cx)),
                cx,
            ),
        ));
    }
    rows
}

fn open_logs_folder(cx: &mut Context<SettingsModal>) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = std::path::Path::new(&home).join("Library/Logs/CoreSimulator");
    cx.background_executor()
        .spawn(async move {
            if let Err(err) = std::process::Command::new("/usr/bin/open").arg(&dir).status() {
                tracing::warn!(%err, "could not open the simulator logs folder");
            }
        })
        .detach();
}

/// "Beta" and what that means, above the settings.
fn beta_note(theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .px(px(6.0))
                .rounded(px(density.r_chip))
                .border_1()
                .border_color(theme.border_inactive)
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child("Beta"),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child("iOS simulators, Android emulators and phones on this Mac, for you and your coding agents."),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext, px, size};
    use oximux_simulator::consent::State;
    use oximux_storage::{SettingsRepo, SimApprovalRepo};

    use super::*;
    use crate::shell::settings_modal::SettingsPane;

    /// The pane paints with approvals listed, and a revoke removes the
    /// approval from the database *and* from the hub's memory (a grant left
    /// in memory would keep answering "allowed" until a restart).
    #[gpui::test]
    fn the_pane_paints_and_revoking_deletes_the_approval(cx: &mut TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let approvals = SimApprovalRepo::new(db.clone());
        // Android ids: opening the pane checks this Mac's Xcode for real, and
        // a simulator udid that `simctl` does not list is pruned as deleted.
        approvals.grant("avd:Pixel_9", "Pixel 9").expect("grant");
        approvals.grant("avd:Pixel_Tablet", "Pixel Tablet").expect("grant");
        cx.update(|cx| {
            cx.set_global(SimulatorSettings::default());
            crate::shell::simulator::hub::install_for_test(cx, SettingsRepo::new(db.clone()), approvals.clone());
        });
        let (w, m) = super::super::env_editor_tests::modal(cx);
        w.update(cx, |_, window, cx| {
            m.update(cx, |m, cx| {
                m.open(window, cx);
                m.selected = SettingsPane::Simulator;
            })
        })
        .expect("open on the Mobile Emulator pane");
        let mut vcx = VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(1100.0), px(800.0)));
        vcx.run_until_parked();

        let hub = vcx.update(|_, cx| hub(cx)).expect("hub");
        assert_eq!(hub.read_with(&vcx, |h, _| h.approvals().len()), 2, "listed from the database at startup");
        let (u1, worktree) = (DeviceId("avd:Pixel_9".into()), std::path::PathBuf::from("/w"));
        hub.update(&mut vcx, |hub, cx| hub.revoke_agents(&u1, cx));
        vcx.run_until_parked();

        let left: Vec<String> = approvals.list().expect("list").into_iter().map(|a| a.udid).collect();
        assert_eq!(left, ["avd:Pixel_Tablet"], "the row is gone from the database");
        hub.update(&mut vcx, |hub, _| {
            assert_eq!(hub.approvals().len(), 1);
            assert_eq!(hub.consent_state(&u1, &worktree), State::NotAsked, "and from memory: it asks again");
        });
        // Still paints with one left, then with none.
        hub.update(&mut vcx, |hub, cx| hub.revoke_all_agents(cx));
        vcx.run_until_parked();
        assert!(approvals.list().expect("list").is_empty());
    }

    /// L11: a chosen folder without adb is named as such, even when another
    /// SDK stands in for it.
    #[test]
    fn the_sdk_row_says_when_the_chosen_folder_is_not_used() {
        use std::path::Path;
        let (env, chosen) = (Path::new("/env/sdk"), Path::new("/picked"));
        use availability::android_summary_text;
        assert!(android_summary_text(Some(env), Some(chosen)).contains("No adb in the chosen folder"));
        assert!(android_summary_text(Some(env), Some(chosen)).ends_with("using /env/sdk."));
        assert_eq!(android_summary_text(Some(chosen), Some(chosen)), "Detected at /picked");
        assert!(android_summary_text(None, Some(chosen)).starts_with("No adb"));
        assert!(android_summary_text(None, None).starts_with("Not found"));
    }
}
