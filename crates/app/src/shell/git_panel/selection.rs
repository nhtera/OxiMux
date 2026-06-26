//! Multi-select state mutators for `GitPanel`.
//!
//! Selection lives on `GitPanel` (the entity that already owns the
//! rendered row list); the methods on this `impl` block are the only
//! sanctioned ways to mutate `selected` and `last_clicked`. Keeping
//! them here — rather than inlined on `mod.rs` — caps `mod.rs` growth
//! below the 500-LOC warn budget and gives the click-routing logic a
//! single file to reason about.
//!
//! Click semantics (standard multi-select list behaviour):
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

    /// Replace the selection with `paths`. Used by Tree-mode Shift+click
    /// on a folder row to pull every leaf in the subtree into selection
    /// at once. No-op when `paths` is empty so a Shift+click on an
    /// already-empty folder doesn't blow away an existing selection.
    ///
    /// The anchor (`last_clicked`) is intentionally NOT updated — this
    /// is a wholesale replacement, not a range-extend, and picking the
    /// "first" of an unordered subtree as the new anchor would make a
    /// subsequent flat-row Shift+click range from a surprising lexical
    /// leaf. Preserving the prior anchor mirrors Finder's "Shift+click
    /// doesn't move the anchor" rule and keeps a follow-up range
    /// predictable.
    pub fn select_paths_replace(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        self.selected.clear();
        self.selected.extend(paths);
        cx.notify();
    }

    /// Add `paths` to `collapsed_dirs`. Used by Tree-mode Cmd+click on
    /// a folder to collapse every sibling folder while leaving the
    /// clicked one expanded (the caller passes the sibling list it
    /// computed off the rendered tree). No-op for an empty list.
    pub fn collapse_dirs(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        for p in paths {
            self.collapsed_dirs.insert(p);
        }
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
        match self.view_mode {
            oximux_core::ViewMode::Flat => flatten_visible_sections(
                &sections,
                &self.collapsed_sections,
                &self.expanded_row_sections,
            ),
            // Tree mode renders folder rows that consume cap slots, so the
            // visible-leaf set must run the SAME tree → flatten → truncate
            // pipeline as the renderer — the flat approximation would let a
            // range select leaves the cap pushed off screen.
            oximux_core::ViewMode::Tree => flatten_visible_tree_sections(
                &sections,
                &self.collapsed_sections,
                &self.expanded_row_sections,
                &self.collapsed_dirs,
            ),
        }
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
/// Tree-mode counterpart of [`flatten_visible_sections`]: replays the
/// renderer's exact pipeline (build tree → flatten with collapsed dirs →
/// truncate rendered rows to the cap) and keeps only the leaf paths, so
/// range selection matches what is actually on screen row-for-row.
pub(super) fn flatten_visible_tree_sections(
    sections: &FileSections<'_>,
    collapsed: &HashSet<&'static str>,
    expanded_rows: &HashSet<&'static str>,
    collapsed_dirs: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    use super::changed_files::SECTION_ROW_CAP;
    use crate::shell::source_control::tree::{NodeKind, TreeSection, build_tree, flatten};

    let mut out = Vec::new();
    let mut push_section =
        |title: &'static str, files: &[&FileStatus], section: TreeSection| {
            if collapsed.contains(title) {
                return;
            }
            let tree = build_tree(files.iter().copied(), section);
            let mut rows = flatten(&tree, collapsed_dirs);
            if !expanded_rows.contains(title) && rows.len() > SECTION_ROW_CAP {
                rows.truncate(SECTION_ROW_CAP);
            }
            out.extend(
                rows.into_iter()
                    .filter(|r| matches!(r.kind, NodeKind::File))
                    .map(|r| r.path),
            );
        };
    push_section("CHANGES", &sections.unstaged, TreeSection::Unstaged);
    push_section("STAGED CHANGES", &sections.staged, TreeSection::Staged);
    push_section("UNTRACKED FILES", &sections.untracked, TreeSection::Untracked);
    out
}

