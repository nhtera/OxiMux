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
use super::wifi;
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

/// Phones OxiMux paired over Wi-Fi: mDNS instance (`adb-<id>`) → the phone's
/// own serial. Only these network transports join the phone's row; the app
/// loads it from its database (never from a file agents can write) and
/// keeps it current as phones are paired and forgotten.
pub type PairedPhones = Mutex<HashMap<String, String>>;

pub fn paired_phones() -> &'static PairedPhones {
    static PAIRED: OnceLock<PairedPhones> = OnceLock::new();
    PAIRED.get_or_init(PairedPhones::default)
}

/// `ip:port` transports OxiMux itself connected this run, → the serial it read
/// over them. Memory only: an address is never remembered (its port changes),
/// and nothing announced on the network can add to this.
pub fn connected_here() -> &'static PairedPhones {
    static CONNECTED: OnceLock<PairedPhones> = OnceLock::new();
    CONNECTED.get_or_init(PairedPhones::default)
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
    let all = adb.devices(timeout)?;
    Ok(identify(&adb, all, known, paired_phones(), connected_here()))
}

/// The online devices of one `adb devices` listing, by the id the panel uses.
/// A phone is one id across its transports: USB first, then a Wi-Fi
/// transport OxiMux paired — adb's reconnect under the instance in `paired`,
/// or the address OxiMux connected this run (`connected`). Any other network
/// transport keeps an id of its own: what the network announces never merges
/// anything.
fn identify(
    adb: &Adb<'_>,
    all: Vec<AdbDevice>,
    known: &Identities,
    paired: &PairedPhones,
    connected: &PairedPhones,
) -> BTreeMap<DeviceId, AdbDevice> {
    let all_serials: Vec<String> = all.iter().map(|d| d.serial.clone()).collect();
    let mut online: Vec<AdbDevice> = all.into_iter().filter(|d| d.state == AdbState::Device).collect();
    // USB before Wi-Fi: a phone on both streams over the cable.
    online.sort_by_key(|d| wifi::is_network_transport(&d.serial));
    let paired = paired.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let connected = {
        let mut connected = connected.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // An address adb no longer lists is forgotten: its port may go to
        // another phone later this run.
        connected.retain(|addr, _| all_serials.iter().any(|s| s == addr));
        connected.clone()
    };
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
        } else if wifi::is_network_transport(&device.serial) {
            let serial = match wifi::tls_instance(&device.serial) {
                Some(instance) => paired.get(instance).cloned(),
                None => connected.get(&device.serial).cloned(),
            };
            match serial {
                // A phone this Mac paired: its own row (behind USB if both).
                Some(serial) => {
                    out.entry(Target::Serial(serial).id()).or_insert(device);
                    continue;
                }
                None => Target::Serial(device.serial.clone()),
            }
        } else {
            Target::Serial(device.serial.clone())
        };
        out.insert(target.id(), device);
    }
    out
}

/// A network transport OxiMux did not pair (`adb tcpip`, another tool's
/// pairing): listed on its own, never merged into a phone's row.
pub const NOT_PAIRED_HERE: &str = "Not paired by OxiMux";

/// Phones adb lists that cannot be used yet, as disabled rows with why: one
/// that has not approved this Mac, or an offline one. Emulators are left
/// out (an offline one is still booting).
fn unavailable(all: &[AdbDevice]) -> Vec<DeviceInfo> {
    all.iter()
        // A Wi-Fi transport that is not online is a dead one (wireless
        // debugging toggled, the port moved): no row, no "reconnect the cable".
        .filter(|d| !d.is_emulator() && !wifi::is_network_transport(&d.serial))
        .filter_map(|d| {
            let (state, note) = match &d.state {
                AdbState::Device => return None,
                AdbState::Unauthorized => ("Unauthorized".to_owned(), "Unlock the phone and tap “Allow USB debugging”".to_owned()),
                AdbState::Offline => ("Offline".to_owned(), "Reconnect the cable".to_owned()),
                AdbState::Other(state) => (state.clone(), format!("adb lists it as “{state}”")),
            };
            let mut row = info(Target::Serial(d.serial.clone()).id(), d.display_name(), None, false);
            row.is_available = false;
            // What an agent's `sim devices` shows: not "Shutdown".
            row.state = DeviceState::Other(state);
            row.note = Some(note);
            Some(row)
        })
        .collect()
}

/// Every phone adb lists, by serial, in whatever state it is in — offline
/// and unauthorized included (the watcher's cue that one was plugged in,
/// unplugged, or approved this Mac). Emulators are left out.
pub fn phone_states(runner: &dyn Runner, sdk: &Sdk, timeout: Duration) -> Result<BTreeMap<String, AdbState>> {
    let adb = Adb::new(runner, &sdk.adb());
    Ok(adb.devices(timeout)?.into_iter().filter(|d| !d.is_emulator()).map(|d| (d.serial, d.state)).collect())
}

