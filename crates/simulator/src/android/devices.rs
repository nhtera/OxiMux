//! Android devices as the panel sees them: every AVD (booted or not) and
//! every phone adb lists, as [`DeviceInfo`]s with stable ids, plus booting an
//! AVD headless and finding the adb serial a running one got this time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Target;
use super::adb::{Adb, AdbDevice, AdbState};
use super::avd;
use super::sdk::Sdk;
use crate::runner::Runner;
use crate::{DeviceId, DeviceInfo, DeviceKind, DeviceState, Result, SimError};

const QUICK: Duration = Duration::from_secs(10);

/// Which AVD each running emulator serial is, learned once per boot: asking
/// again every poll costs a process per emulator, and one slow answer must not
/// make a known emulator look like another device.
pub type Identities = Mutex<HashMap<String, String>>;

fn identities() -> &'static Identities {
    static IDENTITIES: OnceLock<Identities> = OnceLock::new();
    IDENTITIES.get_or_init(Identities::default)
}

/// The serial an AVD was last seen running under (no adb call: for quitting).
pub fn known_serial(avd: &str) -> Option<String> {
    let known = identities().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    known.iter().find(|(_, name)| name.as_str() == avd).map(|(serial, _)| serial.clone())
}

/// The running devices adb sees, by the id the panel uses: an emulator by its
/// AVD (the emulator's own `ro.boot.qemu.avd_name`, else its console — which
/// needs the user's console token), a phone by serial. Unauthorized and offline
/// devices are left out (they cannot be streamed).
pub fn running(runner: &dyn Runner, sdk: &Sdk, timeout: Duration) -> Result<BTreeMap<DeviceId, AdbDevice>> {
    running_with(runner, sdk, timeout, identities())
}

/// [`running`] with its identity cache passed in (tests).
pub fn running_with(runner: &dyn Runner, sdk: &Sdk, timeout: Duration, known: &Identities) -> Result<BTreeMap<DeviceId, AdbDevice>> {
    let adb = Adb::new(runner, &sdk.adb());
    let online: Vec<AdbDevice> = adb.devices(timeout)?.into_iter().filter(|d| d.state == AdbState::Device).collect();
    let mut known = known.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // A serial that went away (or is booting again, offline) may come back
    // as another AVD: forget it.
    known.retain(|serial, _| online.iter().any(|d| &d.serial == serial));
    let mut out = BTreeMap::new();
    for device in online {
        let target = if device.is_emulator() {
            let name = match known.get(&device.serial) {
                Some(name) => Ok(name.clone()),
                None => {
                    let prop = adb.getprop(&device.serial, "ro.boot.qemu.avd_name", QUICK).ok().filter(|n| !n.is_empty());
                    prop.map(Ok).unwrap_or_else(|| adb.avd_name(&device.serial, QUICK))
                }
            };
            match name {
                Ok(name) => {
                    known.insert(device.serial.clone(), name.clone());
                    Target::Avd(name)
                }
                // Not known yet: left out this round rather than listed under
                // a second, serial-based id.
                Err(e) => {
                    tracing::debug!(serial = %device.serial, "emulator identity: {e}");
                    continue;
                }
            }
        } else {
            Target::Serial(device.serial.clone())
        };
        out.insert(target.id(), device);
    }
    Ok(out)
}

/// The ids of every running Android device (the device watcher's view).
pub fn booted_ids(runner: &dyn Runner, sdk: &Sdk, timeout: Duration) -> Result<BTreeSet<DeviceId>> {
    Ok(running(runner, sdk, timeout)?.into_keys().collect())
}

/// Every AVD and every running device, for the device menu.
pub fn list(runner: &dyn Runner, sdk: &Sdk, avd_home: Option<&Path>, timeout: Duration) -> Result<Vec<DeviceInfo>> {
    list_with(runner, sdk, avd_home, timeout, identities())
}

fn list_with(runner: &dyn Runner, sdk: &Sdk, avd_home: Option<&Path>, timeout: Duration, known: &Identities) -> Result<Vec<DeviceInfo>> {
    let running = running_with(runner, sdk, timeout, known)?;
    let avds = if sdk.has_emulator() { avd::list_avds(runner, &sdk.emulator(), timeout).unwrap_or_default() } else { Vec::new() };
    let mut out: Vec<DeviceInfo> = avds
        .iter()
        .map(|name| {
            let id = Target::Avd(name.clone()).id();
            let booted = running.contains_key(&id);
            let api = avd_home.and_then(|home| avd_api(home, name));
            info(id, name.replace('_', " "), api, booted)
        })
        .collect();
    for (id, device) in &running {
        // Phones, and an emulator whose AVD lives somewhere `-list-avds` does
        // not look (another AVD home): listed as they run.
        let name = match Target::from_id(id) {
            Some(Target::Serial(_)) => device.display_name(),
            Some(Target::Avd(name)) if !avds.contains(&name) => name.replace('_', " "),
            _ => continue,
        };
        let adb = Adb::new(runner, &sdk.adb());
        let release = adb.getprop(&device.serial, "ro.build.version.release", QUICK).ok().filter(|v| !v.is_empty());
        out.push(info(id.clone(), name, release.map(|r| format!("Android {r}")), true));
    }
    Ok(out)
}

