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
use oximux_shell_env::ResolvedShell;

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

/// Process-wide mirror of the resolved spawn shell. Same reason as the two
/// above: the spawn helpers are `cx`-less. `None` means "no explicit pick"
/// and the spawn falls back to `SpawnConfig::default().shell`.
///
/// Unlike the bare-string it replaced, this carries the argv and env a shell
/// needs to launch (Git Bash's `-i` / `MSYSTEM` / `CHERE_INVOKING`), resolved
/// once by the settings loader from the user's [`WindowsShell`] choice.
static SPAWN_SHELL: std::sync::RwLock<Option<ResolvedShell>> = std::sync::RwLock::new(None);

/// Set the resolved spawn shell from settings. Called once at startup and on
/// every settings reload. `None` clears any prior override.
pub fn set_spawn_shell_resolved(shell: Option<ResolvedShell>) {
    let mut slot = SPAWN_SHELL
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = shell;
}

/// Back-compat setter for a bare program path (used by tests and any caller
/// that only knows a path). Empty clears the override.
pub fn set_spawn_shell(shell: String) {
    set_spawn_shell_resolved((!shell.is_empty()).then(|| ResolvedShell {
        program: shell,
        args: Vec::new(),
        env: Vec::new(),
    }));
}

/// The resolved spawn shell, or `None` when none was set.
fn spawn_shell() -> Option<ResolvedShell> {
    SPAWN_SHELL
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// A [`SpawnConfig`] whose shell honors the user's resolved choice, falling
/// back to whatever the platform resolver picked.
///
/// The resolved shell's own env is prepended so the caller's context env
/// (passed in `env`) still wins on any key collision — the backend applies
/// `cfg.env` last, so a later duplicate key overrides an earlier one.
///
/// Only for interactive shells. A spawn that names its own program — an agent
/// CLI, an ACP embedded terminal — must not have the user's shell substituted
/// under it.
pub(super) fn shell_spawn_config(cwd: PathBuf, env: Vec<(String, String)>, cols: u16, rows: u16) -> SpawnConfig {
    let base = SpawnConfig::default();
    let (shell, args, mut merged_env) = match spawn_shell() {
        Some(resolved) => (resolved.program, resolved.args, resolved.env),
        None => (base.shell, base.args.clone(), Vec::new()),
    };
    merged_env.extend(env);
    SpawnConfig {
        cwd,
        env: merged_env,
        cols,
        rows,
        scrollback: spawn_scrollback(),
        shell,
        args,
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

    /// End-to-end: the Windows Git Bash choice must resolve, build a spawn
    /// config, survive the OSC-133 augmentation, and launch a shell that runs
    /// a command in the pane's cwd. Self-skipping when Git for Windows is not
    /// installed (CI images without it), exactly like the shell-env resolver
    /// test. The real production functions are used at every step — no
    /// reimplementation — so this is the live proof of the whole path.
    #[cfg(windows)]
    #[test]
    fn git_bash_choice_spawns_a_working_pane() {
        use std::time::{Duration, Instant};

        use oximux_pty::backend::TerminalBackend;
        use oximux_pty::events::TerminalEvent;
        use oximux_pty::portable_pty_backend::PortablePtyBackend;

        let _serial = crate::platform::serialize_input_state();

        // Resolve exactly as the settings loader's `apply` does.
        let resolved = oximux_shell_env::resolve_windows_shell(
            oximux_shell_env::WindowsShell::GitBash,
            oximux_shell_env::WindowsPowerShell::Auto,
        );
        let is_git_bash = std::path::Path::new(&resolved.program)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("bash.exe"))
            .unwrap_or(false);
        if !is_git_bash {
            // No Git for Windows here — nothing to verify.
            return;
        }
        set_spawn_shell_resolved(Some(resolved));

        // Build the config through the real helper, then run the real OSC-133
        // augmentation (writes the overlay + prepends `--rcfile`).
        let cwd = std::env::temp_dir();
        let mut cfg = shell_spawn_config(cwd.clone(), Vec::new(), 100, 32);
        crate::shell::terminal::shell_integration::augment_spawn_config(&mut cfg);
        assert!(
            cfg.args.iter().any(|a| a == "-i"),
            "git bash must launch interactive: {:?}",
            cfg.args
        );
        assert!(
            cfg.env.iter().any(|(k, _)| k == "CHERE_INVOKING"),
            "git bash must keep the pane cwd"
        );

        let mut backend = PortablePtyBackend::new();
        let id = backend.spawn(cfg).expect("spawn git bash pane");

        // Drive it: answer device queries, then type a command whose output is
        // a unique marker, and wait for it to render.
        let t0 = Instant::now();
        let mut typed = false;
        let mut found = false;
        while t0.elapsed() < Duration::from_secs(15) && !found {
            for ev in backend.drain_events() {
                if let TerminalEvent::PtyReply { bytes, .. } = ev {
                    let _ = backend.write(id, &bytes);
                }
            }
            if !typed && t0.elapsed() > Duration::from_millis(1500) {
                let _ = backend.write(id, b"echo OXIMUXLIVE_$((6*7))\r\n");
                typed = true;
            }
            if let Ok(snap) = backend.snapshot(id) {
                found = snap.cells.iter().any(|row| {
                    let s: String = row.iter().map(|c| c.ch).collect();
                    // The command line echoes the literal `$((6*7))`; only the
                    // expanded output proves the shell actually ran it.
                    s.contains("OXIMUXLIVE_42")
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        backend.close(id).ok();
        set_spawn_shell_resolved(None);

        assert!(found, "git bash pane never rendered the expanded command output");
    }
}
