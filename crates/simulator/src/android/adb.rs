//! `adb` as data: the device list, properties, and the handful of calls the
//! backend makes. Every call names its device with `-s <serial>`, so a second
//! phone plugged in never receives another device's input.

use std::path::Path;
use std::time::Duration;

use crate::runner::{CmdOutput, Runner};
use crate::{Result, SimError};

/// One line of `adb devices -l`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: AdbState,
    /// `model:` (underscores for spaces, as adb prints it), if listed.
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdbState {
    /// Online and authorized.
    Device,
    Offline,
    /// The phone has not accepted this computer's USB debugging key yet.
    Unauthorized,
    Other(String),
}

impl AdbDevice {
    /// An emulator adb reaches over its console port (`emulator-5554`).
    pub fn is_emulator(&self) -> bool {
        self.serial.starts_with("emulator-")
    }

    /// A name for people: the model with spaces, else the serial.
    pub fn display_name(&self) -> String {
        self.model.as_deref().map_or_else(|| self.serial.clone(), |m| m.replace('_', " "))
    }
}

/// Parse `adb devices -l`. Header, daemon chatter (`* daemon started…`) and
/// blank lines are skipped.
pub fn parse_devices(out: &str) -> Vec<AdbDevice> {
    out.lines()
        .filter(|line| !line.starts_with("List of devices") && !line.starts_with('*'))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let serial = words.next()?.to_owned();
            let state = match words.next()? {
                "device" => AdbState::Device,
                "offline" => AdbState::Offline,
                "unauthorized" => AdbState::Unauthorized,
                other => AdbState::Other(other.to_owned()),
            };
            let model = words.find_map(|w| w.strip_prefix("model:")).map(str::to_owned);
            Some(AdbDevice { serial, state, model })
        })
        .collect()
}

/// The AVD name from `adb -s <emulator> emu avd name` (`Medium_Phone\r\nOK`).
pub fn parse_avd_name(out: &str) -> Option<String> {
    if console_refused(out) {
        return None;
    }
    out.lines().map(str::trim).find(|l| !l.is_empty() && *l != "OK").map(str::to_owned)
}

/// The emulator console's refusal (`KO: …`), which adb reports with exit 0.
pub fn console_refused(out: &str) -> bool {
    out.lines().any(|l| l.trim_start().starts_with("KO"))
}

/// The local port `adb forward tcp:0 …` picked (it prints it).
pub fn parse_forward_port(out: &str) -> Option<u16> {
    out.trim().parse().ok()
}

