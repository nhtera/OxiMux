//! Process-wide mirrors of the spawn-time terminal settings.
//!
//! The PTY spawn helpers are `cx`-less free functions — they run from the
//! restore reconcile on a background executor, and from `Drop`-adjacent wake
//! paths — so they cannot read the GPUI global. These statics are the seam:
//! the settings loader and its live-reload watcher (both `cx`-holding) push
//! into them, and the spawn path pulls.
//!
//! Only settings sourced AT SPAWN belong here. Render-time knobs (alphas,
//! blink, scroll speed) are read from the global on the frame that needs them,
//! because a live pane has to pick them up without respawning.

use std::path::PathBuf;

use oximux_pty::SpawnConfig;

/// Process-wide mirror of `TerminalSettings::scrollback_lines`. The PTY spawn
/// helpers are `cx`-less free functions, so they read scrollback here instead
/// of threading the global through every call site. The settings loader and
/// the live-reload watcher (both of which hold `cx`) keep it in sync.
static SPAWN_SCROLLBACK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(5000);

/// Update the spawn-scrollback mirror from settings. Called once at startup and
/// on every settings reload.
pub fn set_spawn_scrollback(lines: usize) {
    SPAWN_SCROLLBACK.store(lines, std::sync::atomic::Ordering::Relaxed);
}

pub(super) fn spawn_scrollback() -> usize {
    SPAWN_SCROLLBACK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide mirror of `TerminalSettings::shell_integration`. Read by the
/// `cx`-less PTY spawn helpers (and the dormant-promote paths) to decide
/// whether to inject the OSC 133 shell-integration bootstrap. Kept in sync by
/// the settings loader + live-reload watcher (both `cx`-holding).
static SHELL_INTEGRATION_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Update the shell-integration mirror from settings. Called once at startup
/// and on every settings reload.
pub fn set_shell_integration_enabled(enabled: bool) {
    SHELL_INTEGRATION_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn shell_integration_enabled() -> bool {
    SHELL_INTEGRATION_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide mirror of `TerminalSettings::shell`. Same reason as the two
/// above: the spawn helpers are `cx`-less. Empty means the user set no
/// override, and the spawning process picks.
static SPAWN_SHELL: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// Update the shell-override mirror from settings. Called once at startup and
/// on every settings reload.
pub fn set_spawn_shell(shell: String) {
    let mut slot = SPAWN_SHELL
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = shell;
}

/// The user's shell override, or `None` when they have not set one.
fn spawn_shell() -> Option<String> {
    let slot = SPAWN_SHELL
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (!slot.is_empty()).then(|| slot.clone())
}

/// A [`SpawnConfig`] whose shell honors the user's override, falling back to
/// whatever the platform resolver picked.
///
/// Only for interactive shells. A spawn that names its own program — an agent
/// CLI, an ACP embedded terminal — must not have the user's shell substituted
/// under it.
pub(super) fn shell_spawn_config(cwd: PathBuf, env: Vec<(String, String)>, cols: u16, rows: u16) -> SpawnConfig {
    let base = SpawnConfig::default();
    SpawnConfig {
        cwd,
        env,
        cols,
        rows,
        scrollback: spawn_scrollback(),
        shell: spawn_shell().unwrap_or(base.shell),
        ..base
    }
}


#[cfg(test)]
mod shell_override_tests {
    use super::*;

    // The override has to reach the config the backend is handed, or the
    // setting is a no-op that looks configured. Empty must fall through to the
    // resolver rather than spawning "".
    #[test]
    fn a_shell_override_reaches_the_spawn_config() {
        let _serial = crate::platform::serialize_input_state();
        let resolved = oximux_pty::SpawnConfig::default().shell;

        set_spawn_shell(String::new());
        let auto = shell_spawn_config(std::path::PathBuf::from("."), Vec::new(), 80, 24);
        assert_eq!(auto.shell, resolved, "no override should leave the resolver's pick");

        set_spawn_shell("/nonexistent/oximux-test-shell".to_string());
        let overridden = shell_spawn_config(std::path::PathBuf::from("."), Vec::new(), 80, 24);
        // A path no resolver could return, so the assert cannot pass by
        // coincidence with whatever $SHELL happens to be.
        assert_eq!(overridden.shell, "/nonexistent/oximux-test-shell");

        // Restore so a later test in this process isn't spawning it.
        set_spawn_shell(String::new());
    }
}
