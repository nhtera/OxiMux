//! App-side loader + live-reload watcher for [`CommitMessageAiSettings`].
//!
//! On startup we read `commit_message_ai.toml` from the app data dir
//! (seeding a commented default if absent), install it as a GPUI
//! global, and reload on edit via the same debounced FSEvents pattern
//! the terminal settings use. The commit composer reads
//! [`CommitMessageAiSettings`] at sparkles-click time, so a mid-session
//! edit takes effect on the next click without restarting OxiMux.
//!
//! The watch is on the DIRECTORY (atomic save = write-temp + rename
//! replaces the inode, which a file-level watch would miss) but
//! filtered to the `commit_message_ai.toml` filename so churn from
//! sibling files (terminal.toml, oximux.db, sqlite WAL) is ignored.

use std::path::PathBuf;

use gpui::App;
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdMap, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use oximux_settings::CommitMessageAiSettings;
use tokio::sync::mpsc;

/// Mirror of `terminal_settings.rs::APP_DATA_SUBDIR` — kept in
/// lockstep so all settings land in the same per-user data dir.
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";

/// Debounce window for settings edits. A save can fire several
/// FSEvents; one reload per burst is plenty.
const DEBOUNCE_MS: u64 = 250;

fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR))
}

fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(CommitMessageAiSettings::FILE_NAME))
}

/// Read + sanitize settings from disk, falling back to defaults on
/// a missing file or a parse error (logged so a typo is visible
/// without a crash). Mirrors the terminal-settings load policy
/// exactly so both files behave the same on edit.
fn load() -> CommitMessageAiSettings {
    let Some(path) = settings_path() else {
        return CommitMessageAiSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match CommitMessageAiSettings::from_toml_str(&text) {
            Ok(parsed) => parsed.sanitized(),
            Err(err) => {
                tracing::warn!(
                    ?path,
                    %err,
                    "commit_message_ai.toml parse failed; using defaults"
                );
                CommitMessageAiSettings::default()
            }
        },
        // Absent file is the common case (fresh install or
        // user-deleted) — silent default.
        Err(_) => CommitMessageAiSettings::default(),
    }
}

/// Write a default `commit_message_ai.toml` if none exists yet, so
/// users have a fully-populated template to edit. Best-effort:
/// failures are logged, not fatal (settings still work from
/// in-memory defaults).
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
    let body = CommitMessageAiSettings::default().to_toml_string();
    if let Err(err) = std::fs::write(&path, body) {
        tracing::warn!(
            ?path,
            %err,
            "could not seed default commit_message_ai.toml"
        );
    }
}

fn apply(cx: &mut App, settings: CommitMessageAiSettings) {
    cx.set_global(settings);
}

/// True when a debounced batch touched `commit_message_ai.toml`
/// (ignoring the sibling files that share the data dir).
fn batch_touches_settings(result: &DebounceEventResult) -> bool {
    let Ok(events) = result else { return false };
    events.iter().any(|ev| {
        ev.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == CommitMessageAiSettings::FILE_NAME)
        })
    })
}

/// Load settings, install the global, seed a default file, and
/// start the live-reload watcher. Call once from the app's `run`
/// closure alongside [`crate::terminal_settings::install`].
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
                tracing::warn!(
                    ?dir,
                    %err,
                    "commit_message_ai settings watch failed; live-reload off"
                );
                return;
            }
            d
        }
        Err(err) => {
            tracing::warn!(
                %err,
                "could not create commit_message_ai settings watcher; live-reload off"
            );
            return;
        }
    };
    // The debouncer owns the FSEvents thread; dropping it stops the
    // stream. It must live for the whole process, so leak it rather
    // than thread a holder through the window lifecycle (same
    // strategy `terminal_settings.rs` uses).
    let _: &'static mut Debouncer<RecommendedWatcher, FileIdMap> = Box::leak(Box::new(debouncer));

    cx.spawn(async move |cx| {
        while let Some(result) = rx.recv().await {
            if !batch_touches_settings(&result) {
                continue;
            }
            let next = load();
            cx.update(|cx| {
                apply(cx, next);
                // Force a repaint so the sparkles button's
                // disabled-tooltip state reflects a mode change
                // immediately (mode = "off" should hide the button
                // on the next frame, not after the user causes
                // another notify).
                cx.refresh_windows();
            });
        }
    })
    .detach();
}
