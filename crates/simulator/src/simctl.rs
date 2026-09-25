//! `xcrun simctl` as data: parsing `list devices`/`list runtimes` and the
//! blocking device-lifecycle calls (`boot`, `shutdown`, `install`, …) that the
//! panel drives a simulator with.
//!
//! Every call here is blocking and goes through [`Runner`] (real process on
//! [`SystemRunner`](crate::runner::SystemRunner), scripted output in tests),
//! so callers run these on a background executor and hold no lock across the
//! call. Nothing in this file invokes `xcrun` on its own initiative — that
//! gate lives in [`availability`](crate::availability), which decides whether
//! `xcrun` may run at all before any function here is called.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::runner::{CmdOutput, Runner};
use crate::{DeviceId, DeviceInfo, DeviceKind, DeviceState, Result, SimError};

/// One entry from `simctl list runtimes -j`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// e.g. `com.apple.CoreSimulator.SimRuntime.iOS-26-3`.
    pub identifier: String,
    /// e.g. `iOS 26.3`. Empty when `simctl` omitted `name`.
    pub name: String,
    /// e.g. `26.3.1`, as `simctl` reports it (not derived, unlike
    /// [`DeviceInfo::os_version`]).
    pub version: String,
    /// e.g. `iOS`, `watchOS`, `tvOS`.
    pub platform: String,
    pub is_available: bool,
}

/// Total time [`boot`] will poll for `Booted`, on top of the `boot` command
/// itself. Chosen to comfortably clear a cold-cache boot on CI hardware while
/// still failing well before a caller's own UI timeout.
const BOOT_POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// Delay between boot-state polls.
const BOOT_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// `xcrun simctl list devices -j`, flattened to one list. Every device is
/// kept, including non-iOS ones (watchOS, tvOS, …): they naturally end up
/// [`DeviceKind::Other`] since their name and device-type identifier never
/// contain `iPhone`/`iPad`. Filtering to iOS is the caller's job.
pub fn list_devices(runner: &dyn Runner, timeout: Duration) -> Result<Vec<DeviceInfo>> {
    let out = run(runner, &["simctl", "list", "devices", "-j"], timeout)?;
    let out = require_success("simctl list devices", None, out)?;
    let raw: RawDevicesList = parse_json("simctl list devices -j", &out.stdout)?;
    let mut devices = Vec::new();
    for (runtime_id, entries) in raw.devices {
        let os_version = derive_os_version(&runtime_id);
        for d in entries {
            devices.push(DeviceInfo {
                udid: DeviceId(d.udid),
                kind: derive_kind(d.device_type_identifier.as_deref(), &d.name),
                name: d.name,
                runtime: runtime_id.clone(),
                os_version: os_version.clone(),
                state: DeviceState::from_simctl(&d.state),
                is_available: d.is_available,
            });
        }
    }
    Ok(devices)
}

/// `xcrun simctl list runtimes -j`.
pub fn list_runtimes(runner: &dyn Runner, timeout: Duration) -> Result<Vec<RuntimeInfo>> {
    let out = run(runner, &["simctl", "list", "runtimes", "-j"], timeout)?;
    let out = require_success("simctl list runtimes", None, out)?;
    let raw: RawRuntimesList = parse_json("simctl list runtimes -j", &out.stdout)?;
    Ok(raw
        .runtimes
        .into_iter()
        .map(|r| RuntimeInfo {
            identifier: r.identifier,
            name: r.name,
            version: r.version,
            platform: r.platform,
            is_available: r.is_available,
        })
        .collect())
}

/// What [`boot`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootOutcome {
    /// We booted it: ours to shut down later.
    Booted,
    /// It was already booted (by the user, Xcode, an agent): never ours.
    AlreadyBooted,
}

