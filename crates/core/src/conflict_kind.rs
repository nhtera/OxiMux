//! Three-way-merge conflict taxonomy.
//!
//! Porcelain v2 emits one `u` record per unmerged path with an XY pair that
//! encodes how the two sides diverged. The pair is the only conflict signal
//! the v2 format exposes — the stage triple `(m1, m2, m3)` carries file
//! modes, not status — so the seven kinds below mirror git's own XY space.
//!
//! Reference: `git status --porcelain=v2` "Unmerged entries" section in
//! `man git-status`. Code points for the seven legal combinations are:
//!
//! | XY | Variant               | Label              |
//! |----|-----------------------|--------------------|
//! | DD | `BothDeleted`         | both deleted       |
//! | AU | `AddedByUs`           | added by us        |
//! | UD | `DeletedByThem`       | deleted by them    |
//! | UA | `AddedByThem`         | added by them      |
//! | DU | `DeletedByUs`         | deleted by us      |
//! | AA | `BothAdded`           | both added         |
//! | UU | `BothModified`        | both modified      |

use serde::{Deserialize, Serialize};

/// Conflict classification for an unmerged path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    BothDeleted,
    AddedByUs,
    DeletedByThem,
    AddedByThem,
    DeletedByUs,
    BothAdded,
    BothModified,
}

impl ConflictKind {
    /// Map the porcelain-v2 XY byte pair on a `u` record. Returns `None`
    /// for any other combination — callers treat that as "unknown conflict
    /// shape" rather than crashing the poll cycle.
    pub fn from_xy(x: u8, y: u8) -> Option<Self> {
        Some(match (x, y) {
            (b'D', b'D') => Self::BothDeleted,
            (b'A', b'U') => Self::AddedByUs,
            (b'U', b'D') => Self::DeletedByThem,
            (b'U', b'A') => Self::AddedByThem,
            (b'D', b'U') => Self::DeletedByUs,
            (b'A', b'A') => Self::BothAdded,
            (b'U', b'U') => Self::BothModified,
            _ => return None,
        })
    }

    /// One-line human label — what the SCM panel renders under the file
    /// name on a conflict row.
    pub fn label(self) -> &'static str {
        match self {
            Self::BothDeleted => "both deleted",
            Self::AddedByUs => "added by us",
            Self::DeletedByThem => "deleted by them",
            Self::AddedByThem => "added by them",
            Self::DeletedByUs => "deleted by us",
            Self::BothAdded => "both added",
            Self::BothModified => "both modified",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_seven_xy_pairs_map() {
        let cases = [
            (b'D', b'D', ConflictKind::BothDeleted, "both deleted"),
            (b'A', b'U', ConflictKind::AddedByUs, "added by us"),
            (b'U', b'D', ConflictKind::DeletedByThem, "deleted by them"),
            (b'U', b'A', ConflictKind::AddedByThem, "added by them"),
            (b'D', b'U', ConflictKind::DeletedByUs, "deleted by us"),
            (b'A', b'A', ConflictKind::BothAdded, "both added"),
            (b'U', b'U', ConflictKind::BothModified, "both modified"),
        ];
        for (x, y, want_kind, want_label) in cases {
            let kind = ConflictKind::from_xy(x, y).expect("legal XY pair");
            assert_eq!(kind, want_kind, "xy=({}, {})", x as char, y as char);
            assert_eq!(kind.label(), want_label);
        }
    }

    #[test]
    fn illegal_pairs_return_none() {
        // Common non-conflict pairs that the parser must not silently coerce.
        for (x, y) in [(b'.', b'.'), (b'M', b'.'), (b'?', b'?'), (b'X', b'Z')] {
            assert!(
                ConflictKind::from_xy(x, y).is_none(),
                "unexpected hit on xy=({}, {})",
                x as char,
                y as char
            );
        }
    }

    #[test]
    fn labels_are_lowercase_phrase() {
        // The UI puts these under a status badge — no leading caps, no
        // trailing punctuation, single space separator.
        for variant in [
            ConflictKind::BothDeleted,
            ConflictKind::AddedByUs,
            ConflictKind::DeletedByThem,
            ConflictKind::AddedByThem,
            ConflictKind::DeletedByUs,
            ConflictKind::BothAdded,
            ConflictKind::BothModified,
        ] {
            let lbl = variant.label();
            assert!(
                lbl.chars().next().is_some_and(|c| c.is_ascii_lowercase()),
                "label must start lowercase: {lbl:?}"
            );
            assert!(!lbl.ends_with('.'), "no trailing period: {lbl:?}");
            assert!(lbl.contains(' '), "label must be a phrase: {lbl:?}");
        }
    }
}
