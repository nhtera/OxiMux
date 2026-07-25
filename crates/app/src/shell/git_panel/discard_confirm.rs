//! Per-row + per-section discard copy table.
//!
//! Per-row classifies a `FileStatus` into one of three user-visible
//! flavors of "make this change go away":
//! - [`DiscardKind::Delete`] — untracked or staged-add: the file only
//!   exists locally (or only locally + in the index); discarding makes
//!   it disappear.
//! - [`DiscardKind::Restore`] — worktree- or index-deleted: the file is
//!   gone locally; restoring brings it back from HEAD.
//! - [`DiscardKind::Discard`] — everything else (modify / rename /
//!   copy / conflicted): revert the change in place.
//!
//! Per-section (Phase 03 Slice C) is keyed by [`DiscardAllArea`] — the
//! section the user clicked "Discard all" / "Delete all" on. The copy
//! is plural-aware so N=1 reads naturally.
//!
//! Both code paths produce a [`DiscardCopy`] the host feeds straight
//! into [`ConfirmPrompt`].

use gpui::SharedString;
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};

/// User-facing flavor of a discard, used to pick title / body / button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardKind {
    /// Untracked or staged-added — the file only exists locally; the
    /// op makes it disappear.
    Delete,
    /// Worktree- or index-deleted — the file is gone locally; the op
    /// restores it from HEAD.
    Restore,
    /// Everything else — modify, rename, copy, conflict; the op reverts
    /// the change in place.
    Discard,
}

/// Which SCM section the user clicked "Discard all" / "Delete all"
/// on. Drives both the [`copy_for_area`] copy table and the discard
/// sequence run by `GitPanel::confirmed_discard_area` (staged-area
/// runs `unstage_paths` first, then `discard_paths`; unstaged and
/// untracked go straight to `discard_paths`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardAllArea {
    /// CHANGES section — files modified in the worktree but not yet
    /// staged. "Discard all unstaged changes?"
    Unstaged,
    /// STAGED CHANGES section — files whose changes sit in the index.
    /// `confirmed_discard_area` runs `unstage_paths` first, then
    /// `discard_paths` so HEAD content lands in the worktree too.
    Staged,
    /// UNTRACKED FILES section — files not tracked by git at all.
    /// "Delete all untracked files?" (`Delete` button label, the op
    /// permanently removes the worktree files).
    Untracked,
}

/// Resolved copy for a single discard request — everything the modal
/// needs to state what it is about to do.
#[derive(Debug, Clone)]
pub struct DiscardCopy {
    pub title: SharedString,
    pub body: SharedString,
    pub confirm_label: SharedString,
}

/// Map a `FileStatus` to the (kind, copy) pair the modal should show.
pub fn copy_for(file: &FileStatus) -> (DiscardKind, DiscardCopy) {
    let display = display_name(file);

    if is_delete(file) {
        return (
            DiscardKind::Delete,
            DiscardCopy {
                title: format!("Delete \"{display}\"?").into(),
                body: "This will permanently delete this file. This cannot be undone.".into(),
                confirm_label: "Delete".into(),
            },
        );
    }

    if is_restore(file) {
        return (
            DiscardKind::Restore,
            DiscardCopy {
                title: format!("Restore \"{display}\"?").into(),
                body: "This will restore the file from HEAD and discard the deletion.".into(),
                confirm_label: "Restore".into(),
            },
        );
    }

    (
        DiscardKind::Discard,
        DiscardCopy {
            title: format!("Discard changes to \"{display}\"?").into(),
            body: "This will revert all changes to this file. This cannot be undone.".into(),
            confirm_label: "Discard".into(),
        },
    )
}

/// Copy for the "Discard all" / "Delete all" section-header action.
/// `n` is the file count in the section — copy reads naturally for
/// `n == 1` ("this file" instead of "1 files"); for `n > 1` the count
/// is interpolated.
///
/// Caller is expected to guard against `n == 0` (the section header
/// shouldn't surface "Discard all" buttons on an empty section).
/// The `debug_assert!` makes the contract explicit so a future
/// caller that skips the guard catches it in debug builds; release
/// builds fall through to the plural branch with `0` interpolated.
pub fn copy_for_area(area: DiscardAllArea, n: usize) -> DiscardCopy {
    debug_assert!(
        n > 0,
        "copy_for_area called with n=0 — guard at the call site with `is_empty()`"
    );
    match area {
        DiscardAllArea::Untracked => {
            let (title, body, confirm) = if n == 1 {
                (
                    "Delete this untracked file?".to_string(),
                    "This will permanently delete the file. This cannot be undone.".to_string(),
                    "Delete",
                )
            } else {
                (
                    format!("Delete {n} untracked files?"),
                    "This will permanently delete the files. This cannot be undone.".to_string(),
                    "Delete all",
                )
            };
            DiscardCopy {
                title: title.into(),
                body: body.into(),
                confirm_label: confirm.into(),
            }
        }
        DiscardAllArea::Staged => {
            let title = if n == 1 {
                "Discard this staged change?".to_string()
            } else {
                format!("Discard all {n} staged changes?")
            };
            DiscardCopy {
                title: title.into(),
                body: "This will unstage the changes and revert the files to their committed \
                       state. This cannot be undone."
                    .into(),
                confirm_label: if n == 1 { "Discard" } else { "Discard all" }.into(),
            }
        }
        DiscardAllArea::Unstaged => {
            let title = if n == 1 {
                "Discard this unstaged change?".to_string()
            } else {
                format!("Discard all {n} unstaged changes?")
            };
            DiscardCopy {
                title: title.into(),
                body: "This will revert the files to their staged or committed state. This cannot \
                       be undone."
                    .into(),
                confirm_label: if n == 1 { "Discard" } else { "Discard all" }.into(),
            }
        }
    }
}

