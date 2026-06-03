//! Integration tests for `Repository::diff_combined` — the merged
//! multi-file working-tree diff with parallel group tags.
//!
//! Each test builds a real repo under a `TempDir` with a known mix of
//! unstaged / staged / untracked (and dual-staged) files, then asserts the
//! assembled order and `groups` tagging. The `AllChanges` scope is the
//! richest: a file modified in BOTH index and worktree must appear twice
//! (once `Staged`, once `Unstaged`) — the show-both contract.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::{CombinedDiffScope, FileGroup};
use oximux_git::Repository;
use std::path::PathBuf;
use tempfile::TempDir;

async fn repo_with_mixed_changes() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    init_repo(p);
    // Base commit: a.txt + b.txt tracked.
    write(&p.join("a.txt"), "A0\n");
    write(&p.join("b.txt"), "B0\n");
    run_git(p, &["add", "a.txt", "b.txt"]);
    run_git(p, &["commit", "-m", "base"]);

    // a.txt: stage A0→A1, then modify worktree A1→A2 (appears in BOTH sides).
    write(&p.join("a.txt"), "A1\n");
    run_git(p, &["add", "a.txt"]);
    write(&p.join("a.txt"), "A2\n");

    // b.txt: stage B0→B1 only (staged, no unstaged remainder).
    write(&p.join("b.txt"), "B1\n");
    run_git(p, &["add", "b.txt"]);

    // c.txt: untracked.
    write(&p.join("c.txt"), "C0\n");

    let repo = Repository::open(p).await.unwrap();
    (dir, repo)
}

#[tokio::test]
async fn all_changes_orders_unstaged_then_staged_then_untracked() {
    let (_dir, repo) = repo_with_mixed_changes().await;
    let combined = repo
        .diff_combined(CombinedDiffScope::AllChanges)
        .await
        .unwrap();

    // Invariant: parallel arrays.
    assert_eq!(combined.diffs.len(), combined.groups.len());

    // a(Unstaged), a(Staged), b(Staged), c(Untracked).
    assert_eq!(
        combined.groups,
        vec![
            FileGroup::Unstaged,
            FileGroup::Staged,
            FileGroup::Staged,
            FileGroup::Untracked,
        ],
        "order must be all-unstaged, then all-staged, then untracked"
    );

    // The dual-staged file a.txt shows on BOTH the unstaged and staged side.
    assert_eq!(combined.diffs[0].path, PathBuf::from("a.txt"));
    assert_eq!(combined.diffs[1].path, PathBuf::from("a.txt"));
    assert_eq!(combined.diffs[2].path, PathBuf::from("b.txt"));
    assert_eq!(combined.diffs[3].path, PathBuf::from("c.txt"));
}

#[tokio::test]
async fn staged_scope_returns_only_staged_files() {
    let (_dir, repo) = repo_with_mixed_changes().await;
    let combined = repo.diff_combined(CombinedDiffScope::Staged).await.unwrap();

    // Both a.txt (A0→A1) and b.txt (B0→B1) are staged.
    assert_eq!(combined.diffs.len(), 2);
    assert!(combined.groups.iter().all(|g| *g == FileGroup::Staged));
    let paths: Vec<_> = combined.diffs.iter().map(|d| d.path.clone()).collect();
    assert!(paths.contains(&PathBuf::from("a.txt")));
    assert!(paths.contains(&PathBuf::from("b.txt")));
}

#[tokio::test]
async fn untracked_scope_returns_only_untracked_files() {
    let (_dir, repo) = repo_with_mixed_changes().await;
    let combined = repo
        .diff_combined(CombinedDiffScope::Untracked)
        .await
        .unwrap();

    assert_eq!(combined.diffs.len(), 1);
    assert_eq!(combined.diffs[0].path, PathBuf::from("c.txt"));
    assert_eq!(combined.groups, vec![FileGroup::Untracked]);
}

#[tokio::test]
async fn branch_scope_returns_committed_files_read_only() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    init_repo(p);
    // Base commit, then a second commit so HEAD~1..HEAD has content.
    write(&p.join("a.txt"), "A0\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "base"]);
    write(&p.join("a.txt"), "A1\n");
    write(&p.join("d.txt"), "D0\n");
    run_git(p, &["add", "a.txt", "d.txt"]);
    run_git(p, &["commit", "-m", "second"]);

    let repo = Repository::open(p).await.unwrap();
    let combined = repo
        .diff_combined(CombinedDiffScope::Branch {
            base: "HEAD~1".into(),
            head: "HEAD".into(),
        })
        .await
        .unwrap();

    // Both files the second commit touched, all tagged read-only Committed.
    assert_eq!(combined.diffs.len(), combined.groups.len());
    assert!(!combined.diffs.is_empty(), "range must surface changed files");
    assert!(combined.groups.iter().all(|g| *g == FileGroup::Committed));
    let paths: Vec<_> = combined.diffs.iter().map(|d| d.path.clone()).collect();
    assert!(paths.contains(&PathBuf::from("a.txt")));
    assert!(paths.contains(&PathBuf::from("d.txt")));
}
