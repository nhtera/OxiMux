//! Async directory-load operations for `FileExplorer`.
//!
//! Extracted from `mod.rs` to keep that file under the 300-LOC hard limit.
//! All functions take `&mut FileExplorer` (via the methods on the entity).

use crate::shell::file_explorer::FileExplorer;
use crate::shell::file_explorer::fs_load::load_dir_cache;
use crate::shell::file_explorer::tree_state::DirCache;
use gpui::{Context, Task};
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
}