/// Whether the phone `serial`'s screen is on (`None`: it did not say). A
/// sleeping phone streams nothing, silently: the panel says so instead.
pub fn screen_awake(runner: &dyn Runner, sdk: &Sdk, serial: &str, timeout: Duration) -> Option<bool> {
    let out = Adb::new(runner, &sdk.adb()).shell(serial, &["dumpsys", "power", "|", "grep", "mWakefulness="], timeout).ok()?;
    super::adb::parse_awake(&out)
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
    let all = Adb::new(runner, &sdk.adb()).devices(timeout)?;
    let waiting = unavailable(&all);
    let running = identify(&Adb::new(runner, &sdk.adb()), all, known, paired_phones(), connected_here());
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
        let adb = Adb::new(runner, &sdk.adb());
        let name = match Target::from_id(id) {
            // The name on the box ("Redmi Note 14"), not adb's model code.
            Some(Target::Serial(_)) => adb
                .getprop(&device.serial, "ro.product.marketname", QUICK)
                .ok()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| device.display_name()),
            Some(Target::Avd(name)) if !avds.contains(&name) => name.replace('_', " "),
            _ => continue,
        };
        let release = adb.getprop(&device.serial, "ro.build.version.release", QUICK).ok().filter(|v| !v.is_empty());
        let mut row = info(id.clone(), name, release.map(|r| format!("Android {r}")), true);
        if let Some(Target::Serial(serial)) = Target::from_id(id)
            && wifi::is_network_transport(&serial)
        {
            row.note = Some(NOT_PAIRED_HERE.into());
        }
        out.push(row);
    }
    out.extend(waiting);
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
        note: None,
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
            .expect(&format!("{adb} -s 0A1B2C shell getprop ro.product.marketname"), CmdOutput::ok("\n"))
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
                ("adb:R58".into(), "R58".into(), "Android".into(), DeviceState::Other("Unauthorized".into())),
            ]
        );
        // A phone that has not approved this Mac is listed, disabled, with why.
        let waiting = list.iter().find(|d| d.udid.as_str() == "adb:R58").unwrap();
        assert!(!waiting.is_available);
        assert!(waiting.note.as_deref().is_some_and(|n| n.contains("Allow USB debugging")));
        assert!(list.iter().filter(|d| d.udid.as_str() != "adb:R58").all(|d| d.is_available && d.note.is_none()));
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

    /// The watcher's view of phones: every state, so a phone that has not
    /// approved this Mac yet (or just did) is news; emulators are not phones.
    #[test]
    fn phone_states_keep_unauthorized_phones_and_skip_emulators() {
        let dir = tempfile::tempdir().unwrap();
        let sdk = sdk(dir.path());
        let adb = sdk.adb().display().to_string();
        let listing = "List of devices attached
emulator-5554 device transport_id:1
R58 unauthorized usb:2 transport_id:3
0A1B2C offline transport_id:4
";
        let runner = ScriptedRunner::default().expect(&format!("{adb} devices -l"), CmdOutput::ok(listing));
        let states = phone_states(&runner, &sdk, QUICK).unwrap();
        assert_eq!(states.into_iter().collect::<Vec<_>>(), [("0A1B2C".into(), AdbState::Offline), ("R58".into(), AdbState::Unauthorized)]);
    }

    /// One phone, one row: a Wi-Fi transport OxiMux paired joins the phone's
    /// id (behind USB when both are up) — under adb's mDNS name, or as the
    /// address OxiMux connected. Anything else, even an address the network
    /// announces under a paired name, stays on its own; no mDNS is asked.
    #[test]
    fn a_paired_wifi_phone_shares_its_usb_row() {
        let dir = tempfile::tempdir().unwrap();
        let sdk = sdk(dir.path());
        let paired = PairedPhones::new(HashMap::from([("adb-R58-x1Y2".to_owned(), "R58".to_owned())]));
        let connected = || PairedPhones::new(HashMap::from([("192.168.1.5:37123".to_owned(), "R58".to_owned())]));
        let device = |serial: &str| AdbDevice { serial: serial.into(), state: AdbState::Device, model: None };
        let runner = ScriptedRunner::default(); // no adb call at all
        let adb = Adb::new(&runner, &sdk.adb());

        let both = vec![device("adb-R58-x1Y2._adb-tls-connect._tcp"), device("R58"), device("192.168.1.9:5555")];
        let ids = identify(&adb, both, &Identities::default(), &paired, &connected());
        let got: Vec<(String, String)> = ids.iter().map(|(id, d)| (id.0.clone(), d.serial.clone())).collect();
        assert_eq!(got, [("adb:192.168.1.9:5555".to_owned(), "192.168.1.9:5555".to_owned()), ("adb:R58".into(), "R58".into())], "USB wins; the tcpip one is its own");

        // Wi-Fi only, by the address OxiMux connected: the phone's row.
        let ids = identify(&adb, vec![device("192.168.1.5:37123")], &Identities::default(), &paired, &connected());
        assert_eq!(ids.get(&DeviceId("adb:R58".into())).map(|d| d.serial.as_str()), Some("192.168.1.5:37123"));
        // Any other address keeps its own row, whatever is announced for it.
        let known = connected();
        let ids = identify(&adb, vec![device("192.168.1.66:5555")], &Identities::default(), &paired, &known);
        assert!(ids.contains_key(&DeviceId("adb:192.168.1.66:5555".into())) && !ids.contains_key(&DeviceId("adb:R58".into())));
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        // That listing no longer had 192.168.1.5:37123: forgotten, so another
        // phone given that address later is not taken for this one.
        assert!(known.lock().unwrap().is_empty());
    }

    /// A dead Wi-Fi transport (wireless debugging toggled) leaves no row.
    #[test]
    fn a_dead_wifi_transport_is_not_listed() {
        let rows = unavailable(&[AdbDevice { serial: "192.168.1.5:37123".into(), state: AdbState::Offline, model: None }]);
        assert!(rows.is_empty());
    }

    #[test]
    fn an_avd_target_parses() {
        assert_eq!(parse_avd_target("hw.lcd.width=1080\ntarget=android-36\n").as_deref(), Some("Android API 36"));
        assert_eq!(parse_avd_target("hw.lcd.width=1080\n"), None);
    }
}
