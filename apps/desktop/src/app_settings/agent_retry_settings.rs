//! The app's view of [`AgentRetrySettings`], which the **host config** owns.
//!
//! A chat reads these two values the moment a turn fails, so they are kept in
//! a GPUI global rather than fetched from the core at that moment. The global
//! is a *view* of `HostConfig` and never a second store: `oximux serve` reads
//! the same two rows out of the same database, `oximux config set` writes
//! them, and the settings pane edits them through the core like every other
//! host setting. Two stores for one policy is how the headless host and this
//! one would come to disagree about whether a rate-limited turn is retried.
//!
//! What remains app-side is the *import*: before these values lived in the
//! host config they were a TOML file beside the other per-app settings, and a
//! user who had chosen "6 hours" or switched retries off must not have that
//! choice quietly reset by an upgrade.

use std::path::PathBuf;

use gpui::App;
use oximux_remote_host::host_config::{HostConfig, keys};
use oximux_settings::agent_retry::AgentRetrySettings;

/// The retry settings as a config states them.
pub fn of(config: &HostConfig) -> AgentRetrySettings {
    AgentRetrySettings { enabled: config.retry_enabled, max_automatic_wait: config.retry_max_wait }
}

/// The host's config now holds these: swap the global so a chat with a retry
/// armed reads the new ceiling on its next failure rather than after a
/// restart.
pub fn apply(config: &HostConfig, cx: &mut App) {
    cx.set_global(of(config));
}

/// Seed the global from the host's config, carrying a pre-config
/// `agent_retry.toml` over the first time.
///
/// Call once at boot **after** the core is installed: with no core there is
/// no config to read and nothing to import, and the shipped defaults stand.
pub fn install(cx: &mut App) {
    let core = cx.try_global::<crate::remote_control::RemoteControl>().and_then(|rc| rc.host_core());
    let config = match &core {
        Some(core) => {
            import_file_once(core);
            core.config()
        }
        None => HostConfig::default(),
    };
    apply(&config, cx);
}

fn settings_path() -> Option<PathBuf> {
    crate::app_paths::data_dir().map(|d| d.join(AgentRetrySettings::FILE_NAME))
}

/// Write a choice made in the old file into the host config, once.
///
/// Only when **nothing** is stored for either key: after the first import the
/// rows exist, so a later edit of the (now stale) file cannot overwrite what
/// the pane or `oximux config set` has since put there. The file is left
/// where it is rather than deleted — removing a user's data to tidy up after
/// ourselves is the worse of the two surprises.
///
/// A parse failure imports nothing and leaves the shipped defaults, which is
/// what the file's own loader did: falling back to retries *off* would
/// silently remove a feature a user relies on to survive a rate limit.
fn import_file_once(core: &oximux_remote_host::HostCore) {
    let untouched =
        |key: &str| core.config_entry(key).map(|entry| entry.is_default).unwrap_or(false);
    if !(untouched(keys::RETRY_ENABLED) && untouched(keys::RETRY_MAX_WAIT)) {
        return;
    }
    let Some(path) = settings_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let file = match AgentRetrySettings::from_toml_str(&text) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(?path, %err, "agent_retry.toml parse failed; not imported");
            return;
        }
    };
    if file == AgentRetrySettings::shipped() {
        return;
    }
    let edit = core.edit_config(|config| {
        config.retry_enabled = file.enabled;
        config.retry_max_wait = file.max_automatic_wait;
    });
    match edit {
        Ok(_) => tracing::info!(?path, "imported the retry settings into the host config"),
        Err(err) => tracing::warn!(?path, ?err, "retry settings not imported"),
    }
}