/// Boots `udid`, tolerating "already booted", then polls up to
/// [`BOOT_POLL_TIMEOUT`] for the device to report `Booted`. Checks `cancel`
/// before every poll, so a UI-driven boot can be abandoned without leaving the
/// caller blocked for the full 30 s.
pub fn boot(runner: &dyn Runner, udid: &str, cmd_timeout: Duration, cancel: &AtomicBool) -> Result<BootOutcome> {
    let out = run(runner, &["simctl", "boot", udid], cmd_timeout)?;
    let mut outcome = BootOutcome::Booted;
    if !out.success() {
        let stderr = stderr_of(&out);
        // `simctl boot` on an already-booted device is not success we can
        // treat as "did nothing wrong" any other way: it exits non-zero with
        // this exact message.
        if !stderr.contains("Unable to boot device in current state: Booted") {
            return Err(classify_failure("simctl boot", udid, out.status, stderr));
        }
        outcome = BootOutcome::AlreadyBooted;
    }
    let deadline = Instant::now() + BOOT_POLL_TIMEOUT;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(SimError::Cancelled);
        }
        let devices = list_devices(runner, cmd_timeout)?;
        let device = devices.iter().find(|d| d.udid.as_str() == udid);
        match device {
            Some(d) if d.state == DeviceState::Booted => return Ok(outcome),
            Some(_) => {}
            None => return Err(SimError::DeviceNotFound(udid.to_owned())),
        }
        if Instant::now() >= deadline {
            return Err(SimError::Timeout { what: format!("boot {udid}"), secs: BOOT_POLL_TIMEOUT.as_secs() });
        }
        std::thread::sleep(BOOT_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub fn shutdown(runner: &dyn Runner, udid: &str, timeout: Duration) -> Result<()> {
    let out = run(runner, &["simctl", "shutdown", udid], timeout)?;
    require_success("simctl shutdown", Some(udid), out).map(drop)
}

/// `xcrun simctl io <udid> screenshot --type=png -`: the PNG bytes, written
/// to stdout by the trailing `-`.
pub fn screenshot_png(runner: &dyn Runner, udid: &str, timeout: Duration) -> Result<Vec<u8>> {
    let out = run(runner, &["simctl", "io", udid, "screenshot", "--type=png", "-"], timeout)?;
    let out = require_success("simctl io screenshot", Some(udid), out)?;
    if out.stdout.is_empty() {
        return Err(SimError::Parse { what: "simctl io screenshot".into(), detail: "empty stdout".into() });
    }
    Ok(out.stdout)
}

pub fn launch(runner: &dyn Runner, udid: &str, bundle_id: &str, timeout: Duration) -> Result<()> {
    let out = run(runner, &["simctl", "launch", udid, bundle_id], timeout)?;
    require_success("simctl launch", Some(udid), out).map(drop)
}

pub fn terminate(runner: &dyn Runner, udid: &str, bundle_id: &str, timeout: Duration) -> Result<()> {
    let out = run(runner, &["simctl", "terminate", udid, bundle_id], timeout)?;
    require_success("simctl terminate", Some(udid), out).map(drop)
}

pub fn open_url(runner: &dyn Runner, udid: &str, url: &str, timeout: Duration) -> Result<()> {
    let out = run(runner, &["simctl", "openurl", udid, url], timeout)?;
    require_success("simctl openurl", Some(udid), out).map(drop)
}

pub fn install(runner: &dyn Runner, udid: &str, app_path: &Path, timeout: Duration) -> Result<()> {
    let path = app_path.to_string_lossy();
    let out = run(runner, &["simctl", "install", udid, &path], timeout)?;
    require_success("simctl install", Some(udid), out).map(drop)
}

/// `xcrun simctl pbcopy <udid>`, feeding `text` on stdin: sets the
/// simulator's pasteboard.
pub fn pbcopy(runner: &dyn Runner, udid: &str, text: &str, timeout: Duration) -> Result<()> {
    let out = runner.run("xcrun", &["simctl", "pbcopy", udid], Some(text.as_bytes()), timeout)?;
    require_success("simctl pbcopy", Some(udid), out).map(drop)
}

/// `xcrun simctl get_app_container <udid> <bundle_id> [<container>]`.
/// `container` is `simctl`'s own vocabulary (`app`, `data`, `groups`, or a
/// specific app-group identifier); `None` means `simctl`'s default (`app`).
pub fn get_app_container(
    runner: &dyn Runner,
    udid: &str,
    bundle_id: &str,
    container: Option<&str>,
    timeout: Duration,
) -> Result<PathBuf> {
    let mut args = vec!["simctl", "get_app_container", udid, bundle_id];
    if let Some(c) = container {
        args.push(c);
    }
    let out = run(runner, &args, timeout)?;
    let out = require_success("simctl get_app_container", Some(udid), out)?;
    let path = out.stdout_str();
    let path = path.trim();
    if path.is_empty() {
        return Err(SimError::Parse { what: "simctl get_app_container".into(), detail: "empty stdout".into() });
    }
    Ok(PathBuf::from(path))
}

fn run(runner: &dyn Runner, args: &[&str], timeout: Duration) -> Result<CmdOutput> {
    runner.run("xcrun", args, None, timeout)
}

fn stderr_of(out: &CmdOutput) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
}

