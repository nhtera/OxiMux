//! Every panel body renders without panicking. This catches broken element
//! trees and missing builders, not invisible layout: the live visual pass
//! (P11) remains the gate for how it looks.

use gpui::{AvailableSpace, ParentElement as _, Styled as _, TestAppContext, point, px, size};
use oximux_settings::{Density, Theme, Typography};
use oximux_simulator::availability::{Availability, HelperStatus, Support, Xcode};

use super::SimulatorPanel;
use crate::shell::simulator::state::PanelState;

fn availability(ready: bool) -> Availability {
    Availability {
        xcode: Xcode::Found { path: "/Applications/Xcode.app/Contents/Developer".into(), version: Some("26.3".into()) },
        support: Support::Supported,
        macos_ok: true,
        arch_ok: true,
        ios_runtimes: Vec::new(),
        helper: if ready { HelperStatus::Found("/x".into()) } else { HelperStatus::Missing("not bundled".into()) },
        verified_xcode: None,
    }
}

#[gpui::test]
fn every_state_body_renders(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let states = [
        PanelState::Checking,
        PanelState::Setup(availability(false)),
        PanelState::Empty { error: None },
        PanelState::Empty { error: Some("No usable device.".into()) },
        PanelState::Attaching,
        PanelState::Booting,
        PanelState::Connecting,
        PanelState::Streaming,
        PanelState::Disconnected { reason: "The device shut down.".into(), xcode_hint: false },
        PanelState::Disconnected { reason: "The stream helper exited (code 6).".into(), xcode_hint: true },
        PanelState::Error { message: "framework load failed".into(), xcode_hint: true },
        PanelState::Error { message: "boom".into(), xcode_hint: false },
    ];
    let (panel, vcx) = cx.add_window_view(|_window, cx| {
        SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx)
    });
    for state in states {
        panel.update(vcx, |panel, _| panel.state_override = Some(state.clone()));
        // Narrow sidebar and a maximized one.
        for width in [360.0, 1200.0] {
            let panel = panel.clone();
            vcx.draw(point(px(0.), px(0.)), size(AvailableSpace::Definite(px(width)), AvailableSpace::Definite(px(800.))), |_, _| gpui::div().size_full().child(panel));
        }
    }
}

fn listed(udid: &str, name: &str, booted: bool, available: bool, note: Option<&str>) -> oximux_simulator::DeviceInfo {
    use oximux_simulator::{DeviceId, DeviceInfo, DeviceKind, DeviceState};
    DeviceInfo {
        udid: DeviceId(udid.into()),
        name: name.into(),
        runtime: if udid.starts_with("a") { "Android 16".into() } else { "com.apple.CoreSimulator.SimRuntime.iOS-26-3".into() },
        os_version: if udid.starts_with("a") { "16".into() } else { "26.3".into() },
        state: if booted { DeviceState::Booted } else { DeviceState::Shutdown },
        kind: DeviceKind::Phone,
        is_available: available,
        note: note.map(Into::into),
    }
}

/// Real devices get a group of their own, last, which keeps a phone that has
/// not approved this Mac yet (disabled, with why); the iOS and Android groups
/// hold only simulators and emulators.
#[test]
fn real_devices_have_their_own_group_and_keep_unavailable_rows() {
    use super::header::{PHYSICAL_GROUP, device_groups, menu_label};
    let devices = [
        listed("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3", "iPhone 17", true, true, None),
        listed("iosdev:00008110-001A2C3E0A88401E", "Tien's iPhone", true, true, None),
        listed("avd:Pixel", "Pixel", false, true, None),
        listed("adb:R58", "Galaxy S24", true, true, None),
        listed("adb:0A1B", "Pixel 8", false, false, Some("Unlock the phone and tap Allow USB debugging")),
        listed("avd:Broken", "Broken", false, false, None),
    ];
    let groups = device_groups(&devices);
    let titles: Vec<&str> = groups.iter().map(|(t, _)| *t).collect();
    assert_eq!(titles, ["iOS · Booted", "Android · Emulators (will boot)", PHYSICAL_GROUP]);
    let physical: Vec<&str> = groups[2].1.iter().map(|d| d.udid.as_str()).collect();
    assert_eq!(physical.len(), 3, "{physical:?}");
    assert!(physical.iter().all(|u| u.starts_with("adb:") || u.starts_with("iosdev:")));
    assert!(groups[..2].iter().flat_map(|(_, g)| g).all(|d| !d.udid.is_physical() && d.is_available));
    assert_eq!(menu_label(&devices[4]), "Pixel 8 — Android 16 · Unlock the phone and tap Allow USB debugging");
    assert_eq!(menu_label(&devices[3]), "Galaxy S24 — Android 16");
}

/// The device menu with real devices, one of them unavailable, builds and
/// paints.
#[gpui::test]
fn the_device_menu_with_real_devices_renders(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let devices = vec![
        listed("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3", "iPhone 17", true, true, None),
        listed("adb:R58", "Galaxy S24", true, true, None),
        listed("adb:0A1B", "Pixel 8", false, false, Some("Unlock the phone and tap Allow USB debugging")),
    ];
    let (panel, vcx) = cx.add_window_view(|_window, cx| {
        SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx)
    });
    let weak = panel.downgrade();
    let menu = vcx.update(|window, cx| {
        gpui_component::menu::PopupMenu::build(window, cx, |menu, _, _| super::header::device_menu(menu, &devices, None, weak))
    });
    vcx.draw(point(px(0.), px(0.)), size(AvailableSpace::Definite(px(360.)), AvailableSpace::Definite(px(800.))), |_, _| {
        gpui::div().size_full().child(menu)
    });
}
