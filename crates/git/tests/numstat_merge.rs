//! End-to-end test for `Repository::status` → numstat merge.
//!
//! Spins a real `git init`-backed repo, seeds one commit, then dirties
//! the worktree in several ways. Asserts the merged `FileStatus.line_counts`
//! values match what `git diff --numstat HEAD` actually reports.
//!
//! Why an integration test rather than a unit test: the merge function is
//! one `if let Some(...)` line — almost no behaviour to unit-test in
//! isolation. The risk is in the seams: spawning `git diff`, parsing its
//! `-z` output, indexing by porcelain v2's path-shape. Driving real `git`
//! is the only way to lock those seams down.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::Repository;
use std::path::Path;
use tempfile::tempdir;

#[tokio::test]
async fn line_counts_populated_for_modified_file() {
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    write(&tmp.path().join("src.rs"), "a\nb\nc\n");
    run_git(tmp.path(), &["add", "src.rs"]);
    run_git(tmp.path(), &["commit", "-m", "seed"]);
    // Dirty: replace one line, add two new ones at the end.
    write(&tmp.path().join("src.rs"), "a\nB\nc\nx\ny\n");

    let repo = Repository::open(tmp.path()).await.expect("open repo");
    let state = repo.status().await.expect("status");
    let file = state
        .files
        .iter()
        .find(|f| f.path == Path::new("src.rs"))
        .expect("src.rs in status");
    let (added, removed) = file
        .line_counts
        .expect("line_counts populated by numstat merge");
    // Mutated `b` → `B` counts as +1 / -1. Added `x` + `y` counts as +2.
    // Numstat totals: 3 added, 1 removed.
    assert_eq!(added, 3, "+lines vs HEAD");
    assert_eq!(removed, 1, "-lines vs HEAD");
}

#[tokio::test]
async fn untracked_file_gets_whole_file_line_counts() {
    // Untracked files are not in the regular numstat; the bounded
    // `--no-index` pass counts them against /dev/null instead.
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    write(&tmp.path().join("README"), "seed\n");
    run_git(tmp.path(), &["add", "README"]);
    run_git(tmp.path(), &["commit", "-m", "seed"]);
    write(&tmp.path().join("new.txt"), "fresh\nlines\n");

    let repo = Repository::open(tmp.path()).await.expect("open repo");
    let state = repo.status().await.expect("status");
    let file = state
        .files
        .iter()
        .find(|f| f.path == Path::new("new.txt"))
        .expect("new.txt in status");
    assert_eq!(file.line_counts, Some((2, 0)), "whole-file count");
    assert_eq!(file.staged_line_counts, None, "nothing staged");
}

#[tokio::test]
async fn oversized_untracked_file_is_not_counted() {
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    write(&tmp.path().join("README"), "seed\n");
    run_git(tmp.path(), &["add", "README"]);
    run_git(tmp.path(), &["commit", "-m", "seed"]);
    // Just over the 1 MB cap → skipped (no badge), cached negative.
    let big = "x".repeat(1_000_001 + 1);
    write(&tmp.path().join("big.bin"), &big);

    let repo = Repository::open(tmp.path()).await.expect("open repo");
    let state = repo.status().await.expect("status");
    let file = state
        .files
        .iter()
        .find(|f| f.path == Path::new("big.bin"))
        .expect("big.bin in status");
    assert_eq!(file.line_counts, None, "oversized file skipped");
}

#[tokio::test]
async fn partially_staged_file_has_split_counts() {
    // Stage one edit, then edit again: the Staged row must show the
    // index-vs-HEAD figure and the Changes row the worktree-vs-index one.
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    write(&tmp.path().join("split.rs"), "a\nb\n");
    run_git(tmp.path(), &["add", "split.rs"]);
    run_git(tmp.path(), &["commit", "-m", "seed"]);
    // Staged edit: +2 lines vs HEAD.
    write(&tmp.path().join("split.rs"), "a\nb\nc\nd\n");
    run_git(tmp.path(), &["add", "split.rs"]);
    // Unstaged edit on top: +1 line vs index.
    write(&tmp.path().join("split.rs"), "a\nb\nc\nd\ne\n");

    let repo = Repository::open(tmp.path()).await.expect("open repo");
    let state = repo.status().await.expect("status");
    let file = state
        .files
        .iter()
        .find(|f| f.path == Path::new("split.rs"))
        .expect("split.rs in status");
    assert_eq!(file.staged_line_counts, Some((2, 0)), "index vs HEAD");
    assert_eq!(file.line_counts, Some((1, 0)), "worktree vs index");
}

#[tokio::test]
async fn no_head_yet_returns_state_without_panic() {
    // Pre-first-commit repo: `git diff HEAD` exits non-zero. The status
    // poll must still return cleanly; line_counts just stays `None`.
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    write(&tmp.path().join("greeting.txt"), "hi\n");

    let repo = Repository::open(tmp.path()).await.expect("open repo");
    let state = repo.status().await.expect("status survives no-HEAD repo");
    assert!(!state.files.is_empty(), "untracked surfaces");
    // The regular numstats fail cleanly pre-HEAD; the untracked pass
    // still counts the new file against /dev/null.
    let f = &state.files[0];
    assert_eq!(f.line_counts, Some((1, 0)), "untracked whole-file count");
    assert_eq!(f.staged_line_counts, None);
}

#[tokio::test]
async fn multiple_modified_files_all_get_counts() {
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    write(&tmp.path().join("a.rs"), "1\n2\n3\n");
    write(&tmp.path().join("b.rs"), "x\ny\n");
    run_git(tmp.path(), &["add", "."]);
    run_git(tmp.path(), &["commit", "-m", "seed"]);
    write(&tmp.path().join("a.rs"), "1\n2\n3\n4\n"); // +1
    write(&tmp.path().join("b.rs"), "x\n"); // -1

    let repo = Repository::open(tmp.path()).await.expect("open repo");
    let state = repo.status().await.expect("status");
    let a = state
        .files
        .iter()
        .find(|f| f.path == Path::new("a.rs"))
        .expect("a.rs");
    let b = state
        .files
        .iter()
        .find(|f| f.path == Path::new("b.rs"))
        .expect("b.rs");
    assert_eq!(a.line_counts, Some((1, 0)));
    assert_eq!(b.line_counts, Some((0, 1)));
}
