//! Unit tests for `status_display` — dominant, should_propagate,
//! build_status_map, build_folder_status_map.

use oximux_app::shell::file_explorer::status_display::{
    BadgeStatus, build_folder_status_map, build_status_map, dominant, should_propagate,
};
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use std::path::PathBuf;

fn fs(path: &str, index: IndexStatus, worktree: WorktreeStatus) -> FileStatus {
    FileStatus {
        path: PathBuf::from(path),
        index,
        worktree,
        rename: None,
    }
}

// ── dominant ─────────────────────────────────────────────────────────────────

#[test]
fn dominant_empty_returns_none() {
    assert_eq!(dominant(std::iter::empty()), None);
}

#[test]
fn dominant_single_item() {
    assert_eq!(dominant([BadgeStatus::Added]), Some(BadgeStatus::Added));
}

#[test]
fn dominant_deleted_beats_modified() {
    assert_eq!(
        dominant([BadgeStatus::Modified, BadgeStatus::Deleted]),
        Some(BadgeStatus::Deleted)
    );
}

#[test]
fn dominant_modified_beats_added() {
    assert_eq!(
        dominant([BadgeStatus::Added, BadgeStatus::Modified]),
        Some(BadgeStatus::Modified)
    );
}

#[test]
fn dominant_modified_beats_renamed() {
    assert_eq!(
        dominant([BadgeStatus::Renamed, BadgeStatus::Modified]),
        Some(BadgeStatus::Modified)
    );
}

#[test]
fn dominant_added_and_untracked_both_priority_3_first_wins() {
    // Both have priority 3; iteration order determines winner.
    let result = dominant([BadgeStatus::Added, BadgeStatus::Untracked]);
    assert_eq!(result, Some(BadgeStatus::Added));
}

#[test]
fn dominant_untracked_then_added_first_wins() {
    let result = dominant([BadgeStatus::Untracked, BadgeStatus::Added]);
    assert_eq!(result, Some(BadgeStatus::Untracked));
}

// ── should_propagate ─────────────────────────────────────────────────────────

#[test]
fn should_propagate_deleted_is_false() {
    assert!(!should_propagate(BadgeStatus::Deleted));
}

#[test]
fn should_propagate_modified_is_true() {
    assert!(should_propagate(BadgeStatus::Modified));
}

#[test]
fn should_propagate_added_is_true() {
    assert!(should_propagate(BadgeStatus::Added));
}

#[test]
fn should_propagate_renamed_is_true() {
    assert!(should_propagate(BadgeStatus::Renamed));
}

#[test]
fn should_propagate_untracked_is_true() {
    assert!(should_propagate(BadgeStatus::Untracked));
}

#[test]
fn should_propagate_copied_is_true() {
    assert!(should_propagate(BadgeStatus::Copied));
}

#[test]
fn should_propagate_ignored_is_false() {
    // M1: ignored entries must NOT propagate to folder badges.
    assert!(!should_propagate(BadgeStatus::Ignored));
}

// ── build_status_map ─────────────────────────────────────────────────────────

#[test]
fn status_map_worktree_deleted() {
    let m = build_status_map(&[fs("a.rs", IndexStatus::Unmodified, WorktreeStatus::Deleted)]);
    assert_eq!(m[&PathBuf::from("a.rs")], BadgeStatus::Deleted);
}

#[test]
fn status_map_worktree_modified() {
    let m = build_status_map(&[fs(
        "a.rs",
        IndexStatus::Unmodified,
        WorktreeStatus::Modified,
    )]);
    assert_eq!(m[&PathBuf::from("a.rs")], BadgeStatus::Modified);
}

#[test]
fn status_map_worktree_untracked() {
    let m = build_status_map(&[fs(
        "new.rs",
        IndexStatus::Untracked,
        WorktreeStatus::Untracked,
    )]);
    assert_eq!(m[&PathBuf::from("new.rs")], BadgeStatus::Untracked);
}

#[test]
fn status_map_worktree_unmerged_becomes_modified() {
    let m = build_status_map(&[fs(
        "conflict.rs",
        IndexStatus::Unmerged,
        WorktreeStatus::Unmerged,
    )]);
    assert_eq!(m[&PathBuf::from("conflict.rs")], BadgeStatus::Modified);
}

#[test]
fn status_map_worktree_ignored() {
    let m = build_status_map(&[fs(
        "ignored.log",
        IndexStatus::Ignored,
        WorktreeStatus::Ignored,
    )]);
    assert_eq!(m[&PathBuf::from("ignored.log")], BadgeStatus::Ignored);
}

#[test]
fn status_map_index_added_when_worktree_clean() {
    let m = build_status_map(&[fs(
        "staged.rs",
        IndexStatus::Added,
        WorktreeStatus::Unmodified,
    )]);
    assert_eq!(m[&PathBuf::from("staged.rs")], BadgeStatus::Added);
}

#[test]
fn status_map_index_deleted_when_worktree_clean() {
    let m = build_status_map(&[fs(
        "gone.rs",
        IndexStatus::Deleted,
        WorktreeStatus::Unmodified,
    )]);
    assert_eq!(m[&PathBuf::from("gone.rs")], BadgeStatus::Deleted);
}

#[test]
fn status_map_index_renamed() {
    let m = build_status_map(&[fs(
        "renamed.rs",
        IndexStatus::Renamed,
        WorktreeStatus::Unmodified,
    )]);
    assert_eq!(m[&PathBuf::from("renamed.rs")], BadgeStatus::Renamed);
}

#[test]
fn status_map_index_copied() {
    let m = build_status_map(&[fs(
        "copy.rs",
        IndexStatus::Copied,
        WorktreeStatus::Unmodified,
    )]);
    assert_eq!(m[&PathBuf::from("copy.rs")], BadgeStatus::Copied);
}

#[test]
fn status_map_clean_file_absent() {
    let m = build_status_map(&[fs(
        "clean.rs",
        IndexStatus::Unmodified,
        WorktreeStatus::Unmodified,
    )]);
    assert!(!m.contains_key(&PathBuf::from("clean.rs")));
}

// ── build_folder_status_map ──────────────────────────────────────────────────

#[test]
fn folder_map_propagates_modified_to_ancestors() {
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
fn folder_map_deleted_not_propagated() {
    let files = [fs(
        "crates/foo.rs",
        IndexStatus::Unmodified,
        WorktreeStatus::Deleted,
    )];
    let m = build_folder_status_map(&files);
    assert!(!m.contains_key(&PathBuf::from("crates")));
}

#[test]
fn folder_map_ignored_not_propagated() {
    // M1: an ignored file must not bubble up to its parent folder.
    let files = [fs(
        "crates/ignored.log",
        IndexStatus::Ignored,
        WorktreeStatus::Ignored,
    )];
    let m = build_folder_status_map(&files);
    assert!(
        !m.contains_key(&PathBuf::from("crates")),
        "ignored file should not propagate badge to parent folder"
    );
}

#[test]
fn folder_map_multi_descendant_dominant_wins() {
    // deleted child ignored, modified child wins
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
fn folder_map_deep_nesting_tie_break_added_beats_copied() {
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

#[test]
fn folder_map_empty_files_yields_empty_map() {
    let m = build_folder_status_map(&[]);
    assert!(m.is_empty());
}
