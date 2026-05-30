//! Multi-select state mutators for `GitPanel`.
//!
//! Selection lives on `GitPanel` (the entity that already owns the
//! rendered row list); the methods on this `impl` block are the only
//! sanctioned ways to mutate `selected` and `last_clicked`. Keeping
//! them here — rather than inlined on `mod.rs` — caps `mod.rs` growth
//! below the 500-LOC warn budget and gives the click-routing logic a
//! single file to reason about.
//!
//! Click semantics (matching Finder / VSCode list behaviour):
//! - **Bare click** → [`select_only`] — replace selection with one path,
//!   update anchor.
//! - **Cmd-click** → [`toggle_selection`] — flip one path in/out of
//!   selection, update anchor to the clicked row.
//! - **Shift-click** → [`extend_range_to`] — replace selection with the
//!   inclusive range from `last_clicked` to the clicked row in flat
//!   render order. Anchor stays put so repeated Shift+Clicks re-anchor
//!   from the same starting row.
//! - **Escape / empty-area click** (host-routed) → [`clear_selection`].
//!
//! Selection survives poll ticks because it's keyed by `PathBuf`, not
//! by `FileStatus` identity or render index. A path that vanishes from
//! `git_state` (e.g. user discarded externally) lingers in `selected`
//! until the next selection event — harmless: chord actions iterate
//! `selected` and the vectorised repo ops no-op or log on unknown paths.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::changed_files::{FileSections, partition_files};
use crate::shell::git_panel::range_select;
use crate::shell::source_control::filter::filter_files;
use gpui::Context;
use oximux_core::FileStatus;
use std::collections::HashSet;
use std::path::PathBuf;

impl GitPanel {
    /// Bare click — replace selection with `path`, reset anchor to it.
    pub fn select_only(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected.clear();
        self.selected.insert(path.clone());
        self.last_clicked = Some(path);
        cx.notify();
    }

    /// Cmd-click — flip `path` in / out of `selected` and update the
    /// anchor to it. A second Cmd-click on the same row removes it but
    /// keeps the anchor (so a follow-up Shift+Click still ranges from
    /// that point — matches Finder).
    pub fn toggle_selection(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.selected.remove(&path) {
            self.selected.insert(path.clone());
        }
        self.last_clicked = Some(path);
        cx.notify();
    }

    /// Shift-click — replace `selected` with the inclusive range from
    /// `last_clicked` to `path` in flat render order. When no anchor is
    /// set (first interaction after panel mount / clear), degrades to
    /// [`select_only`].
    pub fn extend_range_to(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(anchor) = self.last_clicked.clone() else {
            self.select_only(path, cx);
            return;
        };
        let rows = self.flat_visible_paths();
        let range = range_select::range_between(&rows, anchor.as_path(), path.as_path());
        self.selected.clear();
        self.selected.extend(range);
        // Anchor stays at `last_clicked`. A second Shift+Click against
        // the same anchor reranges from there — not from the previous
        // shift-target.
        cx.notify();
    }

