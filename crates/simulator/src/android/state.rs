//! Device state on Android, through `adb`: location (an emulator's console),
//! appearance, privacy (per package, per permission), and a clean status bar
//! (System UI's demo mode).
//!
//! `adb shell` joins its words for the device's `sh`, so every word here is a
//! constant, a permission from the table in [`crate::state`], or a package
//! that passed [`is_package`] — nothing is ever quoted at run time. A phone
//! cannot fake its location without an app, and FCM delivers pushes: both are
//! refused, never silently skipped.

use std::time::Duration;

use super::Target;
use super::adb::{Adb, console_refused};
use crate::state::{PrivacyAction, Reject, StateChange, is_package};
use crate::{Result, SimError};

/// One `adb -s <serial> …` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The emulator console (`emu …`), which answers `KO:` with exit 0.
    Console(Vec<String>),
    /// `shell …`.
    Shell(Vec<String>),
    /// A permission change that may not apply: the app need not declare
    /// every permission a service stands for, so a failure here is fine as
    /// long as one of the change's steps worked.
    Permission(Vec<String>),
}

const DEMO: &str = "com.android.systemui.demo";

/// System UI's demo mode: 9:41, full battery and signal, no notifications.
const CLEAN_STATUS_BAR: [&[&str]; 7] = [
    &["settings", "put", "global", "sysui_demo_allowed", "1"],
    &["am", "broadcast", "-a", DEMO, "-e", "command", "enter"],
    &["am", "broadcast", "-a", DEMO, "-e", "command", "clock", "-e", "hhmm", "0941"],
    &["am", "broadcast", "-a", DEMO, "-e", "command", "battery", "-e", "level", "100", "-e", "plugged", "false"],
    &["am", "broadcast", "-a", DEMO, "-e", "command", "network", "-e", "wifi", "show", "-e", "level", "4"],
    &["am", "broadcast", "-a", DEMO, "-e", "command", "network", "-e", "mobile", "show", "-e", "datatype", "none", "-e", "level", "4"],
    &["am", "broadcast", "-a", DEMO, "-e", "command", "notifications", "-e", "visible", "false"],
];

/// Demo mode off: the real status bar back.
pub const DEMO_EXIT: [&str; 7] = ["am", "broadcast", "-a", DEMO, "-e", "command", "exit"];

