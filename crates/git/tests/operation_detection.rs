//! Integration tests for `Repository::current_operation()` — drives real
//! `git merge` / `rebase` / `cherry-pick` / `revert` / `bisect` flows
//! against a tempdir repo and verifies the right `GitOperation`
//! variant surfaces.
//!
//! Each in-progress flow is started via a deliberately conflict-laden
//! setup so the operation pauses (mid-conflict) and the sentinel file
//! persists for the duration of the test — no need to abort/clean up.
//! `tempfile::tempdir` removes the whole repo when the test ends.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::GitOperation;
use oximux_git::Repository;

/// Init main with one base commit + a feature branch that will conflict
/// against main on the next commit. Used by every test that needs to
/// pause a merge / rebase / cherry-pick / revert mid-conflict.
fn init_diverged_with_conflict(p: &std::path::Path) {
    init_repo(p);
    write(&p.join("a.txt"), "base\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "base"]);
    run_git(p, &["branch", "feature"]);

    // feature: change a.txt to "feature\n"
    run_git(p, &["checkout", "feature"]);
    write(&p.join("a.txt"), "feature\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "feature"]);

    // main: change a.txt to "main\n" so a merge/rebase/cherry-pick of
    // feature into main will conflict on a.txt.
    run_git(p, &["checkout", "main"]);
    write(&p.join("a.txt"), "main\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "main"]);
}

/// `current_operation()` panics on its own if `Repository::open` failed
/// to capture `git_dir`. Wrap so the assertion is one expression.
fn op(repo: &Repository) -> Option<GitOperation> {
    repo.current_operation()
}

#[tokio::test]
async fn clean_repo_reports_no_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "x\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "x"]);

    let repo = Repository::open(p).await.unwrap();
    assert_eq!(op(&repo), None, "clean repo should report no in-progress op");
}

#[tokio::test]
async fn merge_in_progress_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_diverged_with_conflict(p);

    // Force `git merge` (NOT --no-commit) — it will pause mid-conflict
    // and leave MERGE_HEAD behind for current_operation to find.
    // `merge_branch` returns Conflicted via the existing backend; we
    // don't care about its return value, only the sentinel.
    let repo = Repository::open(p).await.unwrap();
    let _ = repo.merge_branch("feature").await;

    assert_eq!(op(&repo), Some(GitOperation::Merge));
}

#[tokio::test]
async fn cherry_pick_in_progress_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_diverged_with_conflict(p);

    // Get the feature SHA so we can cherry-pick it into main and
    // conflict on a.txt.
    let feature_sha = std::process::Command::new("git")
        .args(["rev-parse", "feature"])
        .current_dir(p)
        .output()
        .expect("git rev-parse")
        .stdout;
    let sha = String::from_utf8(feature_sha).unwrap().trim().to_string();

    // Cherry-pick will fail with a conflict; ignore the status because
    // the failure IS the in-progress state we want to detect.
    let _ = std::process::Command::new("git")
        .args(["cherry-pick", &sha])
        .current_dir(p)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status();

    let repo = Repository::open(p).await.unwrap();
    assert_eq!(op(&repo), Some(GitOperation::CherryPick));
}

#[tokio::test]
async fn revert_in_progress_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // Commit A
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "v1"]);
    // Commit B (replaces a.txt to v2)
    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "v2"]);
    // Modify a.txt locally so reverting v2 will conflict.
    write(&p.join("a.txt"), "v3\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "v3"]);

    // git revert HEAD~1 (= v2 commit) — conflicts against current v3.
    let _ = std::process::Command::new("git")
        .args(["revert", "--no-edit", "HEAD~1"])
        .current_dir(p)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status();

    let repo = Repository::open(p).await.unwrap();
    assert_eq!(op(&repo), Some(GitOperation::Revert));
}

#[tokio::test]
async fn bisect_in_progress_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "v1"]);
    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "v2"]);

    // Start a bisect session (does NOT need conflict — bisect's
    // sentinel is BISECT_LOG, which appears as soon as `git bisect
    // start` runs).
    run_git(p, &["bisect", "start"]);

    let repo = Repository::open(p).await.unwrap();
    assert_eq!(op(&repo), Some(GitOperation::Bisect));
}

#[tokio::test]
async fn rebase_takes_precedence_over_merge_when_both_sentinels_present() {
    // Synthetic precedence check: stand up a regular .git, then
    // manually drop both `rebase-merge/` and `MERGE_HEAD` so the
    // current_operation precedence rule can be exercised.
    //
    // Real-world this never happens (git enforces one-op-at-a-time),
    // but a partially-cleaned `.git/` after a crash or interrupted
    // sequence could leave a stale MERGE_HEAD alongside an active
    // rebase-merge/. When both are seen, `Rebase` is the more
    // specific diagnosis — the rebase is the live op; the
    // MERGE_HEAD is the straggler.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "x\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "x"]);

    let git_dir = p.join(".git");
    std::fs::create_dir(git_dir.join("rebase-merge")).expect("mkdir rebase-merge");
    std::fs::write(git_dir.join("MERGE_HEAD"), "deadbeef\n").expect("write MERGE_HEAD");

    let repo = Repository::open(p).await.unwrap();
    assert_eq!(op(&repo), Some(GitOperation::Rebase));
}
