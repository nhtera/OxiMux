//! Integration tests for the file-level stage operations on `Repository`:
//! `stage_paths`, `unstage_paths`, `discard_paths`. Tempdir + real `git`
//! binary on the PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::{IndexStatus, WorktreeStatus};
use oximux_git::Repository;
use std::path::Path;

#[tokio::test]
async fn stage_path_marks_file_as_added_in_index() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("hello.txt"), "hi\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stage_paths(&[Path::new("hello.txt")]).await.unwrap();
    let st = repo.status().await.unwrap();
    assert_eq!(st.files.len(), 1);
    assert_eq!(st.files[0].index, IndexStatus::Added);
    assert_eq!(st.files[0].worktree, WorktreeStatus::Unmodified);
}

#[tokio::test]
async fn unstage_path_reverts_index_to_worktree_only() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    repo.stage_paths(&[Path::new("a.txt")]).await.unwrap();

    let st = repo.status().await.unwrap();
    assert_eq!(st.files[0].index, IndexStatus::Modified);

    repo.unstage_paths(&[Path::new("a.txt")]).await.unwrap();
    let st = repo.status().await.unwrap();
    assert_eq!(st.files[0].index, IndexStatus::Unmodified);
    assert_eq!(st.files[0].worktree, WorktreeStatus::Modified);
}

#[tokio::test]
async fn discard_path_restores_committed_content() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("f.txt"), "original\n");
    run_git(p, &["add", "f.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("f.txt"), "scratch\n");
    let repo = Repository::open(p).await.unwrap();
    repo.discard_paths(&[Path::new("f.txt")]).await.unwrap();

    let on_disk = std::fs::read_to_string(p.join("f.txt")).unwrap();
    assert_eq!(on_disk, "original\n");
}

#[tokio::test]
async fn stage_paths_empty_slice_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    let repo = Repository::open(p).await.unwrap();
    // No paths — git is not invoked at all (avoids `git add --` ambiguity)
    // and the call should succeed silently.
    repo.stage_paths(&[]).await.unwrap();
    repo.unstage_paths(&[]).await.unwrap();
    repo.discard_paths(&[]).await.unwrap();
}

#[tokio::test]
async fn stage_paths_handles_filename_with_spaces() {
    // Verifies the `--` end-of-options sentinel + raw `OsStr` passthrough
    // handle whitespace in paths without needing quoting on the caller side.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("my file.txt"), "hello\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stage_paths(&[Path::new("my file.txt")]).await.unwrap();

    let st = repo.status().await.unwrap();
    let entry = st
        .files
        .iter()
        .find(|f| f.path == Path::new("my file.txt"))
        .expect("'my file.txt' present in status");
    assert_eq!(entry.index, IndexStatus::Added);
}