fn info(id: DeviceId, name: String, runtime: Option<String>, booted: bool) -> DeviceInfo {
    let runtime = runtime.unwrap_or_else(|| "Android".into());
    DeviceInfo {
        os_version: runtime.trim_start_matches("Android").trim().to_owned(),
        udid: id,
        name,
        runtime,
        state: if booted { DeviceState::Booted } else { DeviceState::Shutdown },
        kind: DeviceKind::Phone,
        is_available: true,
    }
}

/// Where AVDs live: `ANDROID_AVD_HOME`, else `~/.android/avd`.
pub fn avd_home() -> Option<PathBuf> {
    std::env::var_os("ANDROID_AVD_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".android/avd")))
}

/// "API 37.1" from an AVD's `config.ini` (`target=android-37.1`).
fn avd_api(home: &Path, name: &str) -> Option<String> {
    let config = std::fs::read_to_string(home.join(format!("{name}.avd")).join("config.ini")).ok()?;
    parse_avd_target(&config)
}

fn parse_avd_target(config: &str) -> Option<String> {
    config.lines().find_map(|l| l.strip_prefix("target=")).and_then(|t| t.trim().strip_prefix("android-")).map(|api| format!("Android API {api}"))
}

/// The adb serial `target` has right now, if it is running.
pub fn serial_of(runner: &dyn Runner, sdk: &Sdk, target: &Target, timeout: Duration) -> Result<Option<String>> {
    Ok(running(runner, sdk, timeout)?.remove(&target.id()).map(|d| d.serial))
}

/// How a boot went: OxiMux started the emulator (and owns it), or found it
/// already running (someone else's — never shut down on their behalf).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootOutcome {
    Booted(String),
    AlreadyBooted(String),
}

