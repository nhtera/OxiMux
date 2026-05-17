//! Integration tests for stash operations on `Repository`: `is_dirty`,
//! `stash_push`, `stash_list`, `stash_apply`, `stash_pop`, `stash_drop`.
//! Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::{GitError, Repository};

#[tokio::test]
async fn is_dirty_clean_repo_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    assert!(!repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn is_dirty_with_unstaged_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    assert!(repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn is_dirty_with_staged_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    let repo = Repository::open(p).await.unwrap();
    assert!(repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn is_dirty_ignores_untracked_files() {
    // `is_dirty` uses --untracked-files=no so an untracked-only worktree is
    // considered clean. This mirrors `git stash push` default behavior.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("untracked.txt"), "new\n");
    let repo = Repository::open(p).await.unwrap();
    assert!(!repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn stash_push_captures_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false).await.unwrap();
    assert_eq!(r.index, 0);
    // Working tree is clean post-push and the file is back to v1.
    assert!(!repo.is_dirty().await.unwrap());
    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v1\n");
    let list = repo.stash_list().await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn stash_push_with_message() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("my work in progress"), false)
        .await
        .unwrap();

    let list = repo.stash_list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].branch, "main");
    assert_eq!(list[0].message, "my work in progress");
}

#[tokio::test]
async fn stash_push_include_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("untracked.txt"), "secret\n");
    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("with untracked"), true).await.unwrap();

    // Untracked file is gone (stashed), and stash list has the entry.
    assert!(!p.join("untracked.txt").exists());
    let list = repo.stash_list().await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn stash_push_clean_tree_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.stash_push(None, false).await.unwrap_err();
    assert!(
        matches!(err, GitError::InvalidInput { .. }),
        "expected InvalidInput on clean tree, got {err:?}"
    );
    assert_eq!(repo.stash_list().await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_push_message_special_chars() {
    // `!` `"` `'` are arg-passed (no shell), so they round-trip literally.
    // Newline in a stash message has no good test — git collapses it.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let msg = r#"fix!: "shouldn't" break 'parser'"#;
    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some(msg), false).await.unwrap();
    let list = repo.stash_list().await.unwrap();
    assert_eq!(list[0].message, msg);
}

#[tokio::test]
async fn stash_list_ordering_most_recent_first() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    // First stash.
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("older"), false).await.unwrap();
    // Second stash on top.
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("newer"), false).await.unwrap();

    let list = repo.stash_list().await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].stash_ref.index, 0);
    assert_eq!(list[0].message, "newer");
    assert_eq!(list[1].stash_ref.index, 1);
    assert_eq!(list[1].message, "older");
}

#[tokio::test]
async fn stash_apply_leaves_entry_on_stack() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false).await.unwrap();
    repo.stash_apply(&r).await.unwrap();

    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v2\n", "apply should restore worktree");
    assert_eq!(
        repo.stash_list().await.unwrap().len(),
        1,
        "apply leaves entry on stack"
    );
}

#[tokio::test]
async fn stash_pop_removes_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false).await.unwrap();
    repo.stash_pop(&r).await.unwrap();

    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v2\n");
    assert_eq!(repo.stash_list().await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_drop_removes_without_applying() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false).await.unwrap();
    // stash_push restored worktree to clean. drop must NOT bring v2 back.
    repo.stash_drop(&r).await.unwrap();
    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v1\n", "drop must not re-apply");
    assert_eq!(repo.stash_list().await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_pop_invalid_ref_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .stash_pop(&oximux_core::StashRef { index: 99 })
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}
