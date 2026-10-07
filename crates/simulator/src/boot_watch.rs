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
use crate::{DeviceId, DeviceState, Result, Source, simctl};

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
    /// The sources that observation listed: a device of any other is not in
    /// `booted` because nobody asked, not because it is shut down. Per source,
    /// not per platform: a `simctl`-only round says nothing about a real
    /// iPhone (`devicectl`), though both are iOS.
    listed: Vec<Source>,
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
    /// One poll: list simulators and diff against the previous poll. Blocking.
    pub fn poll(&mut self, runner: &dyn Runner) -> Result<Vec<WatchEvent>> {
        Ok(self.observe_listed(list_booted(runner)?, &[Source::Simctl]))
    }

    /// Diff `now`, a listing of every source, against the last observation
    /// (pure).
    pub fn observe(&mut self, now: BTreeSet<DeviceId>) -> Vec<WatchEvent> {
        self.observe_listed(now, &[Source::Simctl, Source::Adb, Source::Devicectl])
    }

    /// [`Self::observe`] for a listing of `sources` only. A source this round
    /// lists and the last did not starts from a baseline (its devices already
    /// running are not news); one it no longer lists reports no shutdowns
    /// (nobody asked).
    pub fn observe_listed(&mut self, now: BTreeSet<DeviceId>, sources: &[Source]) -> Vec<WatchEvent> {
        let before_listed = std::mem::replace(&mut self.listed, sources.to_vec());
        let Some(before) = self.booted.replace(now.clone()) else { return Vec::new() };
        let both = |udid: &&DeviceId| sources.contains(&udid.source()) && before_listed.contains(&udid.source());
        let mut events: Vec<WatchEvent> = before.difference(&now).filter(both).cloned().map(WatchEvent::Shutdown).collect();
        events.extend(now.difference(&before).filter(both).cloned().map(WatchEvent::Booted));
        events
    }

    /// Whether a baseline exists: `false` before the first observation (and
    /// after [`Self::forget`]), when the next one reports no events.
    pub fn has_baseline(&self) -> bool {
        self.booted.is_some()
    }

    /// The sources the last observation listed.
    pub fn listed(&self) -> &[Source] {
        &self.listed
    }

    /// Drop the baseline: the next observation is a fresh one, not a diff.
    pub fn forget(&mut self) {
        self.booted = None;
    }

    /// Whether `udid` was booted at the last poll (`None` before the first,
    /// or when that poll did not list its source).
    pub fn is_booted(&self, udid: &DeviceId) -> Option<bool> {
        let booted = self.booted.as_ref().filter(|_| self.listed.contains(&udid.source()))?;
        Some(booted.contains(udid))
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
        assert!(!watch.has_baseline());
        assert!(watch.observe(set(&["A", "B"])).is_empty());
        assert!(watch.has_baseline());
        assert_eq!(watch.is_booted(&DeviceId("A".into())), Some(true));
        let events = watch.observe(set(&["B", "C"]));
        assert_eq!(events, [WatchEvent::Shutdown(DeviceId("A".into())), WatchEvent::Booted(DeviceId("C".into()))]);
        assert!(watch.observe(set(&["B", "C"])).is_empty());
        watch.forget();
        assert!(!watch.has_baseline(), "forgetting starts a fresh baseline");
    }

    /// A round that did not list a source says nothing about its devices:
    /// when it is listed again, the ones running are a baseline, not a burst
    /// of boots (a window would auto-attach one); while it is not, nothing
    /// reads as shut down.
    #[test]
    fn a_source_listed_again_starts_from_a_baseline() {
        let (ios, android, both) = ([Source::Simctl], [Source::Adb], [Source::Simctl, Source::Adb]);
        let mut watch = BootWatch::default();
        // Xcode not known yet, Android SDK found: a round listing nothing.
        assert!(watch.observe_listed(set(&[]), &[]).is_empty());
        assert_eq!(watch.is_booted(&DeviceId("A".into())), None, "iOS was not asked");
        assert!(watch.observe_listed(set(&["A"]), &ios).is_empty(), "the simulator already running is not news");
        assert_eq!(watch.is_booted(&DeviceId("A".into())), Some(true));
        assert_eq!(watch.observe_listed(set(&["A", "B"]), &ios), [WatchEvent::Booted(DeviceId("B".into()))]);
        // Android polling starts (an emulator was attached).
        assert!(watch.observe_listed(set(&["A", "B", "avd:Pixel"]), &both).is_empty());
        // And stops: its emulator is not read as shut down.
        assert!(watch.observe_listed(set(&["A", "B"]), &ios).is_empty());
        assert_eq!(watch.is_booted(&DeviceId("avd:Pixel".into())), None);
        assert_eq!(watch.observe_listed(set(&["avd:Pixel"]), &android), Vec::<WatchEvent>::new(), "iOS not asked, Android new");
    }

    /// A real iPhone is iOS but not `simctl`'s: a `simctl`-only round never
    /// reads it as shut down, and neither does an `adb` round a phone.
    #[test]
    fn a_simctl_round_never_shuts_down_a_real_device() {
        let iphone = DeviceId("iosdev:00008110-001A2C3E0A88401E".into());
        let mut watch = BootWatch::default();
        let all = [Source::Simctl, Source::Adb, Source::Devicectl];
        assert!(watch.observe_listed(set(&["A", "iosdev:00008110-001A2C3E0A88401E", "adb:R58"]), &all).is_empty());
        assert!(watch.observe_listed(set(&["A"]), &[Source::Simctl]).is_empty(), "nobody asked devicectl or adb");
        assert_eq!(watch.is_booted(&iphone), None);
        assert_eq!(watch.is_booted(&DeviceId("A".into())), Some(true));
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
