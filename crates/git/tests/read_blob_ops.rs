//! Tests for `Repository::read_blob_at` — raw blob bytes by `<rev>:<path>`,
//! used by the diff view to preview the "before" side of an image change.
//!
//! Each test drives real `git` against a tempdir repo: it must return the
//! committed bytes (not the worktree copy), `None` for a missing object, and
//! must treat glob metacharacters in the filename literally.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::Repository;
use std::path::Path;
use tempfile::TempDir;

#[tokio::test]
async fn read_blob_at_returns_committed_bytes_not_worktree() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    init_repo(p);
    let img = p.join("logo.png");
    std::fs::write(&img, b"COMMITTED-BYTES").unwrap();
    run_git(p, &["add", "logo.png"]);
    run_git(p, &["commit", "-m", "add logo"]);
    // Edit the worktree copy WITHOUT committing — the blob read must still
    // return the HEAD bytes, proving it reads the object store, not disk.
    std::fs::write(&img, b"WORKTREE-BYTES").unwrap();

    let repo = Repository::open(p).await.unwrap();
    let head = repo
        .read_blob_at("HEAD", Path::new("logo.png"))
        .await
        .unwrap();
    assert_eq!(head.as_deref(), Some(b"COMMITTED-BYTES".as_slice()));
}

#[tokio::test]
async fn read_blob_at_missing_object_is_none() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    init_repo(p);
    write(&p.join("a.txt"), "x\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "seed"]);

    let repo = Repository::open(p).await.unwrap();
    let missing = repo
        .read_blob_at("HEAD", Path::new("never-existed.png"))
        .await
        .unwrap();
    assert!(missing.is_none(), "a path absent at HEAD reads as None");
}

#[tokio::test]
async fn read_blob_at_treats_glob_chars_literally() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    init_repo(p);
    // A real filename containing glob metacharacters next to a decoy the glob
    // would otherwise match — the literal pathname must win.
    std::fs::write(p.join("cache[1].png"), b"REAL").unwrap();
    std::fs::write(p.join("cache1.png"), b"DECOY").unwrap();
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);

    let repo = Repository::open(p).await.unwrap();
    let got = repo
        .read_blob_at("HEAD", Path::new("cache[1].png"))
        .await
        .unwrap();
    assert_eq!(got.as_deref(), Some(b"REAL".as_slice()));
}
