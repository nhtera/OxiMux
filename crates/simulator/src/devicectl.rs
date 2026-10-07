//! Real iPhones through `xcrun devicectl` (Xcode 15+): listing them, and
//! launching or installing apps on one. Streaming is the capture helper's
//! (see `helper::HelperKind::DeviceCapture`); input is the control runner's.
//!
//! A phone is named everywhere by its **hardware UDID**
//! (`hardwareProperties.udid`, e.g. `00008130-001234CC2183001C`), not by
//! devicectl's own `identifier`: it is what AVFoundation calls the phone's
//! screen (measured), and `--device` takes it too. In OxiMux it is
//! `iosdev:<udid>` ([`crate::IOSDEV_PREFIX`]).
//!
//! The JSON is read leniently — every field optional — because its shape is
//! devicectl's, not a contract: a row it cannot read is left out, never an
//! error for the whole listing.

use std::time::Duration;

use serde::Deserialize;

use crate::runner::{CmdOutput, Runner};
use crate::{DeviceId, DeviceInfo, DeviceKind, DeviceState, IOSDEV_PREFIX, Result, SimError};

/// Why a listed iPhone cannot be shown yet, for the device menu.
pub const CONNECT_WITH_CABLE: &str = "Connect it with a cable (its screen is shown over USB only)";
pub const TRUST_THIS_MAC: &str = "Unlock the phone and tap “Trust”";

/// `xcrun devicectl list devices`: every real iPhone this Mac can reach, a
/// cabled and trusting one usable (`Booted`), one on Wi-Fi or not trusting
/// this Mac listed but disabled, with a note why. A phone paired once but
/// not around (no transport) is not listed at all.
pub fn list(runner: &dyn Runner, timeout: Duration) -> Result<Vec<DeviceInfo>> {
    let secs = timeout.as_secs().max(1).to_string();
    let out = runner.run(
        "xcrun",
        &["devicectl", "list", "devices", "--quiet", "--timeout", &secs, "--json-output", "/dev/stdout"],
        None,
        timeout + Duration::from_secs(2),
    )?;
    parse_list(&required("devicectl list devices", out)?.stdout)
}

/// The iPhones [`list`] would offer, for the device watcher: one listing
/// answers both.
pub fn connected(devices: &[DeviceInfo]) -> impl Iterator<Item = &DeviceId> {
    devices.iter().filter(|d| d.udid.source() == crate::Source::Devicectl && d.is_available).map(|d| &d.udid)
}

/// The phone's hardware UDID in `id`, if `id` is a real iPhone's (and well
/// formed: hex digits and dashes, which nothing can be bent with).
pub fn hardware_udid(id: &DeviceId) -> Option<&str> {
    id.as_str().strip_prefix(IOSDEV_PREFIX).filter(|u| is_udid(u))
}

