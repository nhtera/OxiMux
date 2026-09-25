//! Noticing devices boot and shut down underneath us (Simulator.app, `simctl`
//! in a terminal, an agent), so a live panel never sits frozen on a dead
//! device and the registry stops treating a user-shut device as ours.
//!
//! **Gated.** Polling `simctl` every few seconds costs every Mac user, and on
//! a Mac without Xcode even one `xcrun` call can pop the "install developer
//! tools" dialog. So nothing polls unless [`WatchGate::should_poll`]: Xcode
//! resolves, the feature is enabled, and the user has used the simulator at
//! least once. The caller owns the timer and runs [`BootWatch::poll`] on a
//! background executor.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::runner::Runner;
use crate::{DeviceId, DeviceState, Result, simctl};

/// How often the caller should poll while the gate is open.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// `simctl list` timeout for one poll.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything that must hold before any polling happens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchGate {
    /// `xcode-select -p` resolves to a full Xcode (see `availability`).
    pub xcode_ok: bool,
    /// Settings: the simulator feature is on.
    pub enabled: bool,
    /// The panel was opened once, or an attachment/session exists
    /// (persisted as `sim_feature_used`).
    pub feature_used: bool,
}

impl WatchGate {
    pub fn should_poll(self) -> bool {
        self.xcode_ok && self.enabled && self.feature_used
    }
}

/// A booted-set transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Booted(DeviceId),
    Shutdown(DeviceId),
}

/// Remembers the last booted set and reports what changed.
#[derive(Debug, Default)]
pub struct BootWatch {
    /// `None` until the first poll: the first observation is a baseline, not
    /// a burst of "booted" events for every device already running.
    booted: Option<BTreeSet<DeviceId>>,
}

/// The booted set right now. Blocking (one `simctl list`); call it without
/// holding any lock the UI thread might take, then [`BootWatch::observe`].
pub fn list_booted(runner: &dyn Runner) -> Result<BTreeSet<DeviceId>> {
    Ok(simctl::list_devices(runner, LIST_TIMEOUT)?
        .into_iter()
        .filter(|d| d.state == DeviceState::Booted)
        .map(|d| d.udid)
        .collect())
}

impl BootWatch {
    /// One poll: list devices and diff against the previous poll. Blocking.
    pub fn poll(&mut self, runner: &dyn Runner) -> Result<Vec<WatchEvent>> {
        Ok(self.observe(list_booted(runner)?))
    }

    /// Diff `now` against the last observation (pure; `poll` feeds it).
    pub fn observe(&mut self, now: BTreeSet<DeviceId>) -> Vec<WatchEvent> {
        let Some(before) = self.booted.replace(now.clone()) else { return Vec::new() };
        let mut events: Vec<WatchEvent> = before.difference(&now).cloned().map(WatchEvent::Shutdown).collect();
        events.extend(now.difference(&before).cloned().map(WatchEvent::Booted));
        events
    }

    /// Whether `udid` was booted at the last poll (`None` before the first).
    pub fn is_booted(&self, udid: &DeviceId) -> Option<bool> {
        self.booted.as_ref().map(|b| b.contains(udid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner};

    fn set(ids: &[&str]) -> BTreeSet<DeviceId> {
        ids.iter().map(|s| DeviceId((*s).into())).collect()
    }

    #[test]
    fn nothing_polls_unless_every_gate_is_open() {
        for xcode_ok in [false, true] {
            for enabled in [false, true] {
                for feature_used in [false, true] {
                    let gate = WatchGate { xcode_ok, enabled, feature_used };
                    assert_eq!(gate.should_poll(), xcode_ok && enabled && feature_used, "{gate:?}");
                }
            }
        }
    }

    #[test]
    fn the_first_observation_is_a_baseline_then_changes_are_reported() {
        let mut watch = BootWatch::default();
        assert!(watch.observe(set(&["A", "B"])).is_empty());
        assert_eq!(watch.is_booted(&DeviceId("A".into())), Some(true));
        let events = watch.observe(set(&["B", "C"]));
        assert_eq!(events, [WatchEvent::Shutdown(DeviceId("A".into())), WatchEvent::Booted(DeviceId("C".into()))]);
        assert!(watch.observe(set(&["B", "C"])).is_empty());
    }

    #[test]
    fn poll_reads_booted_devices_from_simctl() {
        let json = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-3":[
            {"udid":"A","name":"iPhone 17","state":"Booted","isAvailable":true,"deviceTypeIdentifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17"},
            {"udid":"B","name":"iPhone 16","state":"Shutdown","isAvailable":true,"deviceTypeIdentifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-16"}]}}"#;
        let runner = ScriptedRunner::default()
            .expect("xcrun simctl list devices -j", CmdOutput::ok(json))
            .expect("xcrun simctl list devices -j", CmdOutput::ok(json.replace("\"Booted\"", "\"Shutdown\"")));
        let mut watch = BootWatch::default();
        assert!(watch.poll(&runner).unwrap().is_empty());
        assert_eq!(watch.poll(&runner).unwrap(), [WatchEvent::Shutdown(DeviceId("A".into()))]);
    }
}
