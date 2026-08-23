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
    /// The diff line's text as it read when the note was written.
    ///
    /// A line number alone is not an anchor — it is a position, and positions
    /// move. Edit anything above a noted line and the number now addresses
    /// different code, so the note silently re-attaches to a line its author
    /// never saw and the prompt sent to an agent quotes the wrong source.
    /// Keeping the text lets the reader of these rows tell the two apart:
    /// same text at that number means still anchored, text found elsewhere in
    /// the file means it moved, text gone means the note has outlived its
    /// line.
    ///
    /// Empty for rows written before this column existed (and only those) —
    /// unverifiable rather than wrong, so those notes are left where they are.
    pub anchor_text: String,
    pub created_at: String,
    pub updated_at: String,
}

/// The comparable form of an anchor line: its text without surrounding
/// whitespace.
///
/// Ends are trimmed because neither end carries the identity of the line.
/// Re-indenting a block, or a formatter stripping trailing spaces, changes
/// every byte of the leading or trailing run while leaving the statement the
/// reviewer commented on exactly where it was — detaching those notes would
/// punish the tidiest edits hardest.
pub fn normalize_anchor_text(raw: &str) -> &str {
    raw.trim()
}

/// Whether an anchor text can be checked against a diff at all.
///
/// Two kinds cannot. A row written before the text was recorded has nothing
/// to compare, and a blank line normalizes to the empty string — which
/// matches every other blank line in the file, so "found it elsewhere" would
/// mean nothing. Both are left anchored where they are: an unverifiable note
/// is not a wrong one, and moving it on the strength of a match that carries
/// no information is how a correct note becomes a misleading one.
pub fn anchor_text_is_checkable(raw: &str) -> bool {
    !normalize_anchor_text(raw).is_empty()
}

/// Whether a stored anchor text and a live diff line are the same line.
pub fn anchor_text_matches(stored: &str, live: &str) -> bool {
    normalize_anchor_text(stored) == normalize_anchor_text(live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_and_trailing_space_do_not_break_an_anchor() {
        assert!(anchor_text_matches("let x = 1;", "    let x = 1;"));
        assert!(anchor_text_matches("let x = 1;   ", "let x = 1;"));
        assert!(!anchor_text_matches("let x = 1;", "let x = 2;"));
    }

    #[test]
    fn a_blank_line_is_not_a_checkable_anchor() {
        // Every blank line normalizes to the same empty string, so a match
        // against one says nothing about whether it is *the* line.
        assert!(!anchor_text_is_checkable(""));
        assert!(!anchor_text_is_checkable("   \t "));
        assert!(anchor_text_is_checkable("}"));
    }

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
