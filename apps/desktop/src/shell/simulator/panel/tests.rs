//! Every panel body renders without panicking. This catches broken element
//! trees and missing builders, not invisible layout: the live visual pass
//! (P11) remains the gate for how it looks.

use gpui::{AppContext as _, AvailableSpace, ParentElement as _, Styled as _, TestAppContext, point, px, size};
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

/// The Wi-Fi pairing card renders on both tabs, and after a failure (no SDK
/// here: the hub says so instead of pairing).
#[gpui::test]
fn the_pairing_card_renders_on_both_tabs(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let db = oximux_storage::open_memory().expect("db");
    cx.update(|cx| {
        crate::shell::simulator::hub::install_for_test(
            cx,
            oximux_storage::SettingsRepo::new(db.clone()),
            oximux_storage::SimApprovalRepo::new(db),
        )
    });
    let (panel, vcx) = cx.add_window_view(|_window, cx| {
        SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx)
    });
    let draw = |vcx: &mut gpui::VisualTestContext| {
        let panel = panel.clone();
        vcx.draw(point(px(0.), px(0.)), size(AvailableSpace::Definite(px(360.)), AvailableSpace::Definite(px(800.))), |_, _| {
            gpui::div().size_full().child(panel)
        });
    };
    panel.update_in(vcx, |panel, window, cx| panel.open_pairing(window, cx));
    draw(vcx);
    panel.update_in(vcx, |panel, window, cx| panel.pairing_tab(super::pair_card::PairTab::Qr, window, cx));
    draw(vcx);
    panel.update(vcx, |panel, cx| {
        let stage = panel.hub.as_ref().unwrap().read(cx).pair_stage(cx.entity_id());
        assert!(matches!(stage, crate::shell::simulator::hub::PairStage::Failed(_)), "{stage:?}");
    });
    panel.update(vcx, |panel, cx| panel.close_pairing(cx));
    draw(vcx);
}

/// The grid: "+" offers only other worktrees' devices while there is room,
/// tiles render at every width in every state of the active tile, a tile
/// counts as a viewer while the panel shows, and a tile whose worktree
/// becomes the active one leaves the grid.
#[gpui::test]
fn the_grid_adds_renders_and_releases_tiles(cx: &mut TestAppContext) {
    use oximux_simulator::DeviceId;
    use oximux_simulator::registry::{Snapshot, WorktreeKey};
    cx.update(gpui_component::init);
    let db = oximux_storage::open_memory().expect("db");
    let repo = oximux_storage::SettingsRepo::new(db.clone());
    let root = tempfile::tempdir().expect("tempdir");
    let worktrees: Vec<std::path::PathBuf> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|n| {
            let p = root.path().join(n);
            std::fs::create_dir(&p).expect("mkdir");
            WorktreeKey::from_path(&p).path().to_path_buf()
        })
        .collect();
    let udid = |i: usize| DeviceId(format!("0000000{i}-0000-0000-0000-000000000000"));
    let attachments = worktrees.iter().enumerate().map(|(i, w)| (WorktreeKey::from_path(w), udid(i))).collect();
    crate::app_settings::sim_state_keys::save_snapshot(&repo, &Snapshot { attachments, owned_boots: Vec::new() });
    let hub = cx.update(|cx| {
        crate::shell::simulator::hub::install_for_test(cx, repo, oximux_storage::SimApprovalRepo::new(db))
    });
    let (panel, vcx) = cx.add_window_view(|_window, cx| {
        SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx)
    });
    let draw = |vcx: &mut gpui::VisualTestContext| {
        for width in [360.0, 1200.0] {
            let panel = panel.clone();
            vcx.draw(point(px(0.), px(0.)), size(AvailableSpace::Definite(px(width)), AvailableSpace::Definite(px(800.))), |_, _| {
                gpui::div().size_full().child(panel)
            });
        }
    };
    panel.update(vcx, |panel, cx| {
        // As the window names it: not canonical (a temp dir is under a symlink).
        panel.set_worktree(Some(root.path().join("a")), cx);
        assert_eq!(panel.tile_candidates(cx).len(), 4, "every other attached worktree, never the active one");
        // Shown, as `set_shown` would leave it, so tiles count as viewers.
        panel.shown = true;
        for w in &worktrees[1..] {
            panel.add_tile(w.clone(), cx);
        }
        assert_eq!(panel.tiles.len(), super::grid::MAX_TILES - 1, "the grid holds four");
        assert!(panel.tile_candidates(cx).is_empty(), "no room left");
    });
    assert!(hub.read_with(vcx, |hub, _| hub.is_shown(&udid(1)) && hub.is_shown(&udid(3))));
    assert!(!hub.read_with(vcx, |hub, _| hub.is_shown(&udid(4))), "never added");
    for state in [PanelState::Streaming, PanelState::Empty { error: None }, PanelState::Booting] {
        panel.update(vcx, |panel, _| panel.state_override = Some(state.clone()));
        draw(vcx);
    }
    panel.update(vcx, |panel, cx| {
        let first = panel.tiles[0].viewer;
        panel.focus_tile(Some(first), cx);
        assert_eq!(panel.panel_viewer().profile, oximux_simulator::viewers::StreamProfile::Reduced);
    });
    draw(vcx);
    panel.update(vcx, |panel, cx| {
        let first = panel.tiles[0].viewer;
        panel.remove_tile(first, cx);
        assert_eq!(panel.focused, None, "the removed tile's focus returns to the active one");
        // Switching to a tile's worktree makes it the active tile, and the
        // one that was active takes its slot.
        panel.set_worktree(Some(root.path().join("c")), cx);
        let tiles: Vec<_> = panel.tiles.iter().map(|t| t.worktree.clone()).collect();
        assert_eq!(tiles, [worktrees[0].clone(), worktrees[3].clone()]);
    });
    assert!(!hub.read_with(vcx, |hub, _| hub.is_shown(&udid(1))), "a removed tile is released");
    draw(vcx);
}

