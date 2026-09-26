//! iOS Simulator pane (Beta) — the feature switch, whether this Mac can run
//! it, what agents may do with it, the devices they may drive, and the stream
//! defaults.
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
use oximux_simulator::availability::Xcode;

use super::SettingsModal;
use super::controls::{toggle_switch, value_chip};
use super::layout::{SettingEntry, entries_card, entry, section_title};
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
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    let approvals = approval_rows(theme, density, typography, cx);
    div()
        .flex()
        .flex_col()
        .gap(px(16.0))
        .child(beta_note(theme, density, typography))
        .child(entries_card(theme, density, typography, entries(modal, theme, density, typography, cx)))
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
        .into_any_element()
}

/// The pane's rows, also listed by the settings search.
pub(super) fn entries(
    _modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let s = crate::shell::simulator::panel::settings(cx);
    let hub = hub(cx);
    vec![
        entry(
            "Enable iOS Simulator",
            "Show the Simulator tab in the right sidebar.",
            toggle_switch("sim-enabled", s.enabled, theme, |_, _, cx| change(cx, |s| s.enabled = !s.enabled), cx),
        ),
        entry(
            "Availability",
            availability_summary(hub.as_ref(), cx),
            value_chip("sim-refresh", "Refresh", theme, density, typography, |_, _, cx| refresh(cx), cx),
        ),
        entry(
            "Default device",
            "The simulator a worktree gets when it has none yet.",
            device_menu(hub.as_ref(), s.default_device.as_deref(), cx),
        ),
        entry(
            "Open automatically",
            "Show the panel when an agent builds, boots or drives a simulator.",
            toggle_switch("sim-auto-open", s.auto_open, theme, |_, _, cx| change(cx, |s| s.auto_open = !s.auto_open), cx),
        ),
        entry(
            "Agent control",
            "Let agents drive a simulator with `oximux sim`. Each device still asks you first.",
            toggle_switch(
                "sim-agent-control",
                s.agent_control,
                theme,
                |_, _, cx| change(cx, |s| s.agent_control = !s.agent_control),
                cx,
            ),
        ),
        entry(
            "Shut down idle simulators",
            "Simulators OxiMux booted shut down after this long unused.",
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
        entry("Helper", helper_summary(hub.as_ref(), cx), div()),
        entry(
            "Device logs",
            "CoreSimulator's log folder, in Finder.",
            value_chip("sim-logs", "Open", theme, density, typography, |_, _, cx| open_logs_folder(cx), cx),
        ),
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

fn refresh(cx: &mut Context<SettingsModal>) {
    if let Some(hub) = hub(cx) {
        hub.update(cx, |hub, cx| {
            hub.refresh_availability(cx);
            if hub.xcode_ok() {
                hub.refresh_devices(cx);
            }
        });
    }
}

/// Whether this Mac can run the simulator, in one line.
fn availability_summary(hub: Option<&Entity<SimulatorHub>>, cx: &Context<SettingsModal>) -> SharedString {
    let Some(hub) = hub else { return "Needs an Apple silicon Mac.".into() };
    match hub.read(cx).availability() {
        None => "Not checked yet. Refresh to check for Xcode.".into(),
        Some(a) => match a.blocking_reason() {
            Some(reason) => reason.into(),
            None => {
                let xcode = match &a.xcode {
                    Xcode::Found { version: Some(v), .. } => format!("Xcode {v}"),
                    _ => "Xcode".to_owned(),
                };
                let runtimes = a.ios_runtimes.len();
                let plural = if runtimes == 1 { "" } else { "s" };
                format!("Ready: {xcode}, {runtimes} iOS runtime{plural}.").into()
            }
        },
    }
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

/// Automatic, or one of the listed devices. The list is the hub's last
/// device listing (Refresh fetches one).
fn device_menu(hub: Option<&Entity<SimulatorHub>>, current: Option<&str>, cx: &mut Context<SettingsModal>) -> AnyElement {
    let devices: Vec<(String, String)> = hub
        .map(|h| h.read(cx).devices().iter().map(|d| (d.udid.to_string(), format!("{} ({})", d.name, d.runtime))).collect())
        .unwrap_or_default();
    let label = match current {
        None => "Automatic".to_owned(),
        Some(udid) => devices.iter().find(|(u, _)| u == udid).map_or_else(|| "Unlisted device".to_owned(), |(_, n)| n.clone()),
    };
    let mut options = vec![(None, "Automatic".to_owned())];
    options.extend(devices.into_iter().map(|(udid, name)| (Some(udid), name)));
    let current = current.map(str::to_owned);
    let entity = cx.entity();
    Button::new("sim-default-device")
        .label(label)
        .small()
        .outline()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, window, _cx| {
            for (udid, name) in options.clone() {
                let checked = udid == current;
                menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(window.listener_for(
                    &entity,
                    move |_: &mut SettingsModal, _, _, cx| {
                        let udid = udid.clone();
                        change(cx, move |s| s.default_device = udid);
                    },
                )));
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
                .child("iOS simulators on this Mac. Android comes later."),
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
        approvals.grant("U-1", "iPhone 17").expect("grant");
        approvals.grant("U-2", "iPad Air").expect("grant");
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
        .expect("open on the iOS Simulator pane");
        let mut vcx = VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(size(px(1100.0), px(800.0)));
        vcx.run_until_parked();

        let hub = vcx.update(|_, cx| hub(cx)).expect("hub");
        assert_eq!(hub.read_with(&vcx, |h, _| h.approvals().len()), 2, "listed from the database at startup");
        let (u1, worktree) = (DeviceId("U-1".into()), std::path::PathBuf::from("/w"));
        hub.update(&mut vcx, |hub, cx| hub.revoke_agents(&u1, cx));
        vcx.run_until_parked();

        let left: Vec<String> = approvals.list().expect("list").into_iter().map(|a| a.udid).collect();
        assert_eq!(left, ["U-2"], "the row is gone from the database");
        hub.update(&mut vcx, |hub, _| {
            assert_eq!(hub.approvals().len(), 1);
            assert_eq!(hub.consent_state(&u1, &worktree), State::NotAsked, "and from memory: it asks again");
        });
        // Still paints with one left, then with none.
        hub.update(&mut vcx, |hub, cx| hub.revoke_all_agents(cx));
        vcx.run_until_parked();
        assert!(approvals.list().expect("list").is_empty());
    }
}
