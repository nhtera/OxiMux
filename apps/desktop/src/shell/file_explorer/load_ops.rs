//! Async directory-load operations for `FileExplorer`.
//!
//! Extracted from `mod.rs` to keep that file under the 300-LOC hard limit.
//! All functions take `&mut FileExplorer` (via the methods on the entity).

use crate::shell::file_explorer::FileExplorer;
use crate::shell::file_explorer::fs_load::load_dir_cache;
use crate::shell::file_explorer::fs_watch::{as_project_path, is_open, touched_dirs};
use crate::shell::file_explorer::tree_state::DirCache;
use gpui::{Context, Task};
use notify_debouncer_full::DebounceEventResult;
use oximux_git::PollState;
use std::path::PathBuf;

/// Maximum retained in-flight load tasks. When exceeded, the oldest is
/// dropped (drop = cancel). Loads are idempotent so cancellation is safe.
pub const MAX_LOAD_TASKS: usize = 256;


impl FileExplorer {
    /// Spawn an async load for `dir_path`; on completion populate cache and
    /// recompute rows.
    ///
    /// `is_root` — when true, sets `self.root_loaded = true` on completion.
    pub(super) fn spawn_load_dir(
        &mut self,
        dir_path: PathBuf,
        repo_root: PathBuf,
        is_root: bool,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        // Mark as loading so paint_row can show "…" suffix.
        self.cache.entry(dir_path.clone()).or_default().loading = true;

        let dir_clone = dir_path.clone();
        let root_clone = repo_root.clone();

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let (tx, rx) = tokio::sync::oneshot::channel::<DirCache>();
                handle.spawn(async move {
                    let cache = load_dir_cache(dir_clone, root_clone).await;
                    let _ = tx.send(cache);
                });
                cx.spawn(async move |this, cx| {
                    let Ok(cache) = rx.await else {
                        return;
                    };
                    let _ = this.update(cx, |me, cx| {
                        me.cache.insert(dir_path, cache);
                        if is_root {
                            me.root_loaded = true;
                        }
                        me.recompute_rows();
                        // A reveal may have been waiting on this directory's
                        // children to materialize the target row.
                        me.try_scroll_pending_reveal();
                        cx.notify();
                    });
                })
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::file_explorer",
                    "no tokio runtime; dir load skipped"
                );
                cx.spawn(async move |_, _| {})
            }
        }
    }

    /// Push a task, capping the Vec at `MAX_LOAD_TASKS` by dropping the oldest.
    pub(super) fn push_task(&mut self, task: Task<()>) {
        if self._load_tasks.len() >= MAX_LOAD_TASKS {
            // Explicit drop cancels the in-flight load; idempotent so safe.
            drop(self._load_tasks.remove(0));
        }
        self._load_tasks.push(task);
    }

    /// Spawn the background task that mirrors incoming `PollState` changes.
    pub(super) fn start_poll_observer(
        mut rx: tokio::sync::watch::Receiver<PollState>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let state = rx.borrow_and_update().clone();
                if this
                    .update(cx, |me, cx| me.set_poll_state(state, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    /// Re-load every currently-expanded directory. Called on focus regain (H3).
    pub(super) fn refresh_expanded(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        let repo_root = self.repo_root.clone();
        for path in paths {
            let task = self.spawn_load_dir(path, repo_root.clone(), false, cx);
            self.push_task(task);
        }
    }

    /// Apply one debounced filesystem batch: re-read the open directories the
    /// batch touched, and nothing else.
    ///
    /// This is what makes a file written by an agent or a terminal — inside
    /// this window, with the window never losing focus — appear in the tree
    /// without the user pressing Refresh. The mapping from event paths to
    /// directories is `fs_watch::dirs_to_reload`, which is where the "only
    /// what is open" subtraction lives and where it is tested.
    ///
    /// A watch error is logged and dropped rather than surfaced: the panel
    /// keeps its focus-regain and manual refreshes, so a dead stream degrades
    /// this to the behaviour it had before the watch existed, which is not
    /// worth a toast in the user's way.
    pub(super) fn apply_fs_events(&mut self, result: DebounceEventResult, cx: &mut Context<Self>) {
        let events = match result {
            Ok(events) => events,
            Err(errors) => {
                for err in errors {
                    tracing::warn!(
                        target: "oximux_app::file_explorer",
                        %err,
                        "explorer watch error"
                    );
                }
                return;
            }
        };
        // No overflow cap here, deliberately. A peer tool caps its batch at
        // 5000 events and falls back to a blanket refresh, but its refresh
        // crosses an SSH mux — K × (1 + open dirs) remote round trips — while
        // ours is a local `read_dir` on a background task. Counting raw events
        // would also mis-fire exactly where it matters least: a `cargo build`
        // delivers a hundred thousand events that all belong to `target/` and
        // are discarded a microsecond each, so a cap would turn the commonest
        // background activity in this app into repeated full refreshes. The
        // measured worst case without one — a whole-tree checkout — is tens of
        // milliseconds, once.
        let paths = events
            .into_iter()
            .flat_map(|e| e.event.paths)
            .filter_map(|p| as_project_path(&p, &self.watch_root, &self.repo_root));

        let repo_root = self.repo_root.clone();
        let mut reloaded: Vec<PathBuf> = Vec::new();
        for dir in touched_dirs(paths, &repo_root) {
            if is_open(&dir, &repo_root, &self.expanded) {
                // The root's load carries `is_root` so a first-ever load
                // through this path still flips `root_loaded`; a re-read of it
                // is otherwise identical to any other directory's.
                let is_root = dir == repo_root;
                let task = self.spawn_load_dir(dir.clone(), repo_root.clone(), is_root, cx);
                self.push_task(task);
                reloaded.push(dir);
            } else if let Some(cached) = self.cache.get_mut(&dir) {
                // Collapsed, but its children are still cached from when it
                // was open, and `toggle_dir` re-reads only what is not
                // `loaded`. Clearing the flag is what makes the next expand go
                // back to disk — otherwise a file created while the directory
                // was shut never appears, and a deleted one never leaves.
                cached.loaded = false;
            }
        }
        if !reloaded.is_empty() {
            tracing::debug!(
                target: "oximux_app::file_explorer",
                dirs = ?reloaded,
                "watch fired; re-reading open directories"
            );
        }
    }

}
