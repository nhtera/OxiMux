//! Integration tests for worktree operations on `Repository`: `add_worktree`,
//! `list_worktrees`, `remove_worktree`. Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::{GitError, Repository};

#[tokio::test]
async fn list_worktrees_main_only() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let ws = repo.list_worktrees().await.unwrap();
    assert_eq!(ws.len(), 1);
    assert!(ws[0].is_main);
    assert_eq!(ws[0].branch.as_deref(), Some("main"));
    assert!(!ws[0].is_locked);
}

#[tokio::test]
async fn add_worktree_creates_dir_and_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    // Worktree directory must be OUTSIDE the repo root (git refuses nested).
    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("feat-x");

    let repo = Repository::open(p).await.unwrap();
    let info = repo.add_worktree(&wt_path, "feat-x").await.unwrap();
    assert!(wt_path.exists());
    assert!(!info.is_main);
    assert_eq!(info.branch.as_deref(), Some("oximux/feat-x"));

    let bs = repo.list_branches().await.unwrap();
    assert!(bs.iter().any(|b| b.name == "oximux/feat-x"));
}

#[tokio::test]
async fn add_worktree_appears_in_list() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "wt-slug").await.unwrap();

    let ws = repo.list_worktrees().await.unwrap();
    assert_eq!(ws.len(), 2);
    let linked = ws.iter().find(|w| !w.is_main).unwrap();
    assert_eq!(linked.branch.as_deref(), Some("oximux/wt-slug"));
}

#[tokio::test]
async fn remove_worktree_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "remove-clean").await.unwrap();
    repo.remove_worktree(&wt_path, false).await.unwrap();

    let ws = repo.list_worktrees().await.unwrap();
    assert_eq!(ws.len(), 1);
    assert!(!wt_path.exists());
}

#[tokio::test]
async fn remove_worktree_dirty_without_force_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "dirty-no-force").await.unwrap();
    // Dirty the worktree (modify the checked-out copy of a.txt).
    write(&wt_path.join("a.txt"), "modified\n");

    let err = repo.remove_worktree(&wt_path, false).await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
    assert!(wt_path.exists(), "worktree should still be present");
}

#[tokio::test]
async fn remove_worktree_dirty_with_force_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "dirty-force").await.unwrap();
    write(&wt_path.join("a.txt"), "modified\n");

    repo.remove_worktree(&wt_path, true).await.unwrap();
    assert!(!wt_path.exists());
}

#[tokio::test]
async fn remove_main_worktree_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let main = repo.workdir().to_path_buf();
    let err = repo.remove_worktree(&main, false).await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn add_worktree_path_already_exists_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");
    std::fs::create_dir(&wt_path).unwrap();
    write(&wt_path.join("blocker"), "exists\n");

    let repo = Repository::open(p).await.unwrap();
    let err = repo.add_worktree(&wt_path, "exists").await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

#[tokio::test]
async fn add_worktree_slug_with_slash_errors_before_git_call() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    let err = repo.add_worktree(&wt_path, "foo/bar").await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
    // Defense-in-depth: validation runs before any side effect.
    assert!(!wt_path.exists());
}

#[tokio::test]
async fn add_worktree_existing_branch_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    // Pre-create the branch — `worktree add -b` will refuse.
    run_git(p, &["branch", "oximux/already-here"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .add_worktree(&wt_path, "already-here")
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

/// The spawn path asks this on every chat, so it runs against a worktree git
/// actually created rather than a hand-built directory — the pointer files it
/// reads are git's, and a fixture that guessed their shape would pass while the
/// real thing failed.
#[tokio::test]
async fn a_linked_worktree_resolves_back_to_its_main_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("oximux-wt-feat-x");
    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "feat-x").await.unwrap();

    let main = oximux_git::main_worktree_of(&wt_path).expect("a linked worktree resolves");
    assert_eq!(main, p.canonicalize().unwrap());

    // The worktree is a *sibling* of the repo, not a child — which is why
    // containment alone cannot decide this and the lookup has to exist.
    assert!(!wt_path.starts_with(p));
}

#[tokio::test]
async fn a_primary_worktree_is_not_a_linked_one() {
    // `.git` is a directory here, so the question answers itself and no
    // capability is inherited from a repository that is already itself.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    assert_eq!(oximux_git::main_worktree_of(p), None);
}

#[tokio::test]
async fn a_directory_that_is_not_a_repository_resolves_to_nothing() {
    // Fails closed: an uncertain answer withholds the capability.
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(oximux_git::main_worktree_of(tmp.path()), None);
    assert_eq!(
        oximux_git::main_worktree_of(std::path::Path::new("/definitely/not/here")),
        None
    );
}
