//! App side of per-terminal shell history.
//!
//! The shells do the per-command work (see `oximux_shell_env::history`); the
//! app owns the directory and the moments the shells cannot see:
//! - **spawn**: every integrated zsh/bash/fish gets `OXIMUX_HISTORY_DIR`;
//! - **split / new tab in a pane**: the child's file is copied from the
//!   parent's before the child spawns, so it starts with the parent's history;
//! - **close**: the terminal's files are deleted (its commands are already in
//!   the user's own history file);
//! - **project removal / integration off / boot**: stale files are dropped.
//!
//! App quit deliberately does nothing: a quit keeps every terminal, and a
//! restored terminal (same `OXIMUX_TAB_ID`) reads its own file again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use oximux_shell_env::history::{self, TabId};
use oximux_storage::SettingsRepo;

use super::terminal_view::shell_integration_enabled;

/// Unreferenced history older than this is swept at boot. Long enough that a
/// terminal in a project left closed for weeks keeps its history; persisted
/// layouts are the real liveness signal, this is only the backstop.
const GC_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long after a close the files are deleted a second time. The relay
/// hangs up the shell and kills it 500 ms later; a shell writes its history
/// on SIGHUP, which would recreate the file the first delete removed.
const CLOSE_SETTLE: Duration = Duration::from_millis(2500);

/// `<app data>/shell-history`, made owner-only once per process. `None` when
/// there is no data dir or it cannot be closed to other accounts; terminals
/// then keep using the user's own history file, as before.
pub fn history_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = crate::terminal_settings::app_data_dir()?.join("shell-history");
        match oximux_owner_only::prepare_owner_only_dir(&dir) {
            Ok(()) => Some(dir),
            Err(err) => {
                tracing::warn!(?err, ?dir, "shell history dir unavailable; per-terminal history off");
                None
            }
        }
    })
    .as_deref()
}

/// The `OXIMUX_HISTORY_DIR` pair for an integrated spawn. `/`-separated on
/// Windows, where the only consumer is Git Bash.
pub(crate) fn env_pair() -> Option<(String, String)> {
    let dir = history_dir()?.to_string_lossy().into_owned();
    let dir = if cfg!(windows) { dir.replace('\\', "/") } else { dir };
    Some((history::HISTORY_DIR_ENV.to_string(), dir))
}

/// Start `child_tab` with a copy of `parent_tab`'s history. Call before the
/// child spawns; a failure only costs the copy (the child then starts from
/// the user's own history).
pub fn inherit(parent_tab: &str, child_tab: &str) {
    if !shell_integration_enabled() {
        return;
    }
    let (Some(dir), Some(parent), Some(child)) =
        (history_dir(), TabId::parse(parent_tab), TabId::parse(child_tab))
    else {
        return;
    };
    if let Err(err) = history::inherit(dir, &parent, &child) {
        tracing::warn!(?err, parent_tab, child_tab, "shell history inherit failed");
    }
}

/// Delete `tab`'s history now.
pub fn forget(tab: &str) {
    let (Some(dir), Some(id)) = (history_dir(), TabId::parse(tab)) else {
        return;
    };
    if let Err(err) = history::forget(dir, &id) {
        tracing::warn!(?err, tab, "shell history forget failed");
    }
}

/// Delete a closed terminal's history: now, and again once its shell is
/// surely gone. Blocks for [`CLOSE_SETTLE`]; call it off the main thread.
/// The first pass keeps the fish pointer, so the second can still find a
/// fish history file rewritten as the shell exited.
pub fn forget_after_close(tab: &str) {
    let (Some(dir), Some(id)) = (history_dir(), TabId::parse(tab)) else {
        return;
    };
    if let Err(err) = history::forget_keeping_fish_pointer(dir, &id) {
        tracing::warn!(?err, tab, "shell history forget failed");
    }
    std::thread::sleep(CLOSE_SETTLE);
    forget(tab);
}

/// Delete the history of every terminal persisted for a removed project,
/// including windows of it never opened this session (no view, so no close).
pub fn forget_project(repo: &SettingsRepo, project_id: &str) {
    let prefix = crate::persisted_terminals::legacy_settings_key(project_id);
    let rows = match repo.list_prefixed(&prefix) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(?err, project_id, "shell history: project layouts unreadable");
            return;
        }
    };
    let mut ids = HashSet::new();
    for (key, value) in rows {
        // `terminal_tabs:<id>` (legacy) or `terminal_tabs:<id>:<window>`; a
        // longer project id sharing the prefix is someone else's.
        let ours = key
            .strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'));
        if ours && let Ok(json) = serde_json::from_str::<serde_json::Value>(&value) {
            collect_tab_ids(&json, &mut ids);
        }
    }
    if ids.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for id in ids {
            forget(id.as_str());
        }
    });
}

