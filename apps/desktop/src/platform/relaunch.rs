//! Relaunching the app after it quits, for "restart to update".
//!
//! # Why a detached shell instead of just spawning ourselves
//!
//! Two constraints have to hold at once. The staged update is swapped in
//! during the quit path, so the relaunch must happen *after* this process is
//! gone or it would launch the old bundle. And the single-instance guard
//! (`platform::single_instance`) holds a `flock` for the process's lifetime —
//! a new instance started too early sees `AlreadyRunning`, foregrounds the
//! dying process, and exits, leaving the user with nothing.
//!
//! Both are satisfied by waiting for the old pid to disappear: the kernel
//! releases the flock on exit, so the moment the pid is gone the lock is free.
//! A tiny `sh` in its own session outlives us to do that waiting.
//!
//! The wait is bounded. A wedged quit must not strand an immortal sleeper
//! polling forever; on timeout the helper exits without launching, and the
//! user relaunches by hand — into the new version, since the swap either
//! already happened or will on the next clean quit.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Poll interval and cap: 0.2s × 150 ≈ 30s of patience.
const POLL_SECONDS: &str = "0.2";
const MAX_POLLS: u32 = 150;

/// The script the helper runs. Split out so the logic is testable without
/// spawning anything.
///
/// Absolute tool paths throughout: a GUI-launched app inherits no `PATH`, and
/// this shell is its child.
fn relaunch_script(bundle_root: &Path, old_pid: u32) -> String {
    // The path is our own validated install root today, but it is still
    // interpolated into a shell string — escape it so that stays true if the
    // caller ever changes. In single quotes only `'` itself can escape.
    let quoted = bundle_root.display().to_string().replace('\'', r"'\''");
    format!(
        "i=0; \
         while /bin/kill -0 {old_pid} 2>/dev/null; do \
           /bin/sleep {POLL_SECONDS}; \
           i=$((i+1)); \
           if [ $i -ge {MAX_POLLS} ]; then exit 0; fi; \
         done; \
         exec /usr/bin/open -n '{quoted}'"
    )
}

/// Spawn the detached helper that relaunches `bundle_root` once this process
/// exits. Returns once the helper is running — the caller then quits.
pub fn spawn_relaunch_helper(bundle_root: &Path) -> std::io::Result<()> {
    let script = relaunch_script(bundle_root, std::process::id());
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Own session: otherwise the helper dies with the terminal or login
        // session that started us, which is exactly when it is needed.
        .process_group(0)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_waits_for_the_pid_then_opens_a_fresh_instance() {
        let script = relaunch_script(Path::new("/Applications/OxiMux.app"), 4242);
        assert!(script.contains("/bin/kill -0 4242"), "{script}");
        // `-n` is what makes it a *new* instance rather than an activate of
        // whatever LaunchServices thinks is running.
        assert!(script.contains("/usr/bin/open -n"), "{script}");
        assert!(script.contains("/Applications/OxiMux.app"), "{script}");
    }

    #[test]
    fn the_wait_is_bounded() {
        // Without the cap, a quit that never completes leaves a shell polling
        // for the life of the login session.
        let script = relaunch_script(Path::new("/Applications/OxiMux.app"), 1);
        assert!(script.contains(&format!("-ge {MAX_POLLS}")), "{script}");
        assert!(script.contains("exit 0"), "{script}");
    }

    #[test]
    fn a_quote_in_the_path_cannot_break_out_of_the_string() {
        let script = relaunch_script(Path::new("/Apps/We'ird.app"), 9);
        assert!(script.contains(r"'\''"), "quote not escaped in {script}");
        // Nothing after the escaped quote may read as a new shell word.
        assert!(script.ends_with("'"), "{script}");
    }

    #[test]
    fn absolute_tool_paths_only() {
        // A GUI launch has no PATH; bare `kill`/`sleep`/`open` would not
        // resolve.
        let script = relaunch_script(Path::new("/Applications/OxiMux.app"), 7);
        for tool in ["/bin/kill", "/bin/sleep", "/usr/bin/open"] {
            assert!(script.contains(tool), "missing {tool} in {script}");
        }
    }
}
