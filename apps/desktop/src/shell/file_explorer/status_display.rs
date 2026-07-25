//! Git status badge logic for the file explorer.
//!
//! Pure module — no GPUI imports. Derives `BadgeStatus` from raw `FileStatus`
//! records, computes priority-based dominant status for folder propagation.

use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use std::collections::HashMap;
use std::path::PathBuf;

/// Display-level status for a file or folder badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Copied,
    Ignored,
}

/// Single-letter label for each badge status.
/// `Ignored` is intentionally absent — ignored entries are shown italic+dim
/// with no badge label (M2).
pub const STATUS_LABELS: &[(BadgeStatus, &str)] = &[
    (BadgeStatus::Modified, "M"),
    (BadgeStatus::Added, "A"),
    (BadgeStatus::Deleted, "D"),
    (BadgeStatus::Renamed, "R"),
    (BadgeStatus::Untracked, "U"),
    (BadgeStatus::Copied, "C"),
];

/// Priority ladder (higher = wins in `dominant`).
/// Ties resolve by taking the first encountered after sorting.
const STATUS_PRIORITY: &[(BadgeStatus, u8)] = &[
    (BadgeStatus::Deleted, 5),
    (BadgeStatus::Modified, 4),
    (BadgeStatus::Added, 3),
    (BadgeStatus::Untracked, 3),
    (BadgeStatus::Renamed, 2),
    (BadgeStatus::Copied, 1),
    (BadgeStatus::Ignored, 0),
];

fn priority(s: BadgeStatus) -> u8 {
    STATUS_PRIORITY
        .iter()
        .find(|(b, _)| *b == s)
        .map(|(_, p)| *p)
        .unwrap_or(0)
}

/// Return the single highest-priority status from an iterator.
/// Ties resolve by ladder order (first defined in `STATUS_PRIORITY` wins).
pub fn dominant(statuses: impl IntoIterator<Item = BadgeStatus>) -> Option<BadgeStatus> {
    statuses
        .into_iter()
        .reduce(|acc, s| if priority(s) > priority(acc) { s } else { acc })
}

/// `true` for any status that should propagate upward to folder badges.
/// `Deleted` and `Ignored` do not propagate — deleted files should not make
/// a parent appear deleted, and ignored files should stay invisible (M1).
pub fn should_propagate(s: BadgeStatus) -> bool {
    !matches!(s, BadgeStatus::Deleted | BadgeStatus::Ignored)
}

/// Map each `FileStatus` to a `BadgeStatus` using worktree-first priority.
///
/// Mapping rules:
/// - Worktree column takes precedence over index.
/// - `Unmerged` worktree = treat as `Modified` (conflict badge).
pub fn badge_for_file(f: &FileStatus) -> Option<BadgeStatus> {
    match f.worktree {
        WorktreeStatus::Deleted => return Some(BadgeStatus::Deleted),
        WorktreeStatus::Modified => return Some(BadgeStatus::Modified),
        WorktreeStatus::Renamed => return Some(BadgeStatus::Renamed),
        WorktreeStatus::Untracked => return Some(BadgeStatus::Untracked),
        WorktreeStatus::Unmerged => return Some(BadgeStatus::Modified),
        WorktreeStatus::Ignored => return Some(BadgeStatus::Ignored),
        WorktreeStatus::Unmodified => {} // fall through to index
    }
    // Worktree is clean — check index.
    match f.index {
        IndexStatus::Added => Some(BadgeStatus::Added),
        IndexStatus::Deleted => Some(BadgeStatus::Deleted),
        IndexStatus::Modified => Some(BadgeStatus::Modified),
        IndexStatus::Renamed => Some(BadgeStatus::Renamed),
        IndexStatus::Copied => Some(BadgeStatus::Copied),
        _ => None,
    }
}

/// Build a path → BadgeStatus map for individual files.
pub fn build_status_map(files: &[FileStatus]) -> HashMap<PathBuf, BadgeStatus> {
    files
        .iter()
        .filter_map(|f| badge_for_file(f).map(|s| (f.path.clone(), s)))
        .collect()
}

/// Build a path → BadgeStatus map for **folders**, propagating non-Deleted
/// statuses upward through each ancestor directory segment.
pub fn build_folder_status_map(files: &[FileStatus]) -> HashMap<PathBuf, BadgeStatus> {
    let mut folder_map: HashMap<PathBuf, Vec<BadgeStatus>> = HashMap::new();

    for f in files {
        let Some(badge) = badge_for_file(f) else {
            continue;
        };
        if !should_propagate(badge) {
            continue;
        }
        // Walk every ancestor of the file path and collect the badge.
        let mut current = f.path.clone();
        while let Some(parent) = current.parent() {
            if parent == std::path::Path::new("") || parent == std::path::Path::new("/") {
                break;
            }
            folder_map
                .entry(parent.to_path_buf())
                .or_default()
                .push(badge);
            current = parent.to_path_buf();
        }
    }

    folder_map
        .into_iter()
        .filter_map(|(path, statuses)| dominant(statuses).map(|s| (path, s)))
        .collect()
}