fn owned(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The calls that make `change` on `target`.
pub fn steps(target: &Target, change: &StateChange) -> std::result::Result<Vec<Step>, Reject> {
    let phone = matches!(target, Target::Serial(_));
    Ok(match change {
        StateChange::Location { .. } | StateChange::ClearLocation if phone => {
            return Err(Reject::Unsupported("setting a location is not available on a real phone (it needs a mock-location app)".into()));
        }
        StateChange::Location { lat, lon } => {
            crate::state::check_location(*lat, *lon)?;
            // The console takes the LONGITUDE first.
            vec![Step::Console(vec!["emu".into(), "geo".into(), "fix".into(), lon.to_string(), lat.to_string()])]
        }
        StateChange::ClearLocation => {
            return Err(Reject::Unsupported("an Android emulator cannot forget its location; set another one".into()));
        }
        StateChange::Appearance { dark } => vec![Step::Shell(owned(&["cmd", "uimode", "night", if *dark { "yes" } else { "no" }]))],
        StateChange::Privacy { action, service, app } => {
            if !is_package(app) {
                return Err(Reject::BadInput(format!("`{app}` is not an Android package name")));
            }
            let permissions = service.android_permissions();
            if permissions.is_empty() {
                return Err(Reject::Unsupported(format!("Android has no permission for {}", service.name())));
            }
            let mut out = Vec::new();
            for perm in permissions {
                let verb = if *action == PrivacyAction::Grant { "grant" } else { "revoke" };
                out.push(Step::Permission(owned(&["pm", verb, app, perm])));
                // Reset: revoked, and as if never asked (that package only;
                // `pm reset-permissions` would reset every app).
                if *action == PrivacyAction::Reset {
                    out.push(Step::Permission(owned(&["pm", "clear-permission-flags", app, perm, "user-set", "user-fixed"])));
                }
            }
            out
        }
        StateChange::Push { .. } => {
            return Err(Reject::Unsupported("a push notification cannot be sent to Android from here (FCM delivers them)".into()));
        }
        StateChange::StatusBar { clean: true } => CLEAN_STATUS_BAR.iter().map(|w| Step::Shell(owned(w))).collect(),
        StateChange::StatusBar { clean: false } => vec![Step::Shell(owned(&DEMO_EXIT))],
    })
}

/// Make `change` on the device at `serial` (`target` decides emulator or
/// phone). A refused change runs nothing.
pub fn apply(adb: &Adb<'_>, serial: &str, target: &Target, change: &StateChange, timeout: Duration) -> Result<()> {
    let steps = steps(target, change).map_err(|r| match r {
        Reject::BadInput(why) | Reject::Unsupported(why) => SimError::Parse { what: "device state".into(), detail: why },
    })?;
    let (mut applied, mut first_failure) = (false, None);
    for step in &steps {
        match step {
            Step::Console(words) => {
                let mut args = vec!["-s", serial];
                args.extend(words.iter().map(String::as_str));
                let out = adb.raw(&args, None, timeout)?.into_success("adb emu")?;
                if console_refused(&out.stdout_str()) {
                    return Err(SimError::CommandFailed {
                        program: "adb emu".into(),
                        code: None,
                        stderr: "the emulator console refused the command (no console auth token)".into(),
                    });
                }
            }
            Step::Shell(words) => drop(adb.shell(serial, &words.iter().map(String::as_str).collect::<Vec<_>>(), timeout)?),
            Step::Permission(words) => match adb.shell(serial, &words.iter().map(String::as_str).collect::<Vec<_>>(), timeout) {
                Ok(_) => applied = true,
                Err(e) => {
                    first_failure.get_or_insert(e);
                }
            },
        }
    }
    match first_failure {
        Some(e) if !applied => Err(e),
        _ => Ok(()),
    }
}

/// Demo mode off (on detach: a frozen status bar must not outlive the agent).
pub fn exit_demo_mode(adb: &Adb<'_>, serial: &str, timeout: Duration) -> Result<()> {
    adb.shell(serial, &DEMO_EXIT, timeout).map(drop)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner};
    use crate::state::Service;

    const AVD: fn() -> Target = || Target::Avd("Medium_Phone".into());
    const PHONE: fn() -> Target = || Target::Serial("R58".into());

    #[test]
    fn an_emulators_location_is_longitude_first() {
        let fix = steps(&AVD(), &StateChange::Location { lat: 10.5, lon: 106.7 }).unwrap();
        assert_eq!(fix, [Step::Console(owned(&["emu", "geo", "fix", "106.7", "10.5"]))]);
        assert!(matches!(steps(&PHONE(), &StateChange::Location { lat: 1.0, lon: 2.0 }), Err(Reject::Unsupported(_))));
        assert!(matches!(steps(&AVD(), &StateChange::ClearLocation), Err(Reject::Unsupported(_))));
    }

    #[test]
    fn a_reset_touches_only_that_package() {
        let reset = StateChange::Privacy { action: PrivacyAction::Reset, service: Service::Camera, app: "com.a.b".into() };
        let all: Vec<String> = steps(&PHONE(), &reset)
            .unwrap()
            .into_iter()
            .map(|s| match s {
                Step::Permission(w) => w.join(" "),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(all, ["pm revoke com.a.b android.permission.CAMERA", "pm clear-permission-flags com.a.b android.permission.CAMERA user-set user-fixed"]);
        assert!(!all.iter().any(|c| c.contains("reset-permissions")));
    }

    /// Nothing from a request reaches the device's `sh` unchecked.
    #[test]
    fn shell_metacharacters_never_reach_adb() {
        for app in ["com.a;reboot", "com.a $(id)", "com.a`id`", "com.a'b", "com.a&&x", "-x"] {
            let grant = StateChange::Privacy { action: PrivacyAction::Grant, service: Service::Camera, app: app.into() };
            assert!(matches!(steps(&AVD(), &grant), Err(Reject::BadInput(_))), "{app}");
            let runner = ScriptedRunner::default();
            assert!(apply(&Adb::new(&runner, Path::new("/sdk/adb")), "emulator-5554", &AVD(), &grant, Duration::from_secs(1)).is_err());
            assert!(runner.calls().is_empty(), "{app}: nothing ran");
        }
        for step in CLEAN_STATUS_BAR.iter().flat_map(|w| w.iter()).chain(DEMO_EXIT.iter()) {
            assert!(step.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')), "{step}");
        }
    }

    #[test]
    fn a_grant_holds_if_any_of_its_permissions_applied() {
        let adb_path = Path::new("/sdk/adb");
        let grant = StateChange::Privacy { action: PrivacyAction::Grant, service: Service::Location, app: "com.a.b".into() };
        let not_declared = CmdOutput::failed(255, "java.lang.SecurityException: Package com.a.b has not requested permission");
        let some = ScriptedRunner::default()
            .expect("/sdk/adb -s S shell pm grant com.a.b android.permission.ACCESS_FINE_LOCATION", CmdOutput::ok(""))
            .expect("/sdk/adb -s S shell pm grant com.a.b android.permission.ACCESS_COARSE_LOCATION", not_declared.clone());
        apply(&Adb::new(&some, adb_path), "S", &PHONE(), &grant, Duration::from_secs(1)).unwrap();
        let none = ScriptedRunner::default()
            .expect("/sdk/adb -s S shell pm grant com.a.b android.permission.ACCESS_FINE_LOCATION", not_declared.clone())
            .expect("/sdk/adb -s S shell pm grant com.a.b android.permission.ACCESS_COARSE_LOCATION", not_declared);
        assert!(apply(&Adb::new(&none, adb_path), "S", &PHONE(), &grant, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn a_console_refusal_is_an_error() {
        let runner = ScriptedRunner::default().expect("/sdk/adb -s emulator-5554 emu geo fix 2 1", CmdOutput::ok("KO: authentication required\r\n"));
        let set = StateChange::Location { lat: 1.0, lon: 2.0 };
        assert!(apply(&Adb::new(&runner, Path::new("/sdk/adb")), "emulator-5554", &AVD(), &set, Duration::from_secs(1)).is_err());
    }
}
