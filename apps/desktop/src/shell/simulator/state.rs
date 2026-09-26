//! What the simulator panel shows, derived purely from the hub's state.
//!
//! One function, [`derive`], turns availability × the worktree's attachment ×
//! the device's registry phase × the panel's own pending-attach bookkeeping
//! into a [`PanelState`]. Rendering matches on the result; nothing else
//! decides which body is on screen, so every transition is unit-tested here.

use oximux_simulator::availability::{Availability, Support};
use oximux_simulator::registry::Phase;

/// The panel body.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelState {
    /// The availability check has not finished yet.
    Checking,
    /// Something is missing (Xcode, a runtime, the helper…): the checklist.
    Setup(Availability),
    /// Nothing attached. `error` is the last attach failure, if any.
    Empty { error: Option<String> },
    /// Attach clicked; listing devices / picking one.
    Attaching,
    /// `simctl boot` in flight.
    Booting,
    /// Helper spawning and handshaking.
    Connecting,
    /// Live stream (the P6 screen slot).
    Streaming,
    /// Was live; now stopped (helper exited, device shut down).
    Disconnected { reason: String },
    /// A boot or start failed. `xcode_hint` offers the "switch Xcode" note:
    /// only when the selected Xcode is best-effort (27+) and the helper could
    /// not load its frameworks.
    Error { message: String, xcode_hint: bool },
}

/// Inputs to [`derive`], all read from the hub plus the panel's own flags.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    pub availability: Option<&'a Availability>,
    /// Whether the active worktree has an attached device.
    pub attached: bool,
    /// The attached device's phase (`Idle` when none).
    pub phase: &'a Phase,
    /// The panel asked the hub to attach and has not heard back.
    pub attaching: bool,
    pub attach_error: Option<&'a str>,
    /// An Android SDK was found: Android devices work whatever the iOS side
    /// is missing, so a missing Xcode does not block the panel.
    pub android_ready: bool,
}

pub fn derive(i: Inputs<'_>) -> PanelState {
    let Some(availability) = i.availability else { return PanelState::Checking };
    if !availability.is_ready() && !i.android_ready {
        return PanelState::Setup(availability.clone());
    }
    if !i.attached {
        return if i.attaching {
            PanelState::Attaching
        } else {
            PanelState::Empty { error: i.attach_error.map(str::to_owned) }
        };
    }
    match i.phase {
        // A restored attachment waits for the user's Attach (never auto-boots);
        // a click on it is already in flight when `attaching`.
        Phase::Idle if i.attaching => PanelState::Attaching,
        Phase::Idle => PanelState::Empty { error: i.attach_error.map(str::to_owned) },
        Phase::Booting { .. } => PanelState::Booting,
        // Parked restarts as soon as the panel shows it.
        Phase::Starting { .. } | Phase::Parked => PanelState::Connecting,
        Phase::Live { .. } => PanelState::Streaming,
        Phase::Disconnected { reason } => PanelState::Disconnected { reason: reason.clone() },
        Phase::Failed { error } => PanelState::Error {
            message: error.clone(),
            xcode_hint: availability.support == Support::BestEffort && error.contains("framework"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_simulator::availability::{HelperStatus, Xcode};
    use std::path::PathBuf;

    fn ready(support: Support) -> Availability {
        Availability {
            xcode: Xcode::Found { path: PathBuf::from("/Applications/Xcode.app/Contents/Developer"), version: Some("26.3".into()) },
            support,
            macos_ok: true,
            arch_ok: true,
            ios_runtimes: vec![oximux_simulator::simctl::RuntimeInfo {
                identifier: "com.apple.CoreSimulator.SimRuntime.iOS-26-3".into(),
                name: "iOS 26.3".into(),
                version: "26.3".into(),
                platform: "iOS".into(),
                is_available: true,
            }],
            helper: HelperStatus::Found(PathBuf::from("/x/oximux-sim-helper")),
        }
    }

    fn inputs<'a>(a: Option<&'a Availability>, attached: bool, phase: &'a Phase) -> Inputs<'a> {
        Inputs { availability: a, attached, phase, attaching: false, attach_error: None, android_ready: false }
    }

    #[test]
    fn availability_gates_everything() {
        let idle = Phase::Idle;
        assert_eq!(derive(inputs(None, true, &idle)), PanelState::Checking);
        let mut missing = ready(Support::Supported);
        missing.ios_runtimes.clear();
        assert!(matches!(derive(inputs(Some(&missing), true, &idle)), PanelState::Setup(_)));
        // With an Android SDK, a Mac missing the iOS pieces still gets a panel.
        let mut android = inputs(Some(&missing), false, &idle);
        android.android_ready = true;
        assert_eq!(derive(android), PanelState::Empty { error: None });
    }

    #[test]
    fn nothing_attached_is_empty_or_attaching() {
        let a = ready(Support::Supported);
        let idle = Phase::Idle;
        assert_eq!(derive(inputs(Some(&a), false, &idle)), PanelState::Empty { error: None });
        let mut i = inputs(Some(&a), false, &idle);
        i.attaching = true;
        assert_eq!(derive(i), PanelState::Attaching);
        i.attaching = false;
        i.attach_error = Some("no device");
        assert_eq!(derive(i), PanelState::Empty { error: Some("no device".into()) });
    }

    #[test]
    fn each_phase_has_its_body() {
        let a = ready(Support::Supported);
        let cases = [
            (Phase::Idle, PanelState::Empty { error: None }),
            (Phase::Booting { generation: 1 }, PanelState::Booting),
            (Phase::Starting { generation: 1 }, PanelState::Connecting),
            (Phase::Live { generation: 1 }, PanelState::Streaming),
            (Phase::Disconnected { reason: "gone".into() }, PanelState::Disconnected { reason: "gone".into() }),
            (Phase::Failed { error: "boom".into() }, PanelState::Error { message: "boom".into(), xcode_hint: false }),
        ];
        for (phase, want) in cases {
            assert_eq!(derive(inputs(Some(&a), true, &phase)), want, "{phase:?}");
        }
    }

    #[test]
    fn the_xcode_hint_needs_best_effort_xcode_and_a_framework_failure() {
        let failed = Phase::Failed { error: "could not load Xcode's simulator frameworks: x".into() };
        let best = ready(Support::BestEffort);
        assert!(matches!(derive(inputs(Some(&best), true, &failed)), PanelState::Error { xcode_hint: true, .. }));
        let supported = ready(Support::Supported);
        assert!(matches!(derive(inputs(Some(&supported), true, &failed)), PanelState::Error { xcode_hint: false, .. }));
        let other = Phase::Failed { error: "device not booted".into() };
        assert!(matches!(derive(inputs(Some(&best), true, &other)), PanelState::Error { xcode_hint: false, .. }));
    }
}