/// Look up the label string for a `BadgeStatus`.
pub fn label_for(s: BadgeStatus) -> &'static str {
    STATUS_LABELS
        .iter()
        .find(|(b, _)| *b == s)
        .map(|(_, l)| *l)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};

    fn fs(path: &str, index: IndexStatus, worktree: WorktreeStatus) -> FileStatus {
        FileStatus::with_status(PathBuf::from(path), index, worktree)
    }

    // ── dominant ────────────────────────────────────────────────────────────

    #[test]
    fn dominant_single() {
        assert_eq!(
            dominant([BadgeStatus::Modified]),
            Some(BadgeStatus::Modified)
        );
    }

    #[test]
    fn dominant_empty() {
        assert_eq!(dominant([]), None);
    }

    #[test]
    fn dominant_higher_priority_wins() {
        assert_eq!(
            dominant([BadgeStatus::Added, BadgeStatus::Deleted]),
            Some(BadgeStatus::Deleted)
        );
    }

    #[test]
    fn dominant_tie_first_wins() {
        // Added and Untracked both have priority 3 — first encountered kept.
        let result = dominant([BadgeStatus::Added, BadgeStatus::Untracked]);
        assert!(matches!(result, Some(BadgeStatus::Added)));
    }

    // ── should_propagate ────────────────────────────────────────────────────

    #[test]
    fn should_propagate_deleted_is_false() {
        assert!(!should_propagate(BadgeStatus::Deleted));
    }

    #[test]
    fn should_propagate_ignored_is_false() {
        assert!(!should_propagate(BadgeStatus::Ignored));
    }

    #[test]
    fn should_propagate_non_deleted_non_ignored_are_true() {
        assert!(should_propagate(BadgeStatus::Modified));
        assert!(should_propagate(BadgeStatus::Added));
        assert!(should_propagate(BadgeStatus::Renamed));
        assert!(should_propagate(BadgeStatus::Untracked));
        assert!(should_propagate(BadgeStatus::Copied));
    }

    // ── build_status_map ────────────────────────────────────────────────────

    #[test]
    fn build_status_map_worktree_deleted() {
        let files = [fs("a.rs", IndexStatus::Unmodified, WorktreeStatus::Deleted)];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("a.rs")], BadgeStatus::Deleted);
    }

    #[test]
    fn build_status_map_worktree_modified() {
        let files = [fs(
            "a.rs",
            IndexStatus::Unmodified,
            WorktreeStatus::Modified,
        )];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("a.rs")], BadgeStatus::Modified);
    }

    #[test]
    fn build_status_map_worktree_untracked() {
        let files = [fs(
            "new.rs",
            IndexStatus::Untracked,
            WorktreeStatus::Untracked,
        )];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("new.rs")], BadgeStatus::Untracked);
    }

    #[test]
    fn build_status_map_worktree_unmerged_becomes_modified() {
        let files = [fs(
            "conflict.rs",
            IndexStatus::Unmerged,
            WorktreeStatus::Unmerged,
        )];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("conflict.rs")], BadgeStatus::Modified);
    }

    #[test]
    fn build_status_map_index_added_when_worktree_clean() {
        let files = [fs(
            "staged.rs",
            IndexStatus::Added,
            WorktreeStatus::Unmodified,
        )];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("staged.rs")], BadgeStatus::Added);
    }

    #[test]
    fn build_status_map_index_renamed() {
        let files = [fs(
            "renamed.rs",
            IndexStatus::Renamed,
            WorktreeStatus::Unmodified,
        )];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("renamed.rs")], BadgeStatus::Renamed);
    }

    #[test]
    fn build_status_map_index_copied() {
        let files = [fs(
            "copy.rs",
            IndexStatus::Copied,
            WorktreeStatus::Unmodified,
        )];
        let m = build_status_map(&files);
        assert_eq!(m[&PathBuf::from("copy.rs")], BadgeStatus::Copied);
    }

    #[test]
    fn build_status_map_clean_file_absent() {
        let files = [fs(
            "clean.rs",
            IndexStatus::Unmodified,
            WorktreeStatus::Unmodified,
        )];
        let m = build_status_map(&files);
        assert!(!m.contains_key(&PathBuf::from("clean.rs")));
    }

    // ── build_folder_status_map ─────────────────────────────────────────────

    #[test]
    fn build_folder_status_map_propagates_to_ancestors() {
        let files = [fs(
            "crates/app/src/main.rs",
            IndexStatus::Unmodified,
            WorktreeStatus::Modified,
        )];
        let m = build_folder_status_map(&files);
        assert_eq!(m[&PathBuf::from("crates/app/src")], BadgeStatus::Modified);
        assert_eq!(m[&PathBuf::from("crates/app")], BadgeStatus::Modified);
        assert_eq!(m[&PathBuf::from("crates")], BadgeStatus::Modified);
    }

    #[test]
    fn build_folder_status_map_deleted_not_propagated() {
        let files = [fs(
            "crates/foo.rs",
            IndexStatus::Unmodified,
            WorktreeStatus::Deleted,
        )];
        let m = build_folder_status_map(&files);
        assert!(!m.contains_key(&PathBuf::from("crates")));
    }

    #[test]
    fn build_folder_status_map_multi_descendant_dominant() {
        // One deleted child (won't propagate) + one modified child → folder gets Modified.
        let files = [
            fs(
                "src/deleted.rs",
                IndexStatus::Unmodified,
                WorktreeStatus::Deleted,
            ),
            fs(
                "src/changed.rs",
                IndexStatus::Unmodified,
                WorktreeStatus::Modified,
            ),
        ];
        let m = build_folder_status_map(&files);
        assert_eq!(m[&PathBuf::from("src")], BadgeStatus::Modified);
    }

    #[test]
    fn build_folder_status_map_deep_nesting_tie_break() {
        // Added (prio 3) and Copied (prio 1) under same dir → Added wins.
        let files = [
            fs(
                "a/b/added.rs",
                IndexStatus::Added,
                WorktreeStatus::Unmodified,
            ),
            fs(
                "a/b/copied.rs",
                IndexStatus::Copied,
                WorktreeStatus::Unmodified,
            ),
        ];
        let m = build_folder_status_map(&files);
        assert_eq!(m[&PathBuf::from("a/b")], BadgeStatus::Added);
        assert_eq!(m[&PathBuf::from("a")], BadgeStatus::Added);
    }
}