/// Sweep history no persisted terminal refers to, on a background thread.
/// Fail-closed: any layout that cannot be read or parsed, or a layout set
/// aside as corrupt within the grace period, skips the sweep entirely, since
/// an incomplete reference set would delete live history.
pub fn spawn_boot_gc(repo: SettingsRepo) {
    let Some(dir) = history_dir() else {
        return;
    };
    std::thread::spawn(move || {
        if recently_rejected_layout() {
            tracing::info!("shell history gc skipped: a layout was set aside as corrupt");
            return;
        }
        let referenced = match referenced_tab_ids(&repo) {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(%err, "shell history gc skipped");
                return;
            }
        };
        match history::gc(dir, &referenced, GC_MAX_AGE, SystemTime::now()) {
            Ok(0) => {}
            Ok(n) => tracing::info!(n, "shell history gc: forgot closed terminals"),
            Err(err) => tracing::warn!(?err, "shell history gc failed"),
        }
    });
}

/// Every tab id in every persisted pane and floating layout.
fn referenced_tab_ids(repo: &SettingsRepo) -> Result<HashSet<TabId>, String> {
    let mut rows = Vec::new();
    for prefix in [
        crate::persisted_terminals::KEY_PREFIX,
        crate::shell::terminal::floating_terminal_persistence::TABS_KEY_PREFIX,
    ] {
        rows.extend(repo.list_prefixed(prefix).map_err(|err| format!("{prefix}: {err}"))?);
    }
    tab_ids_in(rows)
}

/// Pure half of [`referenced_tab_ids`]: one unparseable value fails all.
fn tab_ids_in(rows: Vec<(String, String)>) -> Result<HashSet<TabId>, String> {
    let mut ids = HashSet::new();
    for (key, value) in rows {
        let json: serde_json::Value =
            serde_json::from_str(&value).map_err(|err| format!("{key}: {err}"))?;
        collect_tab_ids(&json, &mut ids);
    }
    Ok(ids)
}

/// Every `"tab_id"` string anywhere in a layout. Walking the JSON rather
/// than the typed snapshot keeps this right as the layout schema grows (leaf
/// tabs, sub-panes, floating tabs all name their terminal the same way).
fn collect_tab_ids(value: &serde_json::Value, out: &mut HashSet<TabId>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "tab_id"
                    && let Some(id) = child.as_str().and_then(TabId::parse)
                {
                    out.insert(id);
                }
                collect_tab_ids(child, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_tab_ids(item, out)),
        _ => {}
    }
}

/// A `*.corrupt.json` preserved within the grace period: its terminals may
/// still be live on disk, but their ids are no longer in any layout.
fn recently_rejected_layout() -> bool {
    let Some(dir) = crate::restore_fallback::corrupt_layouts_dir() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().ends_with(".corrupt.json")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .map_or(true, |at| at.elapsed().unwrap_or_default() < GC_MAX_AGE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-2222-3333-4444-555555555555";
    const B: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    #[test]
    fn tab_ids_are_found_at_any_depth_and_only_when_valid() {
        let layout = format!(
            r#"{{"tabs":[{{"sub_panes":[{{"tab_id":"{A}","tabs":[{{"tab_id":"{B}"}}]}}]}}],
                "tab_id":"tab-1","other":{{"tab_id":7}}}}"#
        );
        let ids = tab_ids_in(vec![("terminal_tabs:p:main".into(), layout)]).unwrap();
        let expected: HashSet<TabId> =
            [A, B].into_iter().map(|s| TabId::parse(s).unwrap()).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn one_unparseable_layout_fails_the_whole_set() {
        let rows = vec![
            ("terminal_tabs:p:main".to_string(), format!(r#"{{"tab_id":"{A}"}}"#)),
            ("terminal_tabs:q:main".to_string(), r#"{"tabs":[{"tab_id":"#.to_string()),
        ];
        assert!(tab_ids_in(rows).is_err(), "a truncated blob must skip the sweep");
    }

    #[test]
    fn the_history_dir_is_owner_only_and_exported_with_forward_slashes() {
        let dir = history_dir().expect("test data dir");
        assert!(oximux_owner_only::is_dir_restricted_to_owner(dir).unwrap());
        let (key, value) = env_pair().expect("env pair");
        assert_eq!(key, "OXIMUX_HISTORY_DIR");
        assert!(!value.contains('\\'));
    }
}
