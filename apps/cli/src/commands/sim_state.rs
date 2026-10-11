//! `oximux sim`'s device-state verbs (v28): the request each sends, the time
//! the host may take, and what to say once done.

use std::path::Path;
use std::time::Duration;

use oximux_remote_proto::simulator::{SimCmdWire, SimLocationWire, SimPrivacyActionWire, SimStateCmdWire};

use super::sim::{APP, SLOW, usage};
use crate::cli::{AppearanceArg, LocationCommand, SimCommand, StatusBarArg};
use crate::cli_sim_state::PrivacyActionArg;
use crate::output::Failure;

/// The most a push payload may be (Apple's limit for a notification).
const MAX_PUSH: usize = 4096;
/// Asked every time (≤ 90 s), then shut down, wiped and booting (≤ 90 s).
const ERASE: Duration = Duration::from_secs(200);

/// The request for a device-state verb; `None` for any other.
pub(super) fn request(command: &SimCommand, cwd: &Path) -> Option<Result<(SimCmdWire, Duration, String), Failure>> {
    let state = |s, floor, said: String| Ok((SimCmdWire::DeviceState(s), floor, said));
    Some(match command {
        SimCommand::Location(LocationCommand::Set { lat, lon }) => {
            state(SimStateCmdWire::Location(SimLocationWire::Set { lat: *lat, lon: *lon }), SLOW, format!("location set to {lat}, {lon}"))
        }
        SimCommand::Location(LocationCommand::Clear) => state(SimStateCmdWire::Location(SimLocationWire::Clear), SLOW, "location cleared".into()),
        SimCommand::Appearance { mode } => {
            let dark = *mode == AppearanceArg::Dark;
            // On a real phone the user is asked first.
            state(SimStateCmdWire::Appearance { dark }, APP, format!("switched to {} mode", if dark { "dark" } else { "light" }))
        }
        SimCommand::Privacy(args) => {
            let (action, said) = match args.action {
                PrivacyActionArg::Grant => (
                    SimPrivacyActionWire::Grant,
                    format!(
                        "granted {} to {}; the app may restart (note: a grant can hide a missing usage description in the app's Info.plist)",
                        args.service, args.app
                    ),
                ),
                PrivacyActionArg::Revoke => (SimPrivacyActionWire::Revoke, format!("revoked {} from {}; the app may restart", args.service, args.app)),
                PrivacyActionArg::Reset => (SimPrivacyActionWire::Reset, format!("reset {} for {}; it will be asked again", args.service, args.app)),
            };
            state(SimStateCmdWire::Privacy { action, service: args.service.clone(), app: args.app.clone() }, APP, said)
        }
        SimCommand::Push(args) => {
            let path = if args.file.is_absolute() { args.file.clone() } else { cwd.join(&args.file) };
            match push_payload(&path) {
                Ok(payload) => state(SimStateCmdWire::Push { app: args.app.clone(), payload }, APP, format!("sent the notification to {}", args.app)),
                Err(e) => Err(e),
            }
        }
        SimCommand::StatusBar { mode } => {
            let clean = *mode == StatusBarArg::Clean;
            state(SimStateCmdWire::StatusBar { clean }, APP, if clean { "status bar cleaned (9:41)" } else { "status bar restored" }.into())
        }
        SimCommand::Erase { force } => state(SimStateCmdWire::Erase { force: *force }, ERASE, "erased; booting again".into()),
        _ => return None,
    })
}

/// The JSON at `path`, checked here too: an object with an `aps` object, at
/// most [`MAX_PUSH`] bytes.
fn push_payload(path: &Path) -> Result<String, Failure> {
    let text = std::fs::read_to_string(path).map_err(|e| usage(&format!("cannot read {}: {e}", path.display())))?;
    if text.len() > MAX_PUSH {
        return Err(usage(&format!("a push payload is at most {MAX_PUSH} bytes ({} is {})", path.display(), text.len())));
    }
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| usage(&format!("{} is not JSON: {e}", path.display())))?;
    if !value.get("aps").is_some_and(serde_json::Value::is_object) {
        return Err(usage("a push payload is a JSON object with an `aps` object in it"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_is_latitude_then_longitude_by_name() {
        let cmd = SimCommand::Location(LocationCommand::Set { lat: 10.77, lon: 106.7 });
        let (wire, _, said) = request(&cmd, Path::new("/w")).unwrap().unwrap();
        assert_eq!(wire, SimCmdWire::DeviceState(SimStateCmdWire::Location(SimLocationWire::Set { lat: 10.77, lon: 106.7 })));
        assert_eq!(said, "location set to 10.77, 106.7");
        assert!(request(&SimCommand::Detach, Path::new("/w")).is_none(), "not a state verb");
    }

    #[test]
    fn a_push_payload_is_checked_before_it_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            std::fs::write(dir.path().join(name), body).unwrap();
            crate::cli_sim_state::PushArgs { app: "com.a.b".into(), file: name.into() }
        };
        let ok = write("ok.json", r#"{"aps":{"alert":"Hi"}}"#);
        let (wire, ..) = request(&SimCommand::Push(ok), dir.path()).unwrap().unwrap();
        assert!(matches!(wire, SimCmdWire::DeviceState(SimStateCmdWire::Push { payload, .. }) if payload.contains("Hi")));
        for (name, body) in [("no-aps.json", r#"{"alert":"Hi"}"#), ("bad.json", "{"), ("big.json", &format!(r#"{{"aps":{{"a":"{}"}}}}"#, "x".repeat(MAX_PUSH)))] {
            assert!(request(&SimCommand::Push(write(name, body)), dir.path()).unwrap().is_err(), "{name}");
        }
    }
}
