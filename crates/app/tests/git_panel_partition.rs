//! Pure-unit tests for `partition_files` — no GPUI, no tokio. Fast.
//!
//! Covers:
//!   - clean tree → all empty
//!   - staged only / unstaged only / untracked only
//!   - partial-stage (Modified/Modified) appears in BOTH Staged and Unstaged
//!   - unmerged surfaces in Unstaged ONLY (must `git add` to resolve)
//!   - ignored entries are filtered out of every section
//!   - section vec lengths match what render reads via `.len()`

use oximux_app::shell::git_panel::changed_files::partition_files;
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

#[test]
fn empty_input_gives_all_empty_sections() {
    let s = partition_files(&[]);
    assert!(s.is_empty());
    assert_eq!(s.staged.len(), 0);
    assert_eq!(s.unstaged.len(), 0);
    assert_eq!(s.untracked.len(), 0);
}

#[test]
fn untracked_only_goes_to_untracked_section() {
    let files = vec![fs(
        "a.txt",
        IndexStatus::Untracked,
        WorktreeStatus::Untracked,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 0);
    assert_eq!(s.unstaged.len(), 0);
    assert_eq!(s.untracked.len(), 1);
    assert_eq!(s.untracked[0].path, PathBuf::from("a.txt"));
}

#[test]
fn staged_only_modified_in_index_unmodified_in_worktree() {
    let files = vec![fs(
        "a.txt",
        IndexStatus::Modified,
        WorktreeStatus::Unmodified,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 1);
    assert_eq!(s.unstaged.len(), 0);
    assert_eq!(s.untracked.len(), 0);
}

#[test]
fn unstaged_only_unmodified_in_index_modified_in_worktree() {
    let files = vec![fs(
        "a.txt",
        IndexStatus::Unmodified,
        WorktreeStatus::Modified,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 0);
    assert_eq!(s.unstaged.len(), 1);
    assert_eq!(s.untracked.len(), 0);
}

#[test]
fn partial_stage_modified_modified_appears_in_both_staged_and_unstaged() {
    // Classic VSCode / Magit convention: a partially-staged file shows up
    // under BOTH section headers because both sides have real diff content.
    let files = vec![fs("a.txt", IndexStatus::Modified, WorktreeStatus::Modified)];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 1);
    assert_eq!(s.unstaged.len(), 1);
    assert_eq!(s.untracked.len(), 0);
    assert_eq!(s.staged[0].path, s.unstaged[0].path);
}

#[test]
fn partial_stage_added_then_modified_also_appears_in_both() {
    let files = vec![fs("a.txt", IndexStatus::Added, WorktreeStatus::Modified)];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 1);
    assert_eq!(s.unstaged.len(), 1);
}

#[test]
fn unmerged_surfaces_in_unstaged_only() {
    // Conflicts must require an explicit `git add` to mark resolved.
    // Showing them as already-staged would mislead the user into
    // committing unresolved markers.
    let files = vec![fs(
        "conflict.txt",
        IndexStatus::Unmerged,
        WorktreeStatus::Unmerged,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 0);
    assert_eq!(s.unstaged.len(), 1);
    assert_eq!(s.untracked.len(), 0);
}

#[test]
fn ignored_index_filters_from_all_sections() {
    let files = vec![fs(
        "ignored.log",
        IndexStatus::Ignored,
        WorktreeStatus::Unmodified,
    )];
    let s = partition_files(&files);
    assert!(s.is_empty());
}

#[test]
fn ignored_worktree_filters_from_all_sections() {
    let files = vec![fs(
        "ignored.log",
        IndexStatus::Unmodified,
        WorktreeStatus::Ignored,
    )];
    let s = partition_files(&files);
    assert!(s.is_empty());
}

#[test]
fn multi_file_mix_is_routed_correctly_and_counts_match() {
    let files = vec![
        fs("a.txt", IndexStatus::Added, WorktreeStatus::Unmodified), // staged
        fs("b.txt", IndexStatus::Unmodified, WorktreeStatus::Modified), // unstaged
        fs("c.txt", IndexStatus::Untracked, WorktreeStatus::Untracked), // untracked
        fs("d.txt", IndexStatus::Modified, WorktreeStatus::Modified), // both
        fs("e.log", IndexStatus::Ignored, WorktreeStatus::Unmodified), // filtered
    ];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 2, "a + d");
    assert_eq!(s.unstaged.len(), 2, "b + d");
    assert_eq!(s.untracked.len(), 1, "c");
    let staged_paths: Vec<_> = s.staged.iter().map(|f| f.path.as_path()).collect();
    assert!(staged_paths.contains(&std::path::Path::new("a.txt")));
    assert!(staged_paths.contains(&std::path::Path::new("d.txt")));
    let unstaged_paths: Vec<_> = s.unstaged.iter().map(|f| f.path.as_path()).collect();
    assert!(unstaged_paths.contains(&std::path::Path::new("b.txt")));
    assert!(unstaged_paths.contains(&std::path::Path::new("d.txt")));
}

#[test]
fn deleted_in_worktree_only_is_unstaged() {
    let files = vec![fs(
        "gone.txt",
        IndexStatus::Unmodified,
        WorktreeStatus::Deleted,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 0);
    assert_eq!(s.unstaged.len(), 1);
}

#[test]
fn deleted_in_index_only_is_staged() {
    let files = vec![fs(
        "gone.txt",
        IndexStatus::Deleted,
        WorktreeStatus::Unmodified,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 1);
    assert_eq!(s.unstaged.len(), 0);
}

#[test]
fn copied_index_is_staged() {
    let files = vec![fs(
        "copy.txt",
        IndexStatus::Copied,
        WorktreeStatus::Unmodified,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 1);
    assert_eq!(s.unstaged.len(), 0);
}

#[test]
fn renamed_index_is_staged() {
    let files = vec![fs(
        "new.txt",
        IndexStatus::Renamed,
        WorktreeStatus::Unmodified,
    )];
    let s = partition_files(&files);
    assert_eq!(s.staged.len(), 1);
}

#[test]
fn is_empty_predicate_is_consistent_with_section_lens() {
    let empty = partition_files(&[]);
    assert!(empty.is_empty());

    let files = [fs("a", IndexStatus::Added, WorktreeStatus::Unmodified)];
    let non_empty = partition_files(&files);
    assert!(!non_empty.is_empty());
}
