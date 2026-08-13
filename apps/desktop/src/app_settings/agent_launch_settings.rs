//! App-side loader + live-reload watcher for [`AgentLaunchSettings`].
//!
//! On startup we read `agent_launch.toml` from the app data dir (seeding a
//! commented default if absent), install it as a GPUI global, and reload on
//! edit via the same debounced FSEvents pattern the other settings files use.
//! The launch picker and agent runtime read [`AgentLaunchSettings`] at spawn
//! time, so a mid-session edit takes effect on the next launch without a
//! restart.
//!
//! The watch is on the DIRECTORY (atomic save = write-temp + rename replaces
//! the inode, which a file-level watch would miss) but filtered to the
//! `agent_launch.toml` filename so churn from sibling files is ignored.

use std::path::PathBuf;

use gpui::App;
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdMap, new_debouncer,
    notify::{RecommendedWatcher, RecursiveMode},
};
use oximux_settings::AgentLaunchSettings;
use tokio::sync::mpsc;


/// Debounce window for settings edits. A save can fire several FSEvents; one
/// reload per burst is plenty.
const DEBOUNCE_MS: u64 = 250;

fn data_dir() -> Option<PathBuf> {
    crate::app_paths::data_dir()
}

fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(AgentLaunchSettings::FILE_NAME))
}

/// Read + sanitize settings from disk, falling back to defaults on a missing
/// file or a parse error (logged so a typo is visible without a crash).
fn load() -> AgentLaunchSettings {
    let Some(path) = settings_path() else {
        return AgentLaunchSettings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match AgentLaunchSettings::from_toml_str(&text) {
            Ok(parsed) => parsed.sanitized(),
            Err(err) => {
                tracing::warn!(?path, %err, "agent_launch.toml parse failed; using defaults");
                AgentLaunchSettings::default()
            }
        },
        // Absent file is the common case (fresh install or user-deleted) —
        // silent default.
        Err(_) => AgentLaunchSettings::default(),
    }
}

/// Persist `settings` to `agent_launch.toml`. The live-reload watcher
/// reparses and swaps the global, so callers MUST NOT also `set_global`
/// (that would race the debouncer).
pub fn save(settings: &AgentLaunchSettings) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::other("no app data dir for agent_launch.toml"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, settings.to_toml_string())
}

fn apply(cx: &mut App, settings: AgentLaunchSettings) {
    // Mirror the toggle into each agent's own global hooks file so a hand-typed
    // `claude`/`codex` in any pane reports status too (picker agents already get
    // the same hooks via `--settings`; the command strings match, so Claude's
    // hook dedup keeps them firing once). Runs at boot and on every live edit,
    // so flipping the toggle installs or removes the global hooks without a
    // restart. The env override force-enables it like the per-spawn path does.
    let hooks_on = settings.status_hooks_enabled || crate::agent_status_hooks::env_forced();
    crate::agent_hooks_global::sync_global_status_hooks(hooks_on);
    // Codex is what turns its rail rows from a bare status verb into the
    // agent's actual reply. Gated on the same toggle, and Codex still asks the
    // user to trust the hook before it runs any of it.
    crate::agent_hooks_global::sync_global_codex_hooks(hooks_on);
    cx.set_global(settings);
}

/// True when a debounced batch touched `agent_launch.toml` (ignoring the
/// sibling files that share the data dir).
fn batch_touches_settings(result: &DebounceEventResult) -> bool {
    let Ok(events) = result else { return false };
    events.iter().any(|ev| {
        ev.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == AgentLaunchSettings::FILE_NAME)
        })
    })
}

/// Load settings, install the global, and start the live-reload watcher.
/// Call once from the app's `run` closure alongside the other settings
/// installers.
///
/// On a fresh profile we back-fill the skip-permissions defaults via a
/// one-shot migration and persist them, so the first one-click launch starts
/// the agent in full-autonomy mode and the seeded flags are visible + editable
/// in the settings pane. The `yolo_defaults_migrated` guard means a user who
/// later clears a flag is never re-seeded.
pub fn install(cx: &mut App) {
    let mut settings = load();
    if settings.seed_yolo_defaults()
        && let Err(err) = save(&settings)
    {
        tracing::warn!(%err, "could not persist seeded agent_launch.toml defaults");
    }
    apply(cx, settings);

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
                tracing::warn!(?dir, %err, "agent_launch settings watch failed; live-reload off");
                return;
            }
            d
        }
        Err(err) => {
            tracing::warn!(%err, "could not create agent_launch settings watcher; live-reload off");
            return;
        }
    };
    // The debouncer owns the FSEvents thread; dropping it stops the stream.
    // It must live for the whole process, so leak it rather than thread a
    // holder through the window lifecycle (same strategy the sibling
    // settings modules use).
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
