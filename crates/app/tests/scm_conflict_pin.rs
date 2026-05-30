//! Phase 01 unit tests — row action wiring + conflict pin ordering.
//!
//! Covers the NEW behavior added on top of `partition_files`:
//!   - conflict rows are pinned to the top of the Unstaged section
//!   - mixed conflicts + regular changes keep their relative order within
//!     each group (stable sort)
//!   - the `is_conflict` predicate covers both index and worktree Unmerged
//!
//! Hover-action visibility itself is a GPUI render concern; the wiring is
//! covered by the smoke test that exists in `git_panel_smoke.rs`.

use oximux_app::shell::git_panel::changed_files::partition_files;
use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use std::path::PathBuf;

fn fs(path: &str, index: IndexStatus, worktree: WorktreeStatus) -> FileStatus {
    FileStatus::with_status(PathBuf::from(path), index, worktree)
}

#[test]
fn conflict_rows_pin_to_top_of_unstaged() {
    // Input order: regular, conflict, regular. After partition the
    // conflict must lead so an unresolved merge can't hide below ordinary
    // modifications.
    let files = vec![
        fs("a.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
        fs("b.txt", IndexStatus::Unmerged, WorktreeStatus::Unmerged),
        fs("c.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
    ];
    let s = partition_files(&files);
    assert_eq!(s.unstaged.len(), 3);
    assert_eq!(
        s.unstaged[0].path,
        PathBuf::from("b.txt"),
        "conflict row must lead"
    );
}

#[test]
fn multiple_conflicts_keep_relative_order() {
    // Stable sort: among the conflict group, order matches input.
    let files = vec![
        fs("z.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
        fs(
            "conflict-2.txt",
            IndexStatus::Unmerged,
            WorktreeStatus::Unmerged,
        ),
        fs(
            "conflict-1.txt",
            IndexStatus::Unmerged,
            WorktreeStatus::Unmerged,
        ),
        fs("a.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
    ];
    let s = partition_files(&files);
    assert_eq!(s.unstaged[0].path, PathBuf::from("conflict-2.txt"));
    assert_eq!(s.unstaged[1].path, PathBuf::from("conflict-1.txt"));
    assert_eq!(s.unstaged[2].path, PathBuf::from("z.txt"));
    assert_eq!(s.unstaged[3].path, PathBuf::from("a.txt"));
}

#[test]
fn no_conflicts_preserves_input_order() {
    // Regression guard: the sort_by_key on conflict-flag must be stable so
    // a tree with zero conflicts retains insertion order — flicker-free
    // re-renders depend on it.
    let files = vec![
        fs("c.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
        fs("a.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
        fs("b.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
    ];
    let s = partition_files(&files);
    assert_eq!(s.unstaged[0].path, PathBuf::from("c.txt"));
    assert_eq!(s.unstaged[1].path, PathBuf::from("a.txt"));
    assert_eq!(s.unstaged[2].path, PathBuf::from("b.txt"));
}

#[test]
fn index_unmerged_alone_still_counts_as_conflict() {
    // Either side carrying `Unmerged` flags the row as conflict.
    let files = vec![
        fs("a.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
        fs("u.txt", IndexStatus::Unmerged, WorktreeStatus::Modified),
    ];
    let s = partition_files(&files);
    assert_eq!(s.unstaged[0].path, PathBuf::from("u.txt"));
}

#[test]
fn worktree_unmerged_alone_still_counts_as_conflict() {
    let files = vec![
        fs("a.txt", IndexStatus::Unmodified, WorktreeStatus::Modified),
        fs("u.txt", IndexStatus::Modified, WorktreeStatus::Unmerged),
    ];
    let s = partition_files(&files);
    assert_eq!(s.unstaged[0].path, PathBuf::from("u.txt"));
}
