//! Device state on a simulator, through `xcrun simctl`: location, appearance,
//! privacy, push, the status bar, and erasing it.
//!
//! [`argv`] is pure (unit-tested for every change); [`apply`] runs it through
//! the [`Runner`]. Numbers are formatted by Rust, never by the locale, so a
//! latitude always has a `.`.

use std::thread;
use std::time::{Duration, Instant};

use crate::runner::Runner;
use crate::simctl::{self, SimUdid};
use crate::state::{Reject, Service, StateChange};
use crate::{Result, SimError};

/// The status bar a screenshot wants: Apple's 9:41, full signal and battery.
const CLEAN_STATUS_BAR: [&str; 16] = [
    "--time",
    "9:41",
    "--dataNetwork",
    "wifi",
    "--wifiMode",
    "active",
    "--wifiBars",
    "3",
    "--cellularMode",
    "active",
    "--cellularBars",
    "4",
    "--batteryState",
    "charged",
    "--batteryLevel",
    "100",
];

/// `simctl`'s arguments for `change` on `udid`.
pub fn argv(udid: &SimUdid, change: &StateChange) -> std::result::Result<Vec<String>, Reject> {
    let u = udid.as_str();
    let words: Vec<String> = match change {
        StateChange::Location { lat, lon } => {
            crate::state::check_location(*lat, *lon)?;
            vec!["location".into(), u.into(), "set".into(), format!("{lat},{lon}")]
        }
        StateChange::ClearLocation => vec!["location".into(), u.into(), "clear".into()],
        StateChange::Appearance { dark } => vec!["ui".into(), u.into(), "appearance".into(), if *dark { "dark" } else { "light" }.into()],
        StateChange::Privacy { action, service, app } => {
            crate::state::check_app(app)?;
            if matches!(service, Service::Camera | Service::Notifications) {
                return Err(Reject::Unsupported(format!(
                    "a simulator cannot {} {} from here; tap the system alert when the app asks",
                    action.word(),
                    service.name()
                )));
            }
            vec!["privacy".into(), u.into(), action.word().into(), service.name().into(), app.clone()]
        }
        StateChange::Push { app, payload } => {
            crate::state::check_app(app)?;
            crate::state::check_push(payload)?;
            // The payload goes on stdin (`-`), never on the command line.
            vec!["push".into(), u.into(), app.clone(), "-".into()]
        }
        StateChange::StatusBar { clean: true } => {
            let mut words = vec!["status_bar".into(), u.into(), "override".into()];
            words.extend(CLEAN_STATUS_BAR.iter().map(|w| (*w).to_owned()));
            words
        }
        StateChange::StatusBar { clean: false } => vec!["status_bar".into(), u.into(), "clear".into()],
    };
    Ok(std::iter::once("simctl".to_owned()).chain(words).collect())
}

/// Make `change` on `udid`. A refused change runs nothing.
pub fn apply(runner: &dyn Runner, udid: &SimUdid, change: &StateChange, timeout: Duration) -> Result<()> {
    let args = argv(udid, change).map_err(|r| match r {
        Reject::BadInput(why) | Reject::Unsupported(why) => SimError::Parse { what: "device state".into(), detail: why },
    })?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdin = match change {
        StateChange::Push { payload, .. } => Some(payload.as_bytes()),
        _ => None,
    };
    let out = runner.run("xcrun", &args, stdin, timeout)?;
    simctl::require_success(&format!("simctl {}", args[1]), Some(udid.as_str()), out).map(drop)
}

