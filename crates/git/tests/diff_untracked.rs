//! Unit tests for `Repository::diff_for_untracked` — synthesizes an
//! "all-additions" diff for files that git's normal `diff` ignores.
//!
//! Each test creates a fresh git repo under a `TempDir`, writes the
//! requested file (or omits it), then calls `diff_for_untracked` and
//! asserts the shape of the returned `FileDiff`.

use oximux_core::{DiffLineKind, DiffStatus};
use oximux_git::Repository;
use std::path::Path;
use tempfile::TempDir;

async fn fresh_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let repo = Repository::open(dir.path()).await.unwrap();
    (dir, repo)
}

#[tokio::test]
async fn untracked_text_file_is_all_additions() {
    let (dir, repo) = fresh_repo().await;
    std::fs::write(dir.path().join("hello.rs"), "fn main() {}\n").unwrap();

    let diffs = repo
        .diff_for_untracked(Path::new("hello.rs"))
        .await
        .unwrap();
    assert_eq!(diffs.len(), 1);
    let d = &diffs[0];
    assert_eq!(d.status, DiffStatus::Added);
    assert_eq!(d.hunks.len(), 1);
    let h = &d.hunks[0];
    assert_eq!(h.old_start, 0);
    assert_eq!(h.old_lines, 0);
    assert_eq!(h.new_start, 1);
    assert_eq!(h.new_lines, 1);
    assert_eq!(h.lines.len(), 1);
    assert_eq!(h.lines[0].kind, DiffLineKind::Added);
    assert_eq!(h.lines[0].content, "fn main() {}");
}

#[tokio::test]
async fn untracked_multiline_no_trailing_newline_emits_eof_hint() {
    let (dir, repo) = fresh_repo().await;
    std::fs::write(dir.path().join("no_eol.txt"), "first\nsecond").unwrap();

    let diffs = repo
        .diff_for_untracked(Path::new("no_eol.txt"))
        .await
        .unwrap();
    let h = &diffs[0].hunks[0];
    // 2 content lines + 1 EOF hint
    assert_eq!(h.lines.len(), 3);
    assert_eq!(h.lines[0].content, "first");
    assert_eq!(h.lines[1].content, "second");
    assert_eq!(h.lines[2].kind, DiffLineKind::NoNewlineHint);
}

#[tokio::test]
async fn untracked_empty_file_returns_added_status_with_no_hunks() {
    let (dir, repo) = fresh_repo().await;
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();

    let diffs = repo
        .diff_for_untracked(Path::new("empty.txt"))
        .await
        .unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].status, DiffStatus::Added);
    // Empty file → no hunk; renderer falls back to its empty state.
    assert!(diffs[0].hunks.is_empty());
}

#[tokio::test]
async fn untracked_binary_file_is_classified_binary() {
    let (dir, repo) = fresh_repo().await;
    // 16 bytes of NULs trigger the binary heuristic on the first byte.
    std::fs::write(dir.path().join("blob.bin"), [0u8; 16]).unwrap();

    let diffs = repo
        .diff_for_untracked(Path::new("blob.bin"))
        .await
        .unwrap();
    assert_eq!(diffs[0].status, DiffStatus::Binary);
    assert!(diffs[0].hunks.is_empty());
}

#[tokio::test]
async fn untracked_missing_path_errors() {
    let (_dir, repo) = fresh_repo().await;
    let r = repo.diff_for_untracked(Path::new("nope.txt")).await;
    assert!(r.is_err(), "missing file should error");
}

#[tokio::test]
async fn untracked_absolute_path_resolves_inside_workdir() {
    let (dir, repo) = fresh_repo().await;
    let abs = dir.path().join("ok.txt");
    std::fs::write(&abs, "x\n").unwrap();

    let diffs = repo.diff_for_untracked(&abs).await.unwrap();
    assert_eq!(diffs[0].status, DiffStatus::Added);
    assert_eq!(diffs[0].hunks[0].lines.len(), 1);
}
