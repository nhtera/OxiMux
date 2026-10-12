//! Emulators (AVDs): listing them, and the command line that boots one
//! headless. The boot itself (a long-running `emulator` child, then polling
//! `sys.boot_completed`) is the backend's.

use std::path::Path;
use std::time::Duration;

use crate::Result;
use crate::runner::Runner;

/// How long a cold AVD boot may take before we give up.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(180);

/// `emulator -list-avds`: one name per line. The emulator prints its own
/// notices (`INFO    | …`, `WARNING | …`) to stdout too; AVD names cannot
/// contain spaces, so any line with one is chatter.
pub fn parse_list_avds(out: &str) -> Vec<String> {
    out.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains(char::is_whitespace)).map(str::to_owned).collect()
}

pub fn list_avds(runner: &dyn Runner, emulator: &Path, timeout: Duration) -> Result<Vec<String>> {
    let out = runner.run(&emulator.to_string_lossy(), &["-list-avds"], None, timeout)?.into_success("emulator")?;
    Ok(parse_list_avds(&out.stdout_str()))
}

/// The emulator's arguments for a headless boot: no window (the panel is the
/// screen), no audio, no boot animation. `wipe`: back to factory settings
/// first (an erase), which also skips the quick-boot snapshot.
pub fn boot_args(name: &str, wipe: bool) -> Vec<String> {
    let mut args: Vec<String> = ["-avd", name, "-no-window", "-no-audio", "-no-boot-anim"].into_iter().map(str::to_owned).collect();
    if wipe {
        args.push("-wipe-data".into());
    }
    args
}

/// Whether an emulator still holds AVD `name` (under `home`): its
/// `hardware-qemu.ini.lock` names the emulator's pid. The file outlives the
/// emulator, so only a live pid counts — an emulator that dropped off adb can
/// hold the AVD for seconds more while it saves its snapshot.
pub fn held(home: &Path, name: &str) -> bool {
    let lock = home.join(format!("{name}.avd")).join("hardware-qemu.ini.lock");
    let pid = std::fs::read_to_string(lock).ok().and_then(|s| s.trim_matches(|c: char| c == '\0' || c.is_whitespace()).parse::<i32>().ok());
    pid.is_some_and(alive)
}

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(not(unix))]
fn alive(_pid: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_avd_is_held_only_while_its_lock_names_a_live_process() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("Pixel.avd");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!held(home.path(), "Pixel"), "no lock file");
        // The emulator writes its pid NUL-terminated.
        std::fs::write(dir.join("hardware-qemu.ini.lock"), format!("{}\0", std::process::id())).unwrap();
        assert_eq!(held(home.path(), "Pixel"), cfg!(unix), "this process is alive");
        std::fs::write(dir.join("hardware-qemu.ini.lock"), "99999999\0").unwrap();
        assert!(!held(home.path(), "Pixel"), "a stale lock");
    }

    #[test]
    fn avd_names_are_listed_without_the_emulators_notices() {
        let out = "INFO    | Storing crashdata in: /tmp/android-x/emu-crash.db\nMedium_Phone\nPixel_9_API_36\n\n";
        assert_eq!(parse_list_avds(out), ["Medium_Phone", "Pixel_9_API_36"]);
        assert!(parse_list_avds("").is_empty());
    }

    #[test]
    fn a_boot_is_headless() {
        assert_eq!(boot_args("Medium_Phone", false), ["-avd", "Medium_Phone", "-no-window", "-no-audio", "-no-boot-anim"]);
        assert_eq!(boot_args("Medium_Phone", true).last().map(String::as_str), Some("-wipe-data"));
    }
}
