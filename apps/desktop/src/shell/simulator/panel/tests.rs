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
    }
}

#[gpui::test]
fn every_state_body_renders(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let states = [
        PanelState::Checking,
        PanelState::Setup(availability(false)),
        PanelState::Empty { error: None },
        PanelState::Empty { error: Some("No usable iOS simulator.".into()) },
        PanelState::Attaching,
        PanelState::Booting,
        PanelState::Connecting,
        PanelState::Streaming,
        PanelState::Disconnected { reason: "The device shut down.".into() },
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
