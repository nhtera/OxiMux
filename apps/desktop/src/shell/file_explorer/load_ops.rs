//! Async directory-load operations for `FileExplorer`.
//!
//! Extracted from `mod.rs` to keep that file under the 300-LOC hard limit.
//! All functions take `&mut FileExplorer` (via the methods on the entity).

use crate::shell::file_explorer::FileExplorer;
use crate::shell::file_explorer::fs_load::load_dir_cache;
use crate::shell::file_explorer::fs_watch::{as_project_path, is_open, touched_dirs};
use crate::shell::file_explorer::tree_state::DirCache;
use gpui::{Context, Task};
use std::collections::HashMap;
use std::path::Path;
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

        // Claim this directory for this read. Two reads of one directory can
        // be in flight at once — the watch fires again while the first is
        // still running, or a focus-regain refresh overlaps an expand — and
        // `read_dir` gives no promise about which finishes first. Without a
        // claim the last *completion* wins, so an older snapshot can land on
        // top of a newer one and leave a created file missing (or a deleted
        // one showing) until something else happens to reload that directory.
        let seq = self.claim_load(&dir_path);

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
                        if !load_is_current(&me.newest_load, &dir_path, seq) {
                            // Superseded while this read ran, by a newer read
                            // or by an invalidation. Its children describe a
                            // moment that has already been overtaken.
                            return;
                        }
                        me.newest_load.remove(&dir_path);
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

    /// Take the next sequence number for a read of `dir`, making it the newest
    /// load that directory has outstanding. Any earlier read of the same
    /// directory is superseded and will be discarded when it completes.
    fn claim_load(&mut self, dir: &Path) -> u64 {
        self.load_seq = self.load_seq.wrapping_add(1);
        let seq = self.load_seq;
        self.newest_load.insert(dir.to_path_buf(), seq);
        seq
    }

    /// Mark a cached-but-collapsed directory's listing as needing a fresh read
    /// the next time it is expanded, and discard any read of it still in
    /// flight.
    ///
    /// Both halves matter. Clearing `loaded` is what makes `toggle_dir` go back
    /// to disk. Dropping the claim is what stops a read that *started before*
    /// the change from landing afterwards and restoring `loaded = true` over
    /// the very listing we just declared suspect.
    pub(super) fn invalidate_dir(&mut self, dir: &Path) {
        if let Some(cached) = self.cache.get_mut(dir) {
            cached.loaded = false;
            self.newest_load.remove(dir);
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
            } else {
                // Collapsed, but its children are still cached from when it
                // was open, and `toggle_dir` re-reads only what is not
                // `loaded`. Without this a file created while the directory
                // was shut never appears, and a deleted one never leaves.
                self.invalidate_dir(&dir);
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

/// True when a finished read of `dir` may still be applied: it is the newest
/// one issued for that directory.
///
/// A missing entry means "no read of this directory is current" — either a
/// newer read already landed and cleared it, or an invalidation dropped the
/// claim — so a completion that finds nothing is stale, not merely unknown.
fn load_is_current(newest: &HashMap<PathBuf, u64>, dir: &Path, seq: u64) -> bool {
    newest.get(dir) == Some(&seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn newest(entries: &[(&str, u64)]) -> HashMap<PathBuf, u64> {
        entries
            .iter()
            .map(|(p, s)| (PathBuf::from(p), *s))
            .collect()
    }

    #[test]
    fn the_newest_read_of_a_directory_applies() {
        assert!(load_is_current(&newest(&[("/r/src", 7)]), Path::new("/r/src"), 7));
    }

    /// The race both review passes caught: a slow first read finishing after a
    /// fast second one must not put the older listing back.
    #[test]
    fn a_superseded_read_does_not_apply() {
        assert!(!load_is_current(&newest(&[("/r/src", 8)]), Path::new("/r/src"), 7));
    }

    /// An invalidation drops the claim, so a read that was already running
    /// when the directory changed cannot restore what it read beforehand.
    #[test]
    fn a_read_whose_claim_was_dropped_does_not_apply() {
        assert!(!load_is_current(&newest(&[]), Path::new("/r/src"), 7));
    }

    /// Claims are per directory: a read of one does not supersede a read of
    /// another that happens to have been issued earlier.
    #[test]
    fn claims_do_not_cross_directories() {
        let map = newest(&[("/r/src", 7), ("/r/docs", 8)]);
        assert!(load_is_current(&map, Path::new("/r/src"), 7));
        assert!(load_is_current(&map, Path::new("/r/docs"), 8));
    }
}