/// `out.success()` or the appropriate [`SimError`], recognising `simctl`'s
/// "Invalid device" phrasing as [`SimError::DeviceNotFound`] rather than a
/// generic command failure.
fn require_success(program: &str, udid: Option<&str>, out: CmdOutput) -> Result<CmdOutput> {
    if out.success() {
        return Ok(out);
    }
    let stderr = stderr_of(&out);
    Err(classify_failure(program, udid.unwrap_or("?"), out.status, stderr))
}

fn classify_failure(program: &str, udid: &str, code: Option<i32>, stderr: String) -> SimError {
    if stderr.contains("Invalid device") {
        return SimError::DeviceNotFound(udid.to_owned());
    }
    SimError::CommandFailed { program: program.to_owned(), code, stderr }
}

fn parse_json<T: for<'de> Deserialize<'de>>(what: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| SimError::Parse { what: what.to_owned(), detail: e.to_string() })
}

/// The device-type identifier or, failing that, the device name decide the
/// panel's coarse family. Anything not obviously an iPhone/iPad — watches,
/// TVs, a future device type — is [`DeviceKind::Other`], which the panel
/// simply does not offer to open.
fn derive_kind(device_type_identifier: Option<&str>, name: &str) -> DeviceKind {
    let haystack = device_type_identifier.unwrap_or(name);
    if haystack.contains("iPhone") {
        DeviceKind::Phone
    } else if haystack.contains("iPad") {
        DeviceKind::Tablet
    } else {
        DeviceKind::Other
    }
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-3` → `26.3`. Best-effort: an
/// identifier that doesn't match the usual `<Platform>-<N>-<N>` shape still
/// yields *something* (its trailing component, dashes turned to dots) rather
/// than failing the whole device list over one odd runtime.
fn derive_os_version(runtime_id: &str) -> String {
    let last = runtime_id.rsplit('.').next().unwrap_or(runtime_id);
    let stripped = ["iOS-", "watchOS-", "tvOS-", "xrOS-"]
        .iter()
        .find_map(|prefix| last.strip_prefix(prefix))
        .unwrap_or(last);
    stripped.replace('-', ".")
}

#[derive(Deserialize)]
struct RawDevicesList {
    devices: HashMap<String, Vec<RawDevice>>,
}

#[derive(Deserialize)]
struct RawDevice {
    udid: String,
    name: String,
    state: String,
    #[serde(rename = "deviceTypeIdentifier")]
    device_type_identifier: Option<String>,
    #[serde(rename = "isAvailable", default)]
    is_available: bool,
}

#[derive(Deserialize)]
struct RawRuntimesList {
    runtimes: Vec<RawRuntime>,
}

#[derive(Deserialize)]
struct RawRuntime {
    identifier: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    platform: String,
    #[serde(rename = "isAvailable", default)]
    is_available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const T: Duration = Duration::from_secs(5);

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/simctl/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn parses_real_xcode_26_3_device_list() {
        let json = fixture("list_devices_xcode26_3.json");
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl list devices -j",
            CmdOutput::ok(json),
        );
        let devices = list_devices(&runner, T).unwrap();
        assert!(devices.len() > 10, "expected many devices, got {}", devices.len());

        let booted = devices.iter().find(|d| d.state == DeviceState::Booted).expect("a booted device");
        assert_eq!(booted.name, "iPhone 17 Pro");
        assert_eq!(booted.kind, DeviceKind::Phone);
        assert_eq!(booted.os_version, "26.3");
        assert!(booted.is_available);

        let ipad = devices.iter().find(|d| d.name.starts_with("iPad")).expect("an iPad");
        assert_eq!(ipad.kind, DeviceKind::Tablet);

        // iOS 17.0 in this fixture has no matching runtime installed.
        let unavailable = devices.iter().find(|d| d.os_version == "17.0").expect("an iOS 17.0 device");
        assert!(!unavailable.is_available);
    }

    #[test]
    fn parses_real_xcode_26_3_runtime_list() {
        let json = fixture("list_runtimes_xcode26_3.json");
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl list runtimes -j",
            CmdOutput::ok(json),
        );
        let runtimes = list_runtimes(&runner, T).unwrap();
        assert_eq!(runtimes.len(), 1);
        assert_eq!(runtimes[0].platform, "iOS");
        assert_eq!(runtimes[0].version, "26.3.1");
        assert!(runtimes[0].is_available);
    }

    #[test]
    fn hand_made_device_list_survives_missing_keys_and_unknown_state() {
        let json = fixture("list_devices_hand_made.json");
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl list devices -j",
            CmdOutput::ok(json),
        );
        let devices = list_devices(&runner, T).unwrap();
        assert_eq!(devices.len(), 4);

        // Missing `isAvailable` and `deviceTypeIdentifier`: defaults to
        // unavailable, kind falls back to the device name.
        let iphone = devices.iter().find(|d| d.name == "iPhone 15").unwrap();
        assert!(!iphone.is_available);
        assert_eq!(iphone.kind, DeviceKind::Phone);

        // Unrecognised state string is kept verbatim, not dropped.
        let ipad = devices.iter().find(|d| d.name == "A Custom iPad").unwrap();
        assert_eq!(ipad.state, DeviceState::Other("Quiescing".into()));
        assert_eq!(ipad.kind, DeviceKind::Tablet);

        // A non-iPhone/iPad device (a watch) is kept, just as Other.
        let watch = devices.iter().find(|d| d.name.contains("Watch")).unwrap();
        assert_eq!(watch.kind, DeviceKind::Other);

        // An odd runtime id still produces a (best-effort) os_version rather
        // than failing the whole parse.
        let odd = devices.iter().find(|d| d.name == "iPhone Unknown").unwrap();
        assert_eq!(odd.os_version, "Weird.Runtime.Id");
    }

    #[test]
    fn hand_made_runtime_list_survives_missing_keys() {
        let json = fixture("list_runtimes_hand_made.json");
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl list runtimes -j",
            CmdOutput::ok(json),
        );
        let runtimes = list_runtimes(&runner, T).unwrap();
        assert_eq!(runtimes.len(), 3);
        assert!(!runtimes[0].is_available);
        // Missing `name` and `isAvailable` default rather than failing.
        assert_eq!(runtimes[2].name, "");
        assert!(!runtimes[2].is_available);
    }

    #[test]
    fn boot_treats_already_booted_as_success() {
        let runner = ScriptedRunner::default()
            .expect(
                "xcrun simctl boot ABCD",
                CmdOutput::failed(164, "An error was encountered processing the command (domain=com.apple.CoreSimulator.SimError, code=164): Unable to boot device in current state: Booted\n"),
            )
            .expect(
                "xcrun simctl list devices -j",
                CmdOutput::ok(booted_device_json("ABCD")),
            );
        let cancel = AtomicBool::new(false);
        assert_eq!(boot(&runner, "ABCD", T, &cancel).unwrap(), BootOutcome::AlreadyBooted);
    }

    #[test]
    fn boot_polls_until_booted() {
        let runner = ScriptedRunner::default()
            .expect("xcrun simctl boot ABCD", CmdOutput::ok(""))
            .expect("xcrun simctl list devices -j", CmdOutput::ok(booting_device_json("ABCD")))
            .expect("xcrun simctl list devices -j", CmdOutput::ok(booted_device_json("ABCD")));
        let cancel = AtomicBool::new(false);
        assert_eq!(boot(&runner, "ABCD", T, &cancel).unwrap(), BootOutcome::Booted);
    }

    #[test]
    fn boot_reports_cancelled_without_more_polls() {
        let cancel = AtomicBool::new(true);
        let runner = ScriptedRunner::default().expect("xcrun simctl boot ABCD", CmdOutput::ok(""));
        let err = boot(&runner, "ABCD", T, &cancel).unwrap_err();
        assert!(matches!(err, SimError::Cancelled), "{err:?}");
    }

    #[test]
    fn invalid_device_maps_to_device_not_found() {
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl shutdown NOPE",
            CmdOutput::failed(1, "Invalid device: NOPE\n"),
        );
        let err = shutdown(&runner, "NOPE", T).unwrap_err();
        assert!(matches!(&err, SimError::DeviceNotFound(u) if u == "NOPE"), "{err:?}");
    }

    #[test]
    fn other_failures_are_command_failed() {
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl launch ABCD com.example.app",
            CmdOutput::failed(3, "The request to launch com.example.app failed.\n"),
        );
        let err = launch(&runner, "ABCD", "com.example.app", T).unwrap_err();
        assert!(matches!(err, SimError::CommandFailed { code: Some(3), .. }), "{err:?}");
    }

    #[test]
    fn screenshot_returns_stdout_bytes() {
        let png_bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl io ABCD screenshot --type=png -",
            CmdOutput::ok(png_bytes.clone()),
        );
        assert_eq!(screenshot_png(&runner, "ABCD", T).unwrap(), png_bytes);
    }

    #[test]
    fn pbcopy_feeds_stdin_not_args() {
        let runner = ScriptedRunner::default().expect("xcrun simctl pbcopy ABCD", CmdOutput::ok(""));
        pbcopy(&runner, "ABCD", "hello", T).unwrap();
        assert_eq!(runner.calls(), vec!["xcrun simctl pbcopy ABCD"]);
    }

    #[test]
    fn get_app_container_trims_the_path() {
        let runner = ScriptedRunner::default().expect(
            "xcrun simctl get_app_container ABCD com.example.app",
            CmdOutput::ok("/path/to/App.app\n"),
        );
        let path = get_app_container(&runner, "ABCD", "com.example.app", None, T).unwrap();
        assert_eq!(path, PathBuf::from("/path/to/App.app"));
    }

    fn booted_device_json(udid: &str) -> String {
        device_list_json(udid, "Booted")
    }

    fn booting_device_json(udid: &str) -> String {
        device_list_json(udid, "Booting")
    }

    fn device_list_json(udid: &str, state: &str) -> String {
        format!(
            r#"{{"devices":{{"com.apple.CoreSimulator.SimRuntime.iOS-26-3":[
                {{"udid":"{udid}","name":"iPhone 17 Pro","state":"{state}","isAvailable":true,
                  "deviceTypeIdentifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"}}
            ]}}}}"#
        )
    }
}
