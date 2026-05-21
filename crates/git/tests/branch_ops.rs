//! Integration tests for branch operations on `Repository`: `list_branches`,
//! `create_branch`, `switch_branch`. Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::{GitError, Repository};

#[tokio::test]
async fn list_branches_single_main() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let bs = repo.list_branches().await.unwrap();
    assert_eq!(bs.len(), 1);
    assert_eq!(bs[0].name, "main");
    assert!(bs[0].is_current);
    assert_eq!(bs[0].upstream, None);
}

#[tokio::test]
async fn create_branch_appears_in_list() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    repo.create_branch("feature/x", None).await.unwrap();
    let bs = repo.list_branches().await.unwrap();
    assert_eq!(bs.len(), 2);
    let names: Vec<&str> = bs.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"feature/x"), "got: {names:?}");
    // create_branch does NOT switch — main is still current.
    let current = bs.iter().find(|b| b.is_current).expect("a current branch");
    assert_eq!(current.name, "main");
}

#[tokio::test]
async fn create_branch_from_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "first"]);
    // Grab the first-commit SHA, then add a second commit on main.
    let sha = std::str::from_utf8(
        &std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(p)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "second"]);

    let repo = Repository::open(p).await.unwrap();
    repo.create_branch("from-first", Some(&sha)).await.unwrap();
    // Verify the new branch points at the first SHA via git rev-parse.
    let tip = std::str::from_utf8(
        &std::process::Command::new("git")
            .args(["rev-parse", "from-first"])
            .current_dir(p)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(tip, sha, "from-first should be at the first commit");
}

#[tokio::test]
async fn create_branch_invalid_name_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.create_branch("bad..name", None).await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

#[tokio::test]
async fn create_branch_from_nonexistent_ref_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .create_branch("from-ghost", Some("does-not-exist"))
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

#[tokio::test]
async fn switch_branch_changes_current() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    repo.create_branch("other", None).await.unwrap();
    repo.switch_branch("other").await.unwrap();
    let bs = repo.list_branches().await.unwrap();
    let current = bs.iter().find(|b| b.is_current).unwrap();
    assert_eq!(current.name, "other");
}

#[tokio::test]
async fn switch_branch_dirty_errors() {
    // `git switch` only refuses when local changes WOULD CONFLICT with the
    // target branch's state. A trivial uncommitted modification on a file
    // that the target branch hasn't touched gets carried over silently. So
    // we create a divergence (target branch edits the same file) and THEN
    // dirty the worktree on the source side.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    run_git(p, &["checkout", "-b", "other"]);
    write(&p.join("a.txt"), "other-version\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "diverge"]);
    run_git(p, &["checkout", "main"]);
    // Now dirty main's worktree — switching to `other` would clobber the
    // local change, so git refuses.
    write(&p.join("a.txt"), "local-edit\n");

    let repo = Repository::open(p).await.unwrap();
    let err = repo.switch_branch("other").await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

#[tokio::test]
async fn delete_branch_removes_merged_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    // A fresh branch off main is merged-by-definition (same SHA); `-d` succeeds.
    run_git(p, &["branch", "to-delete"]);

    let repo = Repository::open(p).await.unwrap();
    repo.delete_branch("to-delete", false).await.unwrap();
    let bs = repo.list_branches().await.unwrap();
    let names: Vec<&str> = bs.iter().map(|b| b.name.as_str()).collect();
    assert!(!names.contains(&"to-delete"), "got: {names:?}");
}

#[tokio::test]
async fn delete_branch_refuses_unmerged_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    // Create + check out + commit on `feat` so its tip diverges from main.
    run_git(p, &["checkout", "-b", "feat"]);
    write(&p.join("b.txt"), "x\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "diverge"]);
    run_git(p, &["checkout", "main"]);

    let repo = Repository::open(p).await.unwrap();
    // `-d` refuses an unmerged branch — must surface as NonZero.
    let err = repo.delete_branch("feat", false).await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
    // `-D` force-deletes regardless.
    repo.delete_branch("feat", true).await.unwrap();
    let bs = repo.list_branches().await.unwrap();
    let names: Vec<&str> = bs.iter().map(|b| b.name.as_str()).collect();
    assert!(!names.contains(&"feat"), "got: {names:?}");
}

#[tokio::test]
async fn delete_branch_rejects_empty_name() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.delete_branch("", false).await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn list_branches_detached_head_hides_pseudo_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "second"]);
    // Detach at the first commit.
    run_git(p, &["checkout", "--detach", "HEAD~1"]);

    let repo = Repository::open(p).await.unwrap();
    let bs = repo.list_branches().await.unwrap();
    // The `(HEAD detached at ...)` pseudo-entry must be filtered. `main` stays
    // listed but with `is_current=false`.
    assert_eq!(bs.len(), 1);
    assert_eq!(bs[0].name, "main");
    assert!(!bs[0].is_current);
}