/// The display size from `wm size`: an override (set by `wm size WxH`, what
/// apps and screenshots use) wins over the physical size.
pub fn parse_wm_size(out: &str) -> Option<(u32, u32)> {
    let size = |prefix: &str| {
        out.lines().find_map(|l| l.trim().strip_prefix(prefix)).and_then(|v| {
            let (w, h) = v.trim().split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
    };
    size("Override size:").or_else(|| size("Physical size:"))
}

/// The display density (dpi) from `wm density`, override first.
pub fn parse_wm_density(out: &str) -> Option<u32> {
    let density = |prefix: &str| out.lines().find_map(|l| l.trim().strip_prefix(prefix)).and_then(|v| v.trim().parse().ok());
    density("Override density:").or_else(|| density("Physical density:"))
}

/// `adb` at a known path.
pub struct Adb<'a> {
    runner: &'a dyn Runner,
    program: String,
}

impl<'a> Adb<'a> {
    pub fn new(runner: &'a dyn Runner, adb: &Path) -> Self {
        Self { runner, program: adb.to_string_lossy().into_owned() }
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Result<CmdOutput> {
        self.runner.run(&self.program, args, None, timeout)?.into_success("adb")
    }

    fn on(&self, serial: &str, args: &[&str], timeout: Duration) -> Result<CmdOutput> {
        let mut all = vec!["-s", serial];
        all.extend_from_slice(args);
        self.run(&all, timeout)
    }

    pub fn devices(&self, timeout: Duration) -> Result<Vec<AdbDevice>> {
        Ok(parse_devices(&self.run(&["devices", "-l"], timeout)?.stdout_str()))
    }

    /// `adb -s S shell <args…>`: stdout. (adb's shell joins the words; pass
    /// nothing that needs quoting.)
    pub fn shell(&self, serial: &str, args: &[&str], timeout: Duration) -> Result<String> {
        let mut all = vec!["shell"];
        all.extend_from_slice(args);
        Ok(self.on(serial, &all, timeout)?.stdout_str())
    }

    pub fn getprop(&self, serial: &str, key: &str, timeout: Duration) -> Result<String> {
        Ok(self.shell(serial, &["getprop", key], timeout)?.trim().to_owned())
    }

    /// The AVD an emulator serial is running, from its console.
    pub fn avd_name(&self, serial: &str, timeout: Duration) -> Result<String> {
        let out = self.on(serial, &["emu", "avd", "name"], timeout)?.stdout_str();
        parse_avd_name(&out).ok_or_else(|| SimError::Parse { what: "adb emu avd name".into(), detail: out })
    }

    pub fn push(&self, serial: &str, local: &Path, remote: &str, timeout: Duration) -> Result<()> {
        self.on(serial, &["push", &local.to_string_lossy(), remote], timeout).map(drop)
    }

    /// `adb forward tcp:0 <remote>`: the local port adb chose.
    pub fn forward(&self, serial: &str, remote: &str, timeout: Duration) -> Result<u16> {
        let out = self.on(serial, &["forward", "tcp:0", remote], timeout)?.stdout_str();
        parse_forward_port(&out).ok_or_else(|| SimError::Parse { what: "adb forward".into(), detail: out })
    }

    pub fn forward_remove(&self, serial: &str, port: u16, timeout: Duration) -> Result<()> {
        self.on(serial, &["forward", "--remove", &format!("tcp:{port}")], timeout).map(drop)
    }

    /// `adb exec-out screencap -p`: the PNG bytes (exec-out keeps them binary).
    pub fn screencap_png(&self, serial: &str, timeout: Duration) -> Result<Vec<u8>> {
        Ok(self.on(serial, &["exec-out", "screencap", "-p"], timeout)?.stdout)
    }

    /// Shut an emulator down through its console (`emu kill`): a graceful
    /// exit that keeps its quick-boot snapshot. The console wants the user's
    /// auth token (`~/.emulator_console_auth_token`) and answers `KO:` — with
    /// exit 0 — without it; that is an error here, never a reason to power the
    /// guest off instead: an emulator exiting from a powered-off guest saves
    /// that as its quick-boot snapshot, and every later boot resumes a system
    /// with no adb (found live).
    pub fn emu_kill(&self, serial: &str, timeout: Duration) -> Result<()> {
        let out = self.on(serial, &["emu", "kill"], timeout)?.stdout_str();
        if console_refused(&out) {
            return Err(SimError::CommandFailed {
                program: "adb emu kill".into(),
                code: None,
                stderr: "the emulator console refused the command (no console auth token)".into(),
            });
        }
        Ok(())
    }

    /// `adb -s S install -r <apk>` (replacing an installed copy).
    pub fn install(&self, serial: &str, apk: &Path, timeout: Duration) -> Result<()> {
        self.on(serial, &["install", "-r", &apk.to_string_lossy()], timeout).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const DEVICES: &str = "* daemon not running; starting now at tcp:5037\n* daemon started successfully\n\
List of devices attached\n\
emulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\n\
R58M123ABC             unauthorized usb:1-1 transport_id:2\n\
0A1B2C3D               device usb:1-2 product:husky model:Pixel_8_Pro device:husky transport_id:3\n\
emulator-5556          offline transport_id:4\n\n";

    #[test]
    fn the_device_list_parses_every_state() {
        let devices = parse_devices(DEVICES);
        assert_eq!(devices.len(), 4);
        assert_eq!(devices[0].serial, "emulator-5554");
        assert!(devices[0].is_emulator());
        assert_eq!(devices[0].state, AdbState::Device);
        assert_eq!(devices[1].state, AdbState::Unauthorized);
        assert_eq!(devices[1].model, None);
        assert_eq!(devices[1].display_name(), "R58M123ABC");
        assert_eq!(devices[2].display_name(), "Pixel 8 Pro");
        assert!(!devices[2].is_emulator());
        assert_eq!(devices[3].state, AdbState::Offline);
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn console_and_forward_replies_parse() {
        assert_eq!(parse_avd_name("Medium_Phone\r\nOK\r\n").as_deref(), Some("Medium_Phone"));
        assert_eq!(parse_avd_name("OK\r\n"), None);
        assert_eq!(parse_forward_port("51234\n"), Some(51234));
        assert_eq!(parse_forward_port("error"), None);
        assert!(console_refused("KO: unknown command, try 'help'\n"));
        assert!(!console_refused("OK: killing emulator, bye bye\r\n"));
    }

    #[test]
    fn display_size_and_density_prefer_the_override() {
        assert_eq!(parse_wm_size("Physical size: 1080x2400\n"), Some((1080, 2400)));
        assert_eq!(parse_wm_size("Physical size: 1080x2400\nOverride size: 720x1600\n"), Some((720, 1600)));
        assert_eq!(parse_wm_size("error"), None);
        assert_eq!(parse_wm_density("Physical density: 420\n"), Some(420));
        assert_eq!(parse_wm_density("Physical density: 420\nOverride density: 480\n"), Some(480));
    }

    #[test]
    fn every_call_names_its_device() {
        let runner = ScriptedRunner::default()
            .expect("/sdk/adb -s emulator-5554 shell getprop sys.boot_completed", CmdOutput::ok("1\n"))
            .expect("/sdk/adb -s emulator-5554 forward tcp:0 localabstract:scrcpy_0000abcd", CmdOutput::ok("50123\n"));
        let adb = Adb::new(&runner, Path::new("/sdk/adb"));
        let t = Duration::from_secs(5);
        assert_eq!(adb.getprop("emulator-5554", "sys.boot_completed", t).unwrap(), "1");
        assert_eq!(adb.forward("emulator-5554", "localabstract:scrcpy_0000abcd", t).unwrap(), 50123);
    }
}
