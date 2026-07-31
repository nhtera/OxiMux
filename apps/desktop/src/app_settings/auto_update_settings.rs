//! App-side loader + persistence for [`AutoUpdateSettings`].
//!
//! Reads `auto_update.toml` from the app data dir on boot (default if absent)
//! and installs it as a GPUI global. No file watcher: unlike the settings
//! files a user hand-edits, these two values are only ever written by the
//! settings pane, which updates the global itself.
//!
//! A parse failure falls back to the default — updates enabled, no recorded
//! version. That direction is safe: the worst case is a redundant check and a
//! suppressed what's-new notice.

use std::path::PathBuf;

use gpui::App;
use oximux_settings::AutoUpdateSettings;

fn settings_path() -> Option<PathBuf> {
    crate::app_paths::data_dir().map(|d| d.join(AutoUpdateSettings::FILE_NAME))
}

fn load() -> AutoUpdateSettings {
    let Some(path) = settings_path() else {
        return AutoUpdateSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => AutoUpdateSettings::from_toml_str(&text).unwrap_or_else(|err| {
            tracing::warn!(?path, %err, "auto_update.toml parse failed; using defaults");
            AutoUpdateSettings::default()
        }),
        Err(_) => AutoUpdateSettings::default(),
    }
}

/// Persist `settings` and swap the global, so the caller's next read sees it.
pub fn save(settings: &AutoUpdateSettings, cx: &mut App) -> std::io::Result<()> {
    cx.set_global(settings.clone());
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for auto_update.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}

/// Load settings and install the global. Call once from the app's `run`
/// closure, before the updater itself.
///
/// Also settles the what's-new bookkeeping: whether this boot follows an
/// update is decided here (the answer is needed before anything overwrites
/// the recorded version) and returned for the caller to announce.
pub fn install(cx: &mut App) -> bool {
    let loaded = load();
    let current = env!("CARGO_PKG_VERSION");
    let upgraded = loaded.should_announce(current);

    let settings = AutoUpdateSettings {
        last_run_version: current.to_string(),
        ..loaded
    };
    cx.set_global(settings.clone());
    if let Some(path) = settings_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, settings.to_toml_string());
    }
    upgraded
}
