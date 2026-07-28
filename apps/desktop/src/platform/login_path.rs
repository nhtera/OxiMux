//! Give a double-clicked app the `PATH` its owner actually has.
//!
//! # The failure this exists to stop
//!
//! Agent backends are spawned by bare name — `Command::new("claude")`,
//! `Command::new("codex")` — and so is the `which` that detects which ones are
//! installed. That resolves through the inherited `PATH`, and what a process
//! inherits depends entirely on how it was started. A shell launch inherits the
//! user's login `PATH`; a Finder, Dock, or Spotlight launch inherits
//! `/usr/bin:/bin:/usr/sbin:/sbin` and nothing else.
//!
//! Every agent CLI installs outside those four directories — `~/.local/bin`,
//! Homebrew, a Node version manager. So a double-clicked OxiMux cannot start a
//! single agent, and reports "Failed to start agent: spawn agent process" with
//! no hint that the cause is an environment variable.
//!
//! It hides well. Terminals keep working, because those are spawned through the
//! relay's login shell and pick the real `PATH` up on the way. Only the agent
//! path breaks — and never for a developer, who runs the binary from a shell
//! that already has it.
//!
//! # Why the whole process, rather than each spawn site
//!
//! Setting it once here fixes every present and future `Command::new`, and the
//! detection pass that decides which agents to offer. Threading an environment
//! through each call site would leave the next one to be written broken again,
//! and the symptom is far enough from the cause that it would not be noticed.

use std::process::Command;

/// What `launchd` hands a GUI app, and the signal that nothing else set one.
///
/// Matched exactly rather than sniffed for missing directories: a user whose
/// login `PATH` genuinely is this loses nothing, since adopting it is then a
/// no-op, while any inexact test would eventually re-run a login shell for
/// someone who did not need it.
const LAUNCHD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Adopt the login shell's `PATH` when this process was started without one.
///
/// Call before any thread exists — see the `unsafe` note below.
pub fn adopt_login_shell_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    if !looks_like_a_gui_launch(&current) {
        return;
    }
    let Some(login) = login_shell_path() else {
        // Nothing to adopt. Left alone deliberately: a wrong `PATH` is worse
        // than a bare one, and the agent pane already reports what it cannot
        // find.
        tracing::warn!("could not read the login shell's PATH; agent CLIs may not be found");
        return;
    };
    let merged = merged(&login, &current);
    // SAFETY: called from the top of `main`, before the tokio runtime and the
    // GPUI executor exist, so no other thread can be reading the environment.
    // `set_var` is only unsound when it races a concurrent read.
    unsafe { std::env::set_var("PATH", &merged) };
    tracing::info!("adopted the login shell's PATH for agent spawning");
}

/// Whether this process looks launched by the window server rather than a shell.
fn looks_like_a_gui_launch(path: &str) -> bool {
    path.is_empty() || path == LAUNCHD_PATH
}

/// Ask the user's login shell what `PATH` it sets up.
///
/// `-l` and not `-i`: the login files are where `PATH` is assembled, while an
/// interactive shell additionally loads prompts, completions, and whatever else
/// the user has, none of which is wanted on a boot path.
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let out = Command::new(shell).args(["-lc", "printf %s \"$PATH\""]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// The login `PATH`, then anything the process already had that it did not list.
///
/// Login entries come first so the user's own choice of which `claude` wins —
/// the same one their terminal would run. The inherited entries are kept rather
/// than replaced because the system directories must stay reachable even if a
/// login file rewrote `PATH` from scratch.
fn merged(login: &str, current: &str) -> String {
    let mut out: Vec<&str> = login.split(':').filter(|s| !s.is_empty()).collect();
    for entry in current.split(':').filter(|s| !s.is_empty()) {
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launchd_path_is_recognised_as_a_gui_launch() {
        assert!(looks_like_a_gui_launch(LAUNCHD_PATH));
        // An empty one means the variable was never set at all, which fails the
        // same way and has the same remedy.
        assert!(looks_like_a_gui_launch(""));
    }

    #[test]
    fn a_shell_launch_is_left_alone() {
        // The cost of adopting when it was not needed is a login shell on every
        // boot, so anything richer than launchd's four directories is skipped.
        assert!(!looks_like_a_gui_launch(
            "/Users/x/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        ));
        assert!(!looks_like_a_gui_launch("/opt/homebrew/bin:/usr/bin:/bin"));
    }

    #[test]
    fn the_login_path_wins_over_the_inherited_one() {
        // Which `claude` runs has to match the one the user's terminal runs,
        // otherwise an agent behaves differently depending on how the app was
        // opened — a difference nothing in the UI would explain.
        let merged = merged("/opt/homebrew/bin:/usr/bin", LAUNCHD_PATH);
        assert!(merged.starts_with("/opt/homebrew/bin:/usr/bin"), "{merged}");
    }

    #[test]
    fn inherited_directories_are_kept_even_if_the_login_shell_dropped_them() {
        // A login file that assigns rather than appends can leave out /usr/sbin
        // entirely. Dropping it here would break unrelated tools to fix agents.
        let merged = merged("/opt/homebrew/bin", LAUNCHD_PATH);
        for dir in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            assert!(merged.split(':').any(|e| e == dir), "{dir} missing from {merged}");
        }
    }

    #[test]
    fn no_directory_is_listed_twice() {
        // A duplicate is harmless to resolution but makes `PATH` grow on every
        // merge, and it is the kind of thing that shows up in a bug report.
        let merged = merged("/usr/bin:/opt/homebrew/bin", LAUNCHD_PATH);
        let mut seen: Vec<&str> = merged.split(':').collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "{merged}");
    }

    #[test]
    fn empty_segments_are_dropped() {
        // A trailing colon means "the current directory" to some tools, which is
        // not something to inherit into a process that spawns agent binaries.
        let merged = merged("/opt/homebrew/bin::", "/usr/bin:");
        assert!(!merged.split(':').any(str::is_empty), "{merged}");
    }
}
