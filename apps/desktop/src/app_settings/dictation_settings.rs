//! App-side loader + live-reload watcher for [`DictationSettings`].
//!
//! Reads `dictation.toml` from the app data dir on boot (default if absent),
//! installs it as a GPUI global, and reloads on edit via the same debounced
//! FSEvents pattern the other settings files use. The composer reads
//! [`DictationSettings`] at record time, so a mid-session edit (or a Voice-pane
//! change) takes effect on the next press without a restart.
//!
//! Persist writes the TOML only — the watcher swaps the global, so callers must
//! never `set_global` directly (that would race the debouncer).

use std::path::PathBuf;

use gpui::App;
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdMap, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use oximux_settings::DictationSettings;
use tokio::sync::mpsc;

const DEBOUNCE_MS: u64 = 250;

fn data_dir() -> Option<PathBuf> {
    crate::app_paths::data_dir()
}

fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(DictationSettings::FILE_NAME))
}

fn load() -> DictationSettings {
    let Some(path) = settings_path() else {
        return DictationSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match DictationSettings::from_toml_str(&text) {
            Ok(parsed) => parsed.sanitized(),
            Err(err) => {
                tracing::warn!(?path, %err, "dictation.toml parse failed; using defaults");
                DictationSettings::default()
            }
        },
        Err(_) => DictationSettings::default(),
    }
}

/// Persist `settings` to `dictation.toml`. The watcher reparses + swaps the
/// global; callers MUST NOT also `set_global`.
pub fn save(settings: &DictationSettings) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for dictation.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}

fn apply(cx: &mut App, settings: DictationSettings) {
    cx.set_global(settings);
}

fn batch_touches_settings(result: &DebounceEventResult) -> bool {
    let Ok(events) = result else { return false };
    events.iter().any(|ev| {
        ev.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == DictationSettings::FILE_NAME)
        })
    })
}

/// Load settings, install the global, and start the live-reload watcher.
/// Call once from the app's `run` closure.
pub fn install(cx: &mut App) {
    apply(cx, load());

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
                tracing::warn!(?dir, %err, "dictation settings watch failed; live-reload off");
                return;
            }
            d
        }
        Err(err) => {
            tracing::warn!(%err, "could not create dictation settings watcher; live-reload off");
            return;
        }
    };
    // Leak the debouncer so its FSEvents thread lives for the whole process
    // (same strategy the sibling settings modules use).
    let _: &'static mut Debouncer<RecommendedWatcher, FileIdMap> = Box::leak(Box::new(debouncer));

    cx.spawn(async move |cx| {
        while let Some(result) = rx.recv().await {
            if !batch_touches_settings(&result) {
                continue;
            }
            let next = load();
            cx.update(|cx| {
                apply(cx, next);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}
