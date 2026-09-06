//! App-side loader + persistence for [`AgentRetrySettings`].
//!
//! Reads `agent_retry.toml` from the app data dir on boot (default if absent)
//! and installs it as a GPUI global. No file watcher: like the auto-update
//! settings beside it, these two values are only ever written by the settings
//! pane, which updates the global itself.
//!
//! A parse failure falls back to the shipped default — retries on, capped at a
//! day. Falling back to *off* would be the quieter choice and the wrong one: a
//! corrupt settings file would silently remove a feature the user is relying on
//! to survive a rate limit, and nothing would say so.

use std::path::PathBuf;

use gpui::App;
use oximux_settings::agent_retry::AgentRetrySettings;

fn settings_path() -> Option<PathBuf> {
    crate::app_paths::data_dir().map(|d| d.join(AgentRetrySettings::FILE_NAME))
}

fn load() -> AgentRetrySettings {
    let Some(path) = settings_path() else {
        return AgentRetrySettings::shipped();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => AgentRetrySettings::from_toml_str(&text).unwrap_or_else(|err| {
            tracing::warn!(?path, %err, "agent_retry.toml parse failed; using defaults");
            AgentRetrySettings::shipped()
        }),
        Err(_) => AgentRetrySettings::shipped(),
    }
}

/// Persist `settings` and swap the global, so the caller's next read sees it.
pub fn save(settings: &AgentRetrySettings, cx: &mut App) -> std::io::Result<()> {
    cx.set_global(*settings);
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for agent_retry.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}

/// Load settings and install the global. Call once from the app's `run` closure.
pub fn install(cx: &mut App) {
    cx.set_global(load());
}