/// Attachments for `names` (one device each), and the hub that holds them.
fn grid_hub(cx: &mut TestAppContext, names: &[&str]) -> (tempfile::TempDir, Vec<std::path::PathBuf>, gpui::Entity<crate::shell::simulator::hub::SimulatorHub>) {
    use oximux_simulator::registry::{Snapshot, WorktreeKey};
    cx.update(gpui_component::init);
    let db = oximux_storage::open_memory().expect("db");
    let repo = oximux_storage::SettingsRepo::new(db.clone());
    let root = tempfile::tempdir().expect("tempdir");
    let worktrees: Vec<std::path::PathBuf> = names.iter().map(|n| {
        let p = root.path().join(n);
        std::fs::create_dir(&p).expect("mkdir");
        p
    }).collect();
    let attachments = worktrees.iter().enumerate().map(|(i, w)| (WorktreeKey::from_path(w), grid_udid(i))).collect();
    crate::app_settings::sim_state_keys::save_snapshot(&repo, &Snapshot { attachments, owned_boots: Vec::new() });
    let hub = cx.update(|cx| crate::shell::simulator::hub::install_for_test(cx, repo, oximux_storage::SimApprovalRepo::new(db)));
    (root, worktrees, hub)
}

fn grid_udid(i: usize) -> oximux_simulator::DeviceId {
    oximux_simulator::DeviceId(format!("0000000{i}-0000-0000-0000-000000000000"))
}

/// Switching worktree while a hide is still debouncing leaves every tile
/// not shown, so their devices pause and park.
#[gpui::test]
fn a_switch_during_the_hide_debounce_hides_the_tiles(cx: &mut TestAppContext) {
    let (_root, worktrees, hub) = grid_hub(cx, &["a", "b", "c"]);
    let (panel, vcx) = cx.add_window_view(|_window, cx| SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx));
    panel.update(vcx, |panel, cx| {
        panel.set_worktree(Some(worktrees[0].clone()), cx);
        panel.visible = true;
        panel.set_shown(true, cx);
        panel.add_tile(oximux_simulator::registry::WorktreeKey::from_path(&worktrees[2]).path().to_path_buf(), cx);
        // The sidebar closed: the hub has not heard yet.
        panel.visible = false;
        panel.set_worktree(Some(worktrees[1].clone()), cx);
    });
    assert!(!hub.read_with(vcx, |hub, _| hub.is_shown(&grid_udid(2))), "the tile follows the hidden panel");
    assert!(!hub.read_with(vcx, |hub, _| hub.is_shown(&grid_udid(0))));
}

/// Two windows on one worktree are two viewers: one hiding leaves the
/// device shown by the other.
#[gpui::test]
fn two_windows_on_one_worktree_are_two_viewers(cx: &mut TestAppContext) {
    let (_root, worktrees, hub) = grid_hub(cx, &["a"]);
    let (one, vcx) = cx.add_window_view(|_window, cx| SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx));
    let two = vcx.update(|_window, cx| cx.new(|cx| SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx)));
    for panel in [&one, &two] {
        panel.update(vcx, |panel, cx| {
            panel.set_worktree(Some(worktrees[0].clone()), cx);
            panel.set_shown(true, cx);
        });
    }
    one.update(vcx, |panel, cx| panel.set_shown(false, cx));
    assert!(hub.read_with(vcx, |hub, _| hub.is_shown(&grid_udid(0))), "the other window still shows it");
    two.update(vcx, |panel, cx| panel.set_shown(false, cx));
    assert!(!hub.read_with(vcx, |hub, _| hub.is_shown(&grid_udid(0))));
}

/// The header's × and Fill/Split reach the root through the sink, not as a
/// dispatched action: a click that takes the keyboard back from the
/// device's screen leaves nothing focused, and such an action reached no
/// handler (the × did nothing while a tile had the keyboard).
#[gpui::test]
fn header_clicks_reach_the_root_through_the_sink(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (panel, vcx) = cx.add_window_view(|_window, cx| {
        SimulatorPanel::new(Theme::default(), Density::default(), Typography::default(), cx)
    });
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = asked.clone();
    panel.update(vcx, |panel, _| {
        panel.set_command_sink(std::rc::Rc::new(move |request, _window, _cx| {
            seen.borrow_mut().push(match request {
                super::RootRequest::CloseSidebar => "close",
                super::RootRequest::ToggleMaximized => "fill",
                _ => "other",
            });
        }));
    });
    let close = panel.read_with(vcx, |panel, _| {
        panel.root_request(|| super::RootRequest::CloseSidebar, crate::actions::ToggleRightSidebar)
    });
    let fill = panel.read_with(vcx, |panel, _| {
        panel.root_request(|| super::RootRequest::ToggleMaximized, crate::actions::ToggleSimulatorMaximized)
    });
    vcx.update(|window, cx| {
        close(&gpui::ClickEvent::default(), window, cx);
        fill(&gpui::ClickEvent::default(), window, cx);
    });
    assert_eq!(*asked.borrow(), ["close", "fill"]);
}
