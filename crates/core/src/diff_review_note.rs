//! Diff review notes — a reviewer's per-line comment on a diff.
//!
//! Lives in `oximux-core` so the diff view (compose / render) and storage
//! (`DiffReviewNoteRepo` row mapping) share one source of truth. Notes are
//! anchored to a stable `(repo, diff_ref, path, side, line)` coordinate
//! rather than a render-row index, so they re-attach to the right line when
//! the diff reopens — independent of folds, scroll, or split mode.

use serde::{Deserialize, Serialize};

/// Which side of a diff a note is anchored to.
///
/// A note on an added or context line anchors to the NEW side (its
/// `new_line`); a note on a removed line anchors to the OLD side (its
/// `old_line`). Keeping the side explicit means a note on a deleted line and
/// a note on the line that replaced it never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum NoteSide {
    Old,
    New,
}

impl NoteSide {
    /// Stable storage slug. New variants append, never rename.
    pub fn as_str(self) -> &'static str {
        match self {
            NoteSide::Old => "old",
            NoteSide::New => "new",
        }
    }

    /// Reconstruct from a stored slug. Unknown slug → `None` (callers degrade
    /// rather than panic on a corrupt row).
    pub fn from_slug(raw: &str) -> Option<NoteSide> {
        match raw {
            "old" => Some(NoteSide::Old),
            "new" => Some(NoteSide::New),
            _ => None,
        }
    }
}

/// One persisted review note — a single row in the `diff_review_notes` table.
///
/// `id` is the SQLite primary key (UUID); the natural key is the
/// `(repo, diff_ref, path, side, line)` anchor, which is `UNIQUE` so
/// re-annotating a line edits the existing note instead of duplicating it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReviewNote {
    pub id: String,
    /// Repository scope — the worktree / repo root path.
    pub repo: String,
    /// Diff scope identity (e.g. `worktree:unstaged`, `commit:<sha>`,
    /// `range:<base>..<head>`). Distinguishes notes on the same file across
    /// different diffs of that file.
    pub diff_ref: String,
    /// File path within the diff.
    pub path: String,
    /// Side the anchored line belongs to.
    pub side: NoteSide,
    /// 1-based line number on `side`.
    pub line: u32,
    /// The reviewer's note text.
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_side_slug_round_trips() {
        for side in [NoteSide::Old, NoteSide::New] {
            assert_eq!(NoteSide::from_slug(side.as_str()), Some(side));
        }
    }

    #[test]
    fn note_side_unknown_slug_is_none() {
        assert_eq!(NoteSide::from_slug("both"), None);
        assert_eq!(NoteSide::from_slug(""), None);
    }
}