fn is_udid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Launch `bundle_id` on the phone, in front (`relaunch`: restarted when it
/// is running), with `url` handed to it when given.
pub fn launch(runner: &dyn Runner, id: &DeviceId, bundle_id: &str, relaunch: bool, url: Option<&str>, timeout: Duration) -> Result<()> {
    let udid = device_arg(id)?;
    if bundle_id.is_empty() || bundle_id.starts_with('-') {
        return Err(SimError::Unsupported(format!("not a bundle id: {bundle_id}")));
    }
    let mut args = vec!["devicectl", "device", "process", "launch", "--quiet", "--device", udid];
    if relaunch {
        args.push("--terminate-existing");
    }
    if let Some(url) = url {
        args.extend(["--payload-url", url]);
    }
    args.push(bundle_id);
    let out = runner.run("xcrun", &args, None, timeout)?;
    required("devicectl device process launch", out).map(drop)
}

/// Open a web page in Safari on the phone. The caller has already checked it
/// is http(s).
pub fn open_url(runner: &dyn Runner, id: &DeviceId, url: &str, timeout: Duration) -> Result<()> {
    launch(runner, id, SAFARI, false, Some(url), timeout)
}

const SAFARI: &str = "com.apple.mobilesafari";

/// Install the `.app` (or `.ipa`) at `path` on the phone.
pub fn install(runner: &dyn Runner, id: &DeviceId, path: &std::path::Path, timeout: Duration) -> Result<()> {
    let udid = device_arg(id)?;
    let path = path.to_str().ok_or_else(|| SimError::Unsupported("the app's path is not UTF-8".into()))?;
    let out = runner.run("xcrun", &["devicectl", "device", "install", "app", "--quiet", "--device", udid, path], None, timeout)?;
    required("devicectl device install app", out).map(drop)
}

fn device_arg(id: &DeviceId) -> Result<&str> {
    hardware_udid(id).ok_or_else(|| SimError::DeviceNotFound(id.to_string()))
}

/// `out` when devicectl succeeded, else its complaint in words a person can
/// act on.
fn required(program: &str, out: CmdOutput) -> Result<CmdOutput> {
    if out.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(SimError::CommandFailed { program: program.to_owned(), code: out.status, stderr: hint(&stderr).unwrap_or(stderr) })
}

/// A plainer reason for devicectl's commonest failures.
fn hint(stderr: &str) -> Option<String> {
    let s = stderr.to_ascii_lowercase();
    let why = if s.contains("locked") {
        "The iPhone is locked: unlock it and try again."
    } else if s.contains("developer mode") {
        "Developer Mode is off on the iPhone (Settings › Privacy & Security › Developer Mode)."
    } else if s.contains("not paired") || s.contains("pairing") {
        "The iPhone does not trust this Mac yet: unlock it and tap “Trust”."
    } else if s.contains("unable to locate application") || s.contains("no application") {
        "That app is not installed on the iPhone."
    } else {
        return None;
    };
    Some(why.to_owned())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Raw {
    result: RawResult,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawResult {
    devices: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawDevice {
    connection_properties: RawConnection,
    device_properties: RawProperties,
    hardware_properties: RawHardware,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawConnection {
    pairing_state: Option<String>,
    transport_type: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawProperties {
    name: Option<String>,
    os_version_number: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RawHardware {
    device_type: Option<String>,
    marketing_name: Option<String>,
    reality: Option<String>,
    udid: Option<String>,
}

fn parse_list(bytes: &[u8]) -> Result<Vec<DeviceInfo>> {
    let raw: Raw = serde_json::from_slice(bytes)
        .map_err(|e| SimError::Parse { what: "devicectl list devices".into(), detail: e.to_string() })?;
    // One row at a time: a row of a shape this build cannot read is skipped.
    Ok(raw.result.devices.into_iter().filter_map(|v| serde_json::from_value::<RawDevice>(v).ok()).filter_map(row).collect())
}

fn row(d: RawDevice) -> Option<DeviceInfo> {
    let hw = d.hardware_properties;
    // Real iPhones only: no simulators (devicectl lists them on some
    // setups), no iPads, watches or Macs.
    if hw.reality.as_deref() != Some("physical") || hw.device_type.as_deref() != Some("iPhone") {
        return None;
    }
    let udid = hw.udid.filter(|u| is_udid(u))?;
    let conn = d.connection_properties;
    let note = match (conn.transport_type.as_deref()?, conn.pairing_state.as_deref()) {
        (_, Some(state)) if state != "paired" => Some(TRUST_THIS_MAC),
        ("wired", _) => None,
        _ => Some(CONNECT_WITH_CABLE),
    };
    let os_version = d.device_properties.os_version_number.unwrap_or_default();
    Some(DeviceInfo {
        udid: DeviceId(format!("{IOSDEV_PREFIX}{udid}")),
        name: d.device_properties.name.or(hw.marketing_name).unwrap_or_else(|| "iPhone".into()),
        runtime: if os_version.is_empty() { "iOS".into() } else { format!("iOS {os_version}") },
        os_version,
        // What an agent's `sim devices` shows for a phone it cannot use.
        state: if note.is_none() { DeviceState::Booted } else { DeviceState::Other("Unavailable".into()) },
        kind: DeviceKind::Phone,
        is_available: note.is_none(),
        note: note.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<DeviceInfo> {
        let path = format!("{}/tests/fixtures/devicectl/{name}.json", env!("CARGO_MANIFEST_DIR"));
        parse_list(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn a_cabled_trusting_iphone_is_usable() {
        let rows = fixture("wired");
        assert_eq!(rows.len(), 1);
        let phone = &rows[0];
        assert_eq!(phone.udid.as_str(), "iosdev:00008130-000A1B2C3D4E5F60");
        assert!(phone.udid.is_physical());
        assert_eq!((phone.runtime.as_str(), phone.os_version.as_str()), ("iOS 27.0", "27.0"));
        assert_eq!(phone.name, "Test’s iPhone");
        assert!(phone.is_available && phone.note.is_none() && phone.state == DeviceState::Booted);
        assert_eq!(connected(&rows).collect::<Vec<_>>(), [&phone.udid]);
        assert_eq!(hardware_udid(&phone.udid), Some("00008130-000A1B2C3D4E5F60"));
    }

    #[test]
    fn one_on_wifi_or_not_trusting_is_listed_disabled_with_why() {
        let wifi = &fixture("network")[0];
        assert!(!wifi.is_available);
        assert_eq!(wifi.note.as_deref(), Some(CONNECT_WITH_CABLE));
        assert_eq!(wifi.state, DeviceState::Other("Unavailable".into()));
        // `unpaired` is the wired fixture with its pairing state edited (a
        // phone that has not trusted this Mac could not be recorded).
        let untrusted = &fixture("unpaired")[0];
        assert!(!untrusted.is_available);
        assert_eq!(untrusted.note.as_deref(), Some(TRUST_THIS_MAC));
        assert_eq!(connected(&fixture("network")).count(), 0);
    }

    #[test]
    fn only_real_iphones_that_are_around_are_listed() {
        let json = br#"{"result":{"devices":[
            {"hardwareProperties":{"reality":"virtual","deviceType":"iPhone","udid":"AAAA"},"connectionProperties":{"transportType":"wired","pairingState":"paired"}},
            {"hardwareProperties":{"reality":"physical","deviceType":"iPad","udid":"BBBB"},"connectionProperties":{"transportType":"wired","pairingState":"paired"}},
            {"hardwareProperties":{"reality":"physical","deviceType":"iPhone","udid":"CCCC"},"connectionProperties":{"pairingState":"paired"}},
            {"hardwareProperties":{"reality":"physical","deviceType":"iPhone","udid":"../x"},"connectionProperties":{"transportType":"wired","pairingState":"paired"}},
            {"hardwareProperties":{"reality":"physical","deviceType":"iPhone","udid":"DDDD"},"connectionProperties":{"transportType":"wired","pairingState":"paired"}},
            "a row of another shape",
            {"hardwareProperties":{"reality":"physical","deviceType":"iPhone","udid":"EEEE","marketingName":"iPhone 17"},"connectionProperties":{"transportType":"wired","pairingState":"paired"},"deviceProperties":{}}
        ]}}"#;
        let rows = parse_list(json).unwrap();
        let ids: Vec<_> = rows.iter().map(|d| d.udid.as_str()).collect();
        assert_eq!(ids, ["iosdev:DDDD", "iosdev:EEEE"]);
        assert_eq!((rows[0].name.as_str(), rows[0].runtime.as_str()), ("iPhone", "iOS"));
        assert_eq!(rows[1].name, "iPhone 17");
        assert!(parse_list(b"not json").is_err());
        assert!(parse_list(b"{}").unwrap().is_empty());
    }

    #[test]
    fn only_a_well_formed_iphone_id_reaches_devicectl() {
        assert_eq!(hardware_udid(&DeviceId("iosdev:00008130-ABC".into())), Some("00008130-ABC"));
        for bad in ["iosdev:", "iosdev:--device", "iosdev:a b", "adb:R58", "00008130-ABC"] {
            assert_eq!(hardware_udid(&DeviceId(bad.into())), None, "{bad}");
        }
    }

    #[test]
    fn common_failures_read_plainly() {
        assert!(hint("ERROR: The device is locked.").unwrap().contains("unlock"));
        assert!(hint("Developer Mode is disabled").unwrap().contains("Developer Mode"));
        assert!(hint("Unable to locate application com.x").unwrap().contains("not installed"));
        assert_eq!(hint("something else"), None);
    }
}