    /// Drop the entire selection + anchor. Called from the bulk-op
    /// success path (Slice B) and a future BulkActionBar `× Clear`
    /// button.
    ///
    /// Project switching does NOT need to call this directly: the
    /// right-sidebar rebuild on `set_active_project` constructs a fresh
    /// `GitPanel` entity (see `right_sidebar/mod.rs::rebuild`), and the
    /// old one drops with its selection. If a future refactor preserves
    /// the GitPanel entity across project switches, the callsite needs
    /// to invoke this method explicitly to avoid stale paths reaching
    /// the new project's `repo.stage_paths`.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if !self.selected.is_empty() || self.last_clicked.is_some() {
            self.selected.clear();
            self.last_clicked = None;
            cx.notify();
        }
    }

    /// Re-derive the flat path order the renderer walks: CHANGES
    /// (unstaged) → STAGED CHANGES → UNTRACKED FILES, each in section
    /// order. Used by [`extend_range_to`] so range computation doesn't
    /// need a per-render snapshot stored on `Self`. Cheap — same passes
    /// `Render::render` already runs each tick.
    ///
    /// Collapsed sections are excluded. `changed_files::section()`
    /// skips rendering rows when its title is in `collapsed_sections`,
    /// so those paths are invisible to the user; including them in a
    /// Shift+Click range would silently select rows the user can't see.
    ///
    /// Partial-stage rows mirror a path into Unstaged AND Staged; the
    /// returned `Vec` carries the path twice in that case. `range_between`
    /// dedupes on output — selection stays keyed by path.
    pub fn flat_visible_paths(&self) -> Vec<PathBuf> {
        let Some(state) = self.git_state.as_ref() else {
            return Vec::new();
        };
        let filtered: Vec<FileStatus> = filter_files(&state.files, &self.filter_query)
            .into_iter()
            .cloned()
            .collect();
        let sections = partition_files(&filtered);
        flatten_visible_sections(&sections, &self.collapsed_sections)
    }

    /// Vectorised stage for every path in `selected`. No-op when the
    /// selection is empty or another bulk op is already running. Sets
    /// [`bulk_op_in_flight`] for the duration of the op so the
    /// `BulkActionBar` swaps the count for a spinner and disables both
    /// action buttons. On success the selection is cleared (the rows
    /// just moved to STAGED — the prior selection is no longer
    /// meaningful); on failure the selection is preserved so the user
    /// can retry.
    ///
    /// Slice A wired this from the chord action (`S` key). Slice B
    /// adds the `BulkActionBar` UI button that calls the same method.
    ///
    /// [`bulk_op_in_flight`]: super::GitPanel::bulk_op_in_flight
    pub fn bulk_stage_selected(&mut self, cx: &mut Context<Self>) {
        self.run_bulk_path_op(cx, "bulk_stage_selected", |repo, paths| async move {
            let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
            repo.stage_paths(&refs).await
        });
    }

    /// Vectorised unstage for every path in `selected`. Same shape as
    /// [`bulk_stage_selected`].
    pub fn bulk_unstage_selected(&mut self, cx: &mut Context<Self>) {
        self.run_bulk_path_op(cx, "bulk_unstage_selected", |repo, paths| async move {
            let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
            repo.unstage_paths(&refs).await
        });
    }

    /// Common driver for vectorised path ops: snapshot the selection,
    /// flip [`bulk_op_in_flight`], spawn the work on tokio, and
    /// reconcile state when the op resolves (clear selection +
    /// `in_flight` on success; just clear `in_flight` on failure).
    ///
    /// Mirrors `confirmed_discard_path`'s oneshot + cx.spawn shape so
    /// concurrent ops don't cancel each other and the UI always sees
    /// a well-defined `in_flight` edge.
    ///
    /// `op` takes an owned `Repository` plus owned `Vec<PathBuf>` —
    /// passing owned paths sidesteps the lifetime-stuck-in-the-future
    /// trap a borrowed-`&Path` signature creates.
    ///
    /// [`bulk_op_in_flight`]: super::GitPanel::bulk_op_in_flight
    fn run_bulk_path_op<F, Fut>(&mut self, cx: &mut Context<Self>, label: &'static str, op: F)
    where
        F: FnOnce(oximux_git::Repository, Vec<PathBuf>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = oximux_git::Result<()>> + Send + 'static,
    {
        if self.selected.is_empty() || self.bulk_op_in_flight {
            return;
        }
        let paths: Vec<PathBuf> = self.selected.iter().cloned().collect();
        self.bulk_op_in_flight = true;
        cx.notify();

        let repo = self.repo.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = op(repo, paths).await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::git_panel",
                    op = label,
                    "no tokio runtime; bulk op skipped (test wiring)"
                );
                self.bulk_op_in_flight = false;
                cx.notify();
                return;
            }
        }

        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                panel.bulk_op_in_flight = false;
                match result {
                    Ok(Ok(())) => {
                        // Success — the rows have moved (e.g. unstaged
                        // → staged) and the prior selection no longer
                        // corresponds to the same logical bucket. Drop
                        // it so the BulkActionBar dismisses and the
                        // user starts a fresh selection on the new
                        // layout.
                        panel.selected.clear();
                        panel.last_clicked = None;
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(
                            target: "oximux_app::git_panel",
                            error = %err,
                            op = label,
                            "bulk path op failed; selection preserved for retry"
                        );
                    }
                    Err(_) => {
                        // Sender dropped (panic on the tokio side).
                        // Same handling as a logged error — clear the
                        // flag, leave selection intact.
                        tracing::warn!(
                            target: "oximux_app::git_panel",
                            op = label,
                            "bulk path op sender dropped before sending result"
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Pure helper extracted from [`GitPanel::flat_visible_paths`] for unit
/// testing the collapsed-section filter. Section title strings match
/// the `&'static str` keys passed to `changed_files::section()` in
/// `render_sections` and stored in `GitPanel::collapsed_sections`.
pub(super) fn flatten_visible_sections(
    sections: &FileSections<'_>,
    collapsed: &HashSet<&'static str>,
) -> Vec<PathBuf> {
    let unstaged_visible = !collapsed.contains("CHANGES");
    let staged_visible = !collapsed.contains("STAGED CHANGES");
    let untracked_visible = !collapsed.contains("UNTRACKED FILES");
    let mut out = Vec::with_capacity(
        sections.unstaged.len() + sections.staged.len() + sections.untracked.len(),
    );
    if unstaged_visible {
        for f in sections.unstaged.iter() {
            out.push(f.path.clone());
        }
    }
    if staged_visible {
        for f in sections.staged.iter() {
            out.push(f.path.clone());
        }
    }
    if untracked_visible {
        for f in sections.untracked.iter() {
            out.push(f.path.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};

    fn fs(path: &str) -> FileStatus {
        FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Modified,
            WorktreeStatus::Modified,
        )
    }

    fn sections_from<'a>(
        u: &'a [FileStatus],
        s: &'a [FileStatus],
        t: &'a [FileStatus],
    ) -> FileSections<'a> {
        FileSections {
            unstaged: u.iter().collect(),
            staged: s.iter().collect(),
            untracked: t.iter().collect(),
        }
    }

    #[test]
    fn flatten_includes_all_sections_when_none_collapsed() {
        let u = [fs("a.rs"), fs("b.rs")];
        let s = [fs("c.rs")];
        let t = [fs("d.rs")];
        let sections = sections_from(&u, &s, &t);
        let collapsed: HashSet<&'static str> = HashSet::new();
        let got = flatten_visible_sections(&sections, &collapsed);
        assert_eq!(
            got,
            vec![
                PathBuf::from("a.rs"),
                PathBuf::from("b.rs"),
                PathBuf::from("c.rs"),
                PathBuf::from("d.rs"),
            ]
        );
    }

    #[test]
    fn flatten_excludes_collapsed_staged_section() {
        // M1 regression guard: a Shift+Click range from CHANGES to
        // UNTRACKED while STAGED CHANGES is collapsed must NOT pick up
        // the invisible staged path.
        let u = [fs("a.rs")];
        let s = [fs("invisible.rs")];
        let t = [fs("b.rs")];
        let sections = sections_from(&u, &s, &t);
        let mut collapsed: HashSet<&'static str> = HashSet::new();
        collapsed.insert("STAGED CHANGES");
        let got = flatten_visible_sections(&sections, &collapsed);
        assert_eq!(got, vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
    }

    #[test]
    fn flatten_excludes_collapsed_unstaged_section() {
        let u = [fs("hidden.rs")];
        let s = [fs("c.rs")];
        let t = [fs("d.rs")];
        let sections = sections_from(&u, &s, &t);
        let mut collapsed: HashSet<&'static str> = HashSet::new();
        collapsed.insert("CHANGES");
        let got = flatten_visible_sections(&sections, &collapsed);
        assert_eq!(got, vec![PathBuf::from("c.rs"), PathBuf::from("d.rs")]);
    }

    #[test]
    fn flatten_returns_empty_when_all_sections_collapsed() {
        let u = [fs("a.rs")];
        let s = [fs("b.rs")];
        let t = [fs("c.rs")];
        let sections = sections_from(&u, &s, &t);
        let mut collapsed: HashSet<&'static str> = HashSet::new();
        collapsed.insert("CHANGES");
        collapsed.insert("STAGED CHANGES");
        collapsed.insert("UNTRACKED FILES");
        let got = flatten_visible_sections(&sections, &collapsed);
        assert!(got.is_empty());
    }

    #[test]
    fn flatten_preserves_section_order_when_filtering() {
        // Order rule: unstaged → staged → untracked. Excluding the
        // middle section concatenates the first and last without
        // reshuffling.
        let u = [fs("u1.rs"), fs("u2.rs")];
        let s = [fs("s1.rs")];
        let t = [fs("t1.rs"), fs("t2.rs")];
        let sections = sections_from(&u, &s, &t);
        let mut collapsed: HashSet<&'static str> = HashSet::new();
        collapsed.insert("STAGED CHANGES");
        let got = flatten_visible_sections(&sections, &collapsed);
        assert_eq!(
            got,
            vec![
                PathBuf::from("u1.rs"),
                PathBuf::from("u2.rs"),
                PathBuf::from("t1.rs"),
                PathBuf::from("t2.rs"),
            ]
        );
    }
}