/// Boot AVD `name` headless and wait (≤ [`avd::BOOT_TIMEOUT`]) until Android
/// reports `sys.boot_completed`. The emulator runs on its own (OxiMux shuts it
/// down when it owns it); a thread reaps it when it exits. A boot we started
/// that fails or times out is killed — a windowless emulator nobody owns
/// would run on unseen.
pub fn boot_avd(runner: &dyn Runner, sdk: &Sdk, name: &str, cancel: &AtomicBool) -> Result<BootOutcome> {
    let target = Target::Avd(name.to_owned());
    if let Some(serial) = serial_of(runner, sdk, &target, QUICK)? {
        return wait_booted(runner, sdk, &target, Some(serial), cancel).map(BootOutcome::AlreadyBooted);
    }
    let mut child = Command::new(sdk.emulator())
        .args(avd::boot_args(name))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match wait_booted(runner, sdk, &target, None, cancel) {
        Ok(serial) => {
            std::thread::Builder::new().name("oximux-emulator-reaper".into()).spawn(move || {
                let _ = child.wait();
            })?;
            Ok(BootOutcome::Booted(serial))
        }
        Err(e) => {
            // A plain kill: nothing half-booted is saved as its snapshot.
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

fn wait_booted(runner: &dyn Runner, sdk: &Sdk, target: &Target, mut serial: Option<String>, cancel: &AtomicBool) -> Result<String> {
    let deadline = Instant::now() + avd::BOOT_TIMEOUT;
    let adb = Adb::new(runner, &sdk.adb());
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(SimError::Cancelled);
        }
        if serial.is_none() {
            serial = serial_of(runner, sdk, target, QUICK).ok().flatten();
        }
        if let Some(s) = &serial
            && adb.getprop(s, "sys.boot_completed", QUICK).is_ok_and(|v| v == "1")
        {
            return Ok(s.clone());
        }
        if Instant::now() >= deadline {
            return Err(SimError::Timeout { what: "the Android emulator boot".into(), secs: avd::BOOT_TIMEOUT.as_secs() });
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Shut an emulator down (`adb emu kill`). A phone is never shut down.
pub fn shutdown(runner: &dyn Runner, sdk: &Sdk, target: &Target, timeout: Duration) -> Result<()> {
    let Target::Avd(_) = target else { return Ok(()) };
    let Some(serial) = serial_of(runner, sdk, target, timeout)? else { return Ok(()) };
    Adb::new(runner, &sdk.adb()).emu_kill(&serial, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner};

    fn sdk(dir: &Path) -> Sdk {
        for tool in ["platform-tools/adb", "emulator/emulator"] {
            let path = dir.join(tool);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        Sdk { root: dir.to_path_buf() }
    }

    #[test]
    fn avds_and_phones_list_with_stable_ids() {
        let dir = tempfile::tempdir().unwrap();
        let sdk = sdk(dir.path());
        let (adb, emu) = (sdk.adb().display().to_string(), sdk.emulator().display().to_string());
        let devices = "List of devices attached\n\
emulator-5554 device product:sdk model:sdk_gphone64 transport_id:1\n\
0A1B2C device usb:1 model:Pixel_8_Pro transport_id:2\n\
R58 unauthorized usb:2 transport_id:3\n";
        let runner = ScriptedRunner::default()
            .expect(&format!("{adb} devices -l"), CmdOutput::ok(devices))
            .expect(&format!("{adb} -s emulator-5554 shell getprop ro.boot.qemu.avd_name"), CmdOutput::ok("\n"))
            .expect(&format!("{adb} -s emulator-5554 emu avd name"), CmdOutput::ok("Medium_Phone\r\nOK\r\n"))
            .expect(&format!("{emu} -list-avds"), CmdOutput::ok("Medium_Phone\nPixel_Tablet\n"))
            .expect(&format!("{adb} -s 0A1B2C shell getprop ro.build.version.release"), CmdOutput::ok("16\n"));
        let home = dir.path().join("avd");
        std::fs::create_dir_all(home.join("Medium_Phone.avd")).unwrap();
        std::fs::write(home.join("Medium_Phone.avd/config.ini"), "abi.type=arm64-v8a\ntarget=android-37.1\n").unwrap();

        let list = list_with(&runner, &sdk, Some(&home), QUICK, &Identities::default()).unwrap();
        let summary: Vec<(String, String, String, DeviceState)> =
            list.iter().map(|d| (d.udid.to_string(), d.name.clone(), d.runtime.clone(), d.state.clone())).collect();
        assert_eq!(
            summary,
            [
                ("avd:Medium_Phone".into(), "Medium Phone".into(), "Android API 37.1".into(), DeviceState::Booted),
                ("avd:Pixel_Tablet".into(), "Pixel Tablet".into(), "Android".into(), DeviceState::Shutdown),
                ("adb:0A1B2C".into(), "Pixel 8 Pro".into(), "Android 16".into(), DeviceState::Booted),
            ]
        );
        assert!(list.iter().all(|d| d.udid.platform() == crate::Platform::Android));
    }

    /// A known emulator keeps its AVD id without asking again, and one whose
    /// identity cannot be read is left out — never listed as a second device.
    #[test]
    fn emulator_identities_are_cached_and_never_downgraded() {
        let dir = tempfile::tempdir().unwrap();
        let sdk = sdk(dir.path());
        let adb = sdk.adb().display().to_string();
        let listing = "List of devices attached\nemulator-5554 device transport_id:1\nemulator-5556 device transport_id:2\n";
        let runner = ScriptedRunner::default()
            .expect(&format!("{adb} devices -l"), CmdOutput::ok(listing))
            .expect(&format!("{adb} -s emulator-5554 shell getprop ro.boot.qemu.avd_name"), CmdOutput::ok("Medium_Phone\n"))
            .expect(&format!("{adb} -s emulator-5556 shell getprop ro.boot.qemu.avd_name"), CmdOutput::failed(1, "timeout"))
            .expect(&format!("{adb} -s emulator-5556 emu avd name"), CmdOutput::ok("KO: unknown command\n"))
            // Second poll: 5554 is known (no getprop), 5556 still unknown.
            .expect(&format!("{adb} devices -l"), CmdOutput::ok(listing))
            .expect(&format!("{adb} -s emulator-5556 shell getprop ro.boot.qemu.avd_name"), CmdOutput::ok("Pixel_9\n"));
        let known = Identities::default();
        let first: Vec<String> = running_with(&runner, &sdk, QUICK, &known).unwrap().into_keys().map(|d| d.0).collect();
        assert_eq!(first, ["avd:Medium_Phone"], "the unreadable one is left out, not `adb:emulator-5556`");
        let second: Vec<String> = running_with(&runner, &sdk, QUICK, &known).unwrap().into_keys().map(|d| d.0).collect();
        assert_eq!(second, ["avd:Medium_Phone", "avd:Pixel_9"]);
    }

    #[test]
    fn an_avd_target_parses() {
        assert_eq!(parse_avd_target("hw.lcd.width=1080\ntarget=android-36\n").as_deref(), Some("Android API 36"));
        assert_eq!(parse_avd_target("hw.lcd.width=1080\n"), None);
    }
}
