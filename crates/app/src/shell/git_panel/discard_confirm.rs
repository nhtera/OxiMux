//! Per-row discard copy table.
//!
//! Classifies a `FileStatus` into one of three user-visible flavors of
//! "make this change go away":
//! - [`DiscardKind::Delete`] — untracked or staged-add: the file only
//!   exists locally (or only locally + in the index); discarding makes
//!   it disappear.
//! - [`DiscardKind::Restore`] — worktree- or index-deleted: the file is
//!   gone locally; restoring brings it back from HEAD.
//! - [`DiscardKind::Discard`] — everything else (modify / rename /
//!   copy / conflicted): revert the change in place.
//!
//! Copy is keyed off the kind so the host can build a [`ConfirmPrompt`]
//! with the right title / body / button label.

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

/// Resolved copy for a single discard request. The host pairs this
/// with `expected = file_name` to drive the type-to-confirm gate.
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

/// Short string the user types to confirm: the basename, falling back
/// to the full path string for path-segment-only entries (rare but
/// well-defined).
pub fn expected_for(file: &FileStatus) -> SharedString {
    display_name(file).into()
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
        FileStatus {
            path: PathBuf::from(path),
            index,
            worktree,
            rename: None,
        }
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
    fn expected_uses_basename() {
        let f = fs("nested/deep/file.rs", IndexStatus::Modified, WorktreeStatus::Modified);
        assert_eq!(expected_for(&f).as_ref(), "file.rs");
    }

    #[test]
    fn expected_falls_back_to_full_path_when_no_basename() {
        // A path that's just "." has no file_name; fallback is the
        // path's own string form.
        let f = fs(".", IndexStatus::Modified, WorktreeStatus::Modified);
        assert_eq!(expected_for(&f).as_ref(), ".");
    }
}
