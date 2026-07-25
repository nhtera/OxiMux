//! App-side loader + live-reload watcher for [`TerminalSettings`].
//!
//! On startup we read `terminal.toml` from the app data dir (seeding a
//! commented default if absent), install it as a GPUI global, and mirror the
//! spawn-time scrollback into the `cx`-less spawn path. A debounced FSEvents
//! watch on the data dir reparses on edit and swaps the global; the terminal's
//! continuous poll-repaint then picks up render/event-time knobs, and a
//! `cx.refresh()` forces a repaint so an idle pane updates too.
//!
//! The watch is on the DIRECTORY (atomic save = write-temp + rename replaces
//! the inode, which a file-level watch would miss) but filtered to the
//! `terminal.toml` filename so the sqlite WAL churn in the same dir is ignored.

use std::path::PathBuf;

use gpui::App;
use notify_debouncer_full::{
    DebounceEventResult, FileIdMap, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use oximux_settings::TerminalSettings;
use tokio::sync::mpsc;

use crate::shell::terminal_view::{set_shell_integration_enabled, set_spawn_scrollback};

/// Mirror of `main.rs::APP_DATA_SUBDIR` — kept in lockstep so settings land
/// next to `oximux.db`.
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";

/// Debounce window for settings edits. Generous: a save can fire several
/// FSEvents; one reload per burst is plenty.
const DEBOUNCE_MS: u64 = 250;

fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR))
}

fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(TerminalSettings::FILE_NAME))
}

/// Read + sanitize settings from disk, falling back to defaults on a missing
/// file or a parse error (logged so a typo is visible without a crash).
fn load() -> TerminalSettings {
    let Some(path) = settings_path() else {
        return TerminalSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match TerminalSettings::from_toml_str(&text) {
            Ok(parsed) => {
                let clean = parsed.sanitized();
                if clean != parsed {
                    // Silent clamps create a UX trap: the user's value on disk
                    // and the value the app uses disagree. Warn so a typo like
                    // `scroll_multiplier = 0` is visible in the logs.
                    tracing::warn!(
                        ?path,
                        "terminal.toml had out-of-range values; clamped to safe bounds"
                    );
                }
                clean
            }
            Err(err) => {
                tracing::warn!(?path, %err, "terminal.toml parse failed; using defaults");
                TerminalSettings::default()
            }
        },
        // Absent file is the common case (fresh install) — silent default.
        Err(_) => TerminalSettings::default(),
    }
}

/// Write a default `terminal.toml` if none exists yet, so users have a
/// fully-populated template to edit. Best-effort: failures are logged, not
/// fatal (settings still work from in-memory defaults).
fn seed_default_if_absent() {
    let Some(path) = settings_path() else { return };
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return;
    }
    let body = TerminalSettings::default().to_toml_string();
    if let Err(err) = std::fs::write(&path, body) {
        tracing::warn!(?path, %err, "could not seed default terminal.toml");
    }
}

fn apply(cx: &mut App, settings: TerminalSettings) {
    set_spawn_scrollback(settings.scrollback_lines);
    set_shell_integration_enabled(settings.shell_integration);
    cx.set_global(settings);
}

/// Resolved app data dir (where `terminal.toml` + sibling configs live).
/// Public so the settings UI can surface the path in its About pane.
pub fn app_data_dir() -> Option<PathBuf> {
    data_dir()
}

/// Persist `settings` to `terminal.toml`. The live-reload watcher then
/// reparses and swaps the global, so callers MUST NOT also `set_global`
/// (that would race the debouncer). Best-effort directory create so a
/// fresh profile that never seeded the file still writes cleanly.
pub fn save(settings: &TerminalSettings) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for terminal.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}

/// True when a debounced batch touched `terminal.toml` (ignoring the sqlite
/// WAL/shm files that share the data dir).
fn batch_touches_settings(result: &DebounceEventResult) -> bool {
    let Ok(events) = result else { return false };
    events.iter().any(|ev| {
        ev.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == TerminalSettings::FILE_NAME)
        })
    })
}

/// Load settings, install the global, seed a default file, and start the
/// live-reload watcher. Call once from the app's `run` closure.
pub fn install(cx: &mut App) {
    apply(cx, load());
    seed_default_if_absent();

    let Some(dir) = data_dir() else { return };
    let (tx, mut rx) = mpsc::unbounded_channel::<DebounceEventResult>();
    let debouncer = match new_debouncer(
        std::time::Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    ) {
        Ok(mut d) => {
            if let Err(err) = d.watch(&dir, RecursiveMode::NonRecursive) {
                tracing::warn!(?dir, %err, "terminal settings watch failed; live-reload off");
                return;
            }
            d
        }
        Err(err) => {
            tracing::warn!(%err, "could not create settings watcher; live-reload off");
            return;
        }
    };
    // The debouncer owns the FSEvents thread; dropping it stops the stream.
    // It must live for the whole process, so leak it rather than thread a
    // holder through the window lifecycle.
    let _: &'static mut Debouncer<RecommendedWatcher, FileIdMap> = Box::leak(Box::new(debouncer));

    cx.spawn(async move |cx| {
        while let Some(result) = rx.recv().await {
            if !batch_touches_settings(&result) {
                continue;
            }
            let next = load();
            cx.update(|cx| {
                apply(cx, next);
                // Force a repaint so an idle pane (no PTY output) still
                // reflects render-time knobs immediately.
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

use notify_debouncer_full::Debouncer;