/// Erase `udid` once it has shut down: `simctl erase` refuses a booted (or
/// shutting-down) device, so it is retried until `wait` runs out.
pub fn erase(runner: &dyn Runner, udid: &SimUdid, timeout: Duration, wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let out = runner.run("xcrun", &["simctl", "erase", udid.as_str()], None, timeout)?;
        let still_up = String::from_utf8_lossy(&out.stderr).contains("current state:");
        if out.success() || !still_up || Instant::now() >= deadline {
            return simctl::require_success("simctl erase", Some(udid.as_str()), out).map(drop);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceId;
    use crate::runner::{CmdOutput, ScriptedRunner};
    use crate::state::PrivacyAction;

    fn udid() -> SimUdid {
        SimUdid::new(&DeviceId("U".into())).unwrap()
    }

    fn words(change: StateChange) -> Vec<String> {
        argv(&udid(), &change).unwrap()
    }

    #[test]
    fn every_change_has_its_argv() {
        assert_eq!(words(StateChange::Location { lat: 37.7749, lon: -122.4194 }), ["simctl", "location", "U", "set", "37.7749,-122.4194"]);
        assert_eq!(words(StateChange::Location { lat: 10.77, lon: 106.7 })[4], "10.77,106.7", "latitude first, a `.` always");
        assert_eq!(words(StateChange::ClearLocation), ["simctl", "location", "U", "clear"]);
        assert_eq!(words(StateChange::Appearance { dark: true }), ["simctl", "ui", "U", "appearance", "dark"]);
        assert_eq!(
            words(StateChange::Privacy { action: PrivacyAction::Reset, service: Service::Photos, app: "com.a.b".into() }),
            ["simctl", "privacy", "U", "reset", "photos", "com.a.b"]
        );
        assert_eq!(words(StateChange::Push { app: "com.a.b".into(), payload: r#"{"aps":{}}"#.into() }), ["simctl", "push", "U", "com.a.b", "-"]);
        assert_eq!(words(StateChange::StatusBar { clean: false }), ["simctl", "status_bar", "U", "clear"]);
        let clean = words(StateChange::StatusBar { clean: true });
        assert_eq!(&clean[..4], ["simctl", "status_bar", "U", "override"]);
        assert!(clean.windows(2).any(|w| w == ["--time", "9:41"]));
    }

    #[test]
    fn what_a_simulator_cannot_do_is_refused_before_running() {
        let camera = StateChange::Privacy { action: PrivacyAction::Grant, service: Service::Camera, app: "com.a.b".into() };
        assert!(matches!(argv(&udid(), &camera), Err(Reject::Unsupported(e)) if e.contains("system alert")));
        let bad_app = StateChange::Privacy { action: PrivacyAction::Grant, service: Service::Photos, app: "-x".into() };
        assert!(matches!(argv(&udid(), &bad_app), Err(Reject::BadInput(_))));
        assert!(argv(&udid(), &StateChange::Location { lat: 100.0, lon: 0.0 }).is_err());
    }

    #[test]
    fn a_push_payload_goes_on_stdin() {
        let runner = ScriptedRunner::default().expect("xcrun simctl push U com.a.b -", CmdOutput::ok(""));
        let push = StateChange::Push { app: "com.a.b".into(), payload: r#"{"aps":{"alert":"Hi"}}"#.into() };
        apply(&runner, &udid(), &push, Duration::from_secs(5)).unwrap();
        assert_eq!(runner.stdin_of(0).as_deref(), Some(br#"{"aps":{"alert":"Hi"}}"#.as_slice()));
    }

    #[test]
    fn erase_waits_for_the_shutdown() {
        let booted = CmdOutput::failed(149, "Unable to erase contents and settings in current state: Booted");
        let runner = ScriptedRunner::default()
            .expect("xcrun simctl erase U", booted)
            .expect("xcrun simctl erase U", CmdOutput::ok(""));
        erase(&runner, &udid(), Duration::from_secs(5), Duration::from_secs(10)).unwrap();
        let gone = ScriptedRunner::default().expect("xcrun simctl erase U", CmdOutput::failed(1, "Invalid device: U"));
        assert!(matches!(erase(&gone, &udid(), Duration::from_secs(5), Duration::from_secs(10)), Err(SimError::DeviceNotFound(_))));
    }
}