pub(super) fn flatten_visible_sections(
    sections: &FileSections<'_>,
    collapsed: &HashSet<&'static str>,
    expanded_rows: &HashSet<&'static str>,
) -> Vec<PathBuf> {
    use super::changed_files::SECTION_ROW_CAP;
    let mut out = Vec::with_capacity(
        sections.unstaged.len() + sections.staged.len() + sections.untracked.len(),
    );
    // Mirror of the FLAT renderer's visibility rules: collapsed sections
    // hide everything, capped sections hide rows past `SECTION_ROW_CAP` —
    // a Shift+Click range must never grab a row the user can't see. Tree
    // mode goes through `flatten_visible_tree_sections` instead.
    let mut push_section = |title: &'static str, files: &[&FileStatus]| {
        if collapsed.contains(title) {
            return;
        }
        let cap = if expanded_rows.contains(title) {
            files.len()
        } else {
            SECTION_ROW_CAP
        };
        for f in files.iter().take(cap) {
            out.push(f.path.clone());
        }
    };
    push_section("CHANGES", &sections.unstaged);
    push_section("STAGED CHANGES", &sections.staged);
    push_section("UNTRACKED FILES", &sections.untracked);
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
    fn flatten_respects_section_row_cap_until_expanded() {
        use super::super::changed_files::SECTION_ROW_CAP;
        let u: Vec<FileStatus> = (0..SECTION_ROW_CAP + 3)
            .map(|i| fs(&format!("f{i:02}.rs")))
            .collect();
        let sections = sections_from(&u, &[], &[]);
        let collapsed = HashSet::new();

        let capped = flatten_visible_sections(&sections, &collapsed, &HashSet::new());
        assert_eq!(capped.len(), SECTION_ROW_CAP, "capped section hides the tail");
        assert!(!capped.contains(&PathBuf::from(format!("f{:02}.rs", SECTION_ROW_CAP))));

        let mut expanded = HashSet::new();
        expanded.insert("CHANGES");
        let full = flatten_visible_sections(&sections, &collapsed, &expanded);
        assert_eq!(full.len(), SECTION_ROW_CAP + 3, "expanded section shows all");
    }

    #[test]
    fn tree_flatten_counts_folder_rows_against_the_cap() {
        use super::super::changed_files::SECTION_ROW_CAP;
        // One folder + (CAP + 2) leaves inside it: the renderer shows the
        // folder row plus the first CAP-1 leaves, so selection must too.
        let u: Vec<FileStatus> = (0..SECTION_ROW_CAP + 2)
            .map(|i| fs(&format!("dir/f{i:02}.rs")))
            .collect();
        let sections = sections_from(&u, &[], &[]);
        let got = flatten_visible_tree_sections(
            &sections,
            &HashSet::new(),
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(
            got.len(),
            SECTION_ROW_CAP - 1,
            "folder row consumes one cap slot"
        );

        let mut expanded = HashSet::new();
        expanded.insert("CHANGES");
        let full = flatten_visible_tree_sections(
            &sections,
            &HashSet::new(),
            &expanded,
            &HashSet::new(),
        );
        assert_eq!(full.len(), SECTION_ROW_CAP + 2, "expanded shows every leaf");
    }

    #[test]
    fn flatten_includes_all_sections_when_none_collapsed() {
        let u = [fs("a.rs"), fs("b.rs")];
        let s = [fs("c.rs")];
        let t = [fs("d.rs")];
        let sections = sections_from(&u, &s, &t);
        let collapsed: HashSet<&'static str> = HashSet::new();
        let got = flatten_visible_sections(&sections, &collapsed, &HashSet::new());
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
        let got = flatten_visible_sections(&sections, &collapsed, &HashSet::new());
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
        let got = flatten_visible_sections(&sections, &collapsed, &HashSet::new());
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
        let got = flatten_visible_sections(&sections, &collapsed, &HashSet::new());
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
        let got = flatten_visible_sections(&sections, &collapsed, &HashSet::new());
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