fn display_name(file: &FileStatus) -> String {
    file.path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.path.to_string_lossy().into_owned())
}

fn is_delete(file: &FileStatus) -> bool {
    // Pure-untracked: file lives only on disk.
    let pure_untracked = matches!(file.index, IndexStatus::Untracked)
        && matches!(file.worktree, WorktreeStatus::Untracked);
    // Staged-added: tracked-but-new; discarding drops the index entry
    // AND the worktree file from the user's view.
    let staged_added = matches!(file.index, IndexStatus::Added);
    pure_untracked || staged_added
}

fn is_restore(file: &FileStatus) -> bool {
    // Deletion in either column means the file is missing locally. We
    // intentionally let `is_delete` short-circuit first so a
    // staged-add never claims Restore.
    matches!(file.index, IndexStatus::Deleted) || matches!(file.worktree, WorktreeStatus::Deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fs(path: &str, index: IndexStatus, worktree: WorktreeStatus) -> FileStatus {
        FileStatus::with_status(PathBuf::from(path), index, worktree)
    }

    #[test]
    fn untracked_classifies_as_delete() {
        let f = fs("src/new.rs", IndexStatus::Untracked, WorktreeStatus::Untracked);
        let (kind, copy) = copy_for(&f);
        assert_eq!(kind, DiscardKind::Delete);
        assert!(copy.title.contains("Delete"));
        assert!(copy.title.contains("\"new.rs\""));
        assert_eq!(copy.confirm_label.as_ref(), "Delete");
    }

    #[test]
    fn staged_added_classifies_as_delete() {
        let f = fs("a.rs", IndexStatus::Added, WorktreeStatus::Unmodified);
        let (kind, _) = copy_for(&f);
        assert_eq!(kind, DiscardKind::Delete);
    }

    #[test]
    fn worktree_deleted_classifies_as_restore() {
        let f = fs("removed.rs", IndexStatus::Unmodified, WorktreeStatus::Deleted);
        let (kind, copy) = copy_for(&f);
        assert_eq!(kind, DiscardKind::Restore);
        assert!(copy.title.contains("Restore"));
        assert_eq!(copy.confirm_label.as_ref(), "Restore");
    }

    #[test]
    fn index_deleted_classifies_as_restore() {
        let f = fs("removed.rs", IndexStatus::Deleted, WorktreeStatus::Unmodified);
        let (kind, _) = copy_for(&f);
        assert_eq!(kind, DiscardKind::Restore);
    }

    #[test]
    fn modified_classifies_as_discard() {
        let f = fs("touched.rs", IndexStatus::Unmodified, WorktreeStatus::Modified);
        let (kind, copy) = copy_for(&f);
        assert_eq!(kind, DiscardKind::Discard);
        assert!(copy.title.starts_with("Discard changes to"));
        assert_eq!(copy.confirm_label.as_ref(), "Discard");
    }

    #[test]
    fn staged_modified_classifies_as_discard() {
        let f = fs("touched.rs", IndexStatus::Modified, WorktreeStatus::Unmodified);
        assert_eq!(copy_for(&f).0, DiscardKind::Discard);
    }

    #[test]
    fn renamed_classifies_as_discard() {
        // Rename in either column. Reverting the rename keeps the file
        // — semantically a Discard, not a Restore.
        let f = fs("dst.rs", IndexStatus::Renamed, WorktreeStatus::Unmodified);
        assert_eq!(copy_for(&f).0, DiscardKind::Discard);
        let f2 = fs("dst.rs", IndexStatus::Unmodified, WorktreeStatus::Renamed);
        assert_eq!(copy_for(&f2).0, DiscardKind::Discard);
    }

    #[test]
    fn copied_classifies_as_discard() {
        let f = fs("dst.rs", IndexStatus::Copied, WorktreeStatus::Unmodified);
        assert_eq!(copy_for(&f).0, DiscardKind::Discard);
    }

    #[test]
    fn conflicted_classifies_as_discard() {
        let f = fs("conflict.rs", IndexStatus::Unmerged, WorktreeStatus::Unmerged);
        assert_eq!(copy_for(&f).0, DiscardKind::Discard);
    }

    #[test]
    fn deletion_wins_over_modified() {
        // If the index says deleted but the worktree column shows
        // modified (rare, surfaces in some merge races), the user-
        // visible reality is "file is gone" → Restore.
        let f = fs("gone.rs", IndexStatus::Deleted, WorktreeStatus::Modified);
        assert_eq!(copy_for(&f).0, DiscardKind::Restore);
    }

    #[test]
    fn staged_added_beats_worktree_deleted() {
        // `added` short-circuits ahead of `restore` because the file
        // was Just Added; the user thinks of it as "delete this new
        // thing", not "restore the deletion".
        let f = fs("staged.rs", IndexStatus::Added, WorktreeStatus::Deleted);
        assert_eq!(copy_for(&f).0, DiscardKind::Delete);
    }

    #[test]
    fn title_uses_basename() {
        let f = fs("nested/deep/file.rs", IndexStatus::Modified, WorktreeStatus::Modified);
        assert!(copy_for(&f).1.title.contains("\"file.rs\""));
    }

    #[test]
    fn title_falls_back_to_full_path_when_no_basename() {
        // A path that's just "." has no file_name; fallback is the
        // path's own string form.
        let f = fs(".", IndexStatus::Modified, WorktreeStatus::Modified);
        assert!(copy_for(&f).1.title.contains("\".\""));
    }

    // ---------- copy_for_area (Slice C) ----------

    #[test]
    fn untracked_area_singular_uses_delete_label() {
        let c = copy_for_area(DiscardAllArea::Untracked, 1);
        assert!(c.title.starts_with("Delete this untracked file"));
        assert_eq!(c.confirm_label.as_ref(), "Delete");
    }

    #[test]
    fn untracked_area_plural_interpolates_count() {
        let c = copy_for_area(DiscardAllArea::Untracked, 5);
        assert!(c.title.contains("Delete 5 untracked files"));
        assert_eq!(c.confirm_label.as_ref(), "Delete all");
    }

    #[test]
    fn staged_area_singular_drops_the_all() {
        let c = copy_for_area(DiscardAllArea::Staged, 1);
        assert!(c.title.starts_with("Discard this staged change"));
        assert_eq!(c.confirm_label.as_ref(), "Discard");
    }

    #[test]
    fn staged_area_plural_says_all_n() {
        let c = copy_for_area(DiscardAllArea::Staged, 3);
        assert!(c.title.contains("Discard all 3 staged changes"));
        assert_eq!(c.confirm_label.as_ref(), "Discard all");
    }

    #[test]
    fn unstaged_area_singular() {
        let c = copy_for_area(DiscardAllArea::Unstaged, 1);
        assert!(c.title.starts_with("Discard this unstaged change"));
        assert_eq!(c.confirm_label.as_ref(), "Discard");
    }

    #[test]
    fn unstaged_area_plural() {
        let c = copy_for_area(DiscardAllArea::Unstaged, 7);
        assert!(c.title.contains("Discard all 7 unstaged changes"));
        assert_eq!(c.confirm_label.as_ref(), "Discard all");
    }

    #[test]
    fn staged_and_unstaged_bodies_are_count_invariant() {
        // Staged + Unstaged body is informational and singular-shaped
        // ("revert the files to their committed state"). Only the
        // title carries the count. Untracked body intentionally swaps
        // "the file" ↔ "the files" so the prose reads naturally for
        // a 1-file delete.
        assert_eq!(
            copy_for_area(DiscardAllArea::Staged, 1).body,
            copy_for_area(DiscardAllArea::Staged, 4).body
        );
        assert_eq!(
            copy_for_area(DiscardAllArea::Unstaged, 1).body,
            copy_for_area(DiscardAllArea::Unstaged, 12).body
        );
    }

    #[test]
    fn area_bodies_reach_for_the_right_verb() {
        // Untracked = "permanently delete"; Staged + Unstaged =
        // "revert" / "unstage". Catches a copy-paste mistake where
        // the wrong area pastes the wrong verb.
        assert!(
            copy_for_area(DiscardAllArea::Untracked, 1)
                .body
                .contains("delete")
        );
        assert!(
            copy_for_area(DiscardAllArea::Staged, 1)
                .body
                .contains("unstage")
        );
        assert!(
            copy_for_area(DiscardAllArea::Unstaged, 1)
                .body
                .contains("revert")
        );
    }
}
