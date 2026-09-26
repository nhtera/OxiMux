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
/// screen), no audio, no boot animation.
pub fn boot_args(name: &str) -> Vec<String> {
    ["-avd", name, "-no-window", "-no-audio", "-no-boot-anim"].into_iter().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avd_names_are_listed_without_the_emulators_notices() {
        let out = "INFO    | Storing crashdata in: /tmp/android-x/emu-crash.db\nMedium_Phone\nPixel_9_API_36\n\n";
        assert_eq!(parse_list_avds(out), ["Medium_Phone", "Pixel_9_API_36"]);
        assert!(parse_list_avds("").is_empty());
    }

    #[test]
    fn a_boot_is_headless() {
        assert_eq!(boot_args("Medium_Phone"), ["-avd", "Medium_Phone", "-no-window", "-no-audio", "-no-boot-anim"]);
    }
}
