//! App-side loader + persistence for [`SessionSearchSettings`].
//!
//! Reads `session_search.toml` from the app data dir on boot (default — off —
//! if absent) and installs it as a GPUI global. Only the settings pane writes
//! it; [`crate::session_search_service`] observes the global to start or stop
//! the indexer.

use std::path::PathBuf;

use gpui::App;
use oximux_settings::SessionSearchSettings;

fn settings_path() -> Option<PathBuf> {
    crate::app_paths::data_dir().map(|d| d.join(SessionSearchSettings::FILE_NAME))
}

/// Load settings and install the global. Call before
/// [`crate::session_search_service::install`].
pub fn install(cx: &mut App) {
    let settings = crate::app_paths::data_dir()
        .map(|dir| SessionSearchSettings::load_from_dir(&dir))
        .unwrap_or_else(SessionSearchSettings::shipped);
    cx.set_global(settings);
}

/// Persist `settings` and swap the global (which starts or stops the indexer).
pub fn save(settings: &SessionSearchSettings, cx: &mut App) -> std::io::Result<()> {
    cx.set_global(settings.clone());
    let path = settings_path().ok_or_else(|| std::io::Error::other("no app data dir for session_search.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}
