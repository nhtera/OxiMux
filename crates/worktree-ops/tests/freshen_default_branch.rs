//! Integration tests for `freshen_default_branch` — real `git` binary in a
//! tempdir, same style as `workspace_merge.rs`.
//!
//! The interesting property is not that the fast-forward works; it is that
//! **every other path leaves the repository untouched**. This runs on the
//! create path, where the user asked for a worktree and not for a sync, so a
//! skip is the correct outcome for a dirty tree, a local-only commit, a
//! missing remote, and an unreachable one. Each of those has its own test, and
//! each asserts the local sha did not move.

use std::path::Path;
use std::process::Command;

use oximux_git::Repository;
use oximux_worktree_ops::freshen::{Freshened, SkipReason, freshen_default_branch};

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .expect("git not on PATH");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

fn init_repo(cwd: &Path) {
    run_git(cwd, &["init", "-b", "main"]);
    run_git(cwd, &["config", "commit.gpgsign", "false"]);
    run_git(cwd, &["config", "user.name", "Test"]);
    run_git(cwd, &["config", "user.email", "test@example.com"]);
    std::fs::write(cwd.join("a.txt"), "v1\n").expect("write seed");
    run_git(cwd, &["add", "a.txt"]);
    run_git(cwd, &["commit", "-m", "init"]);
}

fn commit(cwd: &Path, text: &str, msg: &str) {
    std::fs::write(cwd.join("a.txt"), text).expect("write");
    run_git(cwd, &["add", "a.txt"]);
    run_git(cwd, &["commit", "-m", msg]);
}

fn sha(cwd: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A clone with `origin` pointing at an upstream that is one commit ahead.
/// Returns `(tmp, clone_path, upstream_path)`.
fn clone_behind_by_one() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    init_repo(&upstream);

    let clone = tmp.path().join("clone");
    run_git(
        tmp.path(),
        &["clone", "-q", upstream.to_str().unwrap(), clone.to_str().unwrap()],
    );
    run_git(&clone, &["config", "user.name", "Test"]);
    run_git(&clone, &["config", "user.email", "test@example.com"]);
    run_git(&clone, &["config", "commit.gpgsign", "false"]);

    // Upstream moves ahead. The clone's `main` is now one behind.
    commit(&upstream, "v2\n", "upstream moves");
    (tmp, clone, upstream)
}

#[tokio::test]
async fn a_clean_checkout_on_the_default_branch_fast_forwards() {
    let (_tmp, clone, upstream) = clone_behind_by_one();
    let repo = Repository::open(&clone).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    let want = sha(&upstream, "main");
    assert_eq!(
        outcome,
        Freshened::FastForwarded { branch: "main".to_string(), to: want.clone() },
        "expected a fast-forward"
    );
    assert_eq!(sha(&clone, "main"), want, "local main did not move");
    assert_eq!(std::fs::read_to_string(clone.join("a.txt")).expect("read"), "v2\n");
}

/// The default branch is not checked out here — the checkout sits on a feature
/// branch, which is the state a worktree-heavy workflow is usually in. This is
/// the path that cannot use `merge --ff-only`, because git will not merge into
/// a branch you are not on.
#[tokio::test]
async fn a_default_branch_that_is_not_checked_out_still_fast_forwards() {
    let (_tmp, clone, upstream) = clone_behind_by_one();
    run_git(&clone, &["checkout", "-q", "-b", "feature"]);
    let repo = Repository::open(&clone).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    let want = sha(&upstream, "main");
    assert!(
        matches!(&outcome, Freshened::FastForwarded { to, .. } if *to == want),
        "got {outcome:?}"
    );
    assert_eq!(sha(&clone, "main"), want, "local main did not move");
    // The checkout itself must not have been disturbed.
    assert_eq!(
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&clone)
                .output()
                .expect("git")
                .stdout
        )
        .trim(),
        "feature"
    );
}

/// The single most important skip: uncommitted work in the tree being
/// fast-forwarded. Creating a worktree must never touch it.
#[tokio::test]
async fn a_dirty_default_checkout_is_skipped_and_left_alone() {
    let (_tmp, clone, _upstream) = clone_behind_by_one();
    std::fs::write(clone.join("a.txt"), "my uncommitted edit\n").expect("write");
    let before = sha(&clone, "main");
    let repo = Repository::open(&clone).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    assert_eq!(outcome, Freshened::Skipped(SkipReason::WorkingTreeDirty));
    assert_eq!(sha(&clone, "main"), before, "main moved under a dirty tree");
    assert_eq!(
        std::fs::read_to_string(clone.join("a.txt")).expect("read"),
        "my uncommitted edit\n",
        "the user's edit was disturbed"
    );
}

/// A local commit the remote does not have means fast-forwarding would lose
/// it. Skip, and say which reason it was — this one is worth telling apart
/// from "already current" in a log.
#[tokio::test]
async fn a_local_only_commit_is_skipped_rather_than_discarded() {
    let (_tmp, clone, _upstream) = clone_behind_by_one();
    commit(&clone, "local work\n", "local only");
    let before = sha(&clone, "main");
    let repo = Repository::open(&clone).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    assert_eq!(outcome, Freshened::Skipped(SkipReason::LocalOnlyCommits));
    assert_eq!(sha(&clone, "main"), before, "a local commit was discarded");
}

#[tokio::test]
async fn an_already_current_branch_reports_it_rather_than_running_a_merge() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).expect("mkdir");
    init_repo(&upstream);
    let clone = tmp.path().join("clone");
    run_git(
        tmp.path(),
        &["clone", "-q", upstream.to_str().unwrap(), clone.to_str().unwrap()],
    );
    let before = sha(&clone, "main");
    let repo = Repository::open(&clone).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    assert_eq!(outcome, Freshened::Skipped(SkipReason::AlreadyCurrent));
    assert_eq!(sha(&clone, "main"), before);
}

/// The overwhelmingly common case for a local-only repository: no remote at
/// all. It must change nothing.
///
/// Note where it skips. `git fetch --all --prune` *succeeds* in a repo with no
/// remotes — there is nothing to fetch, which is not an error — so this lands
/// on `NoRemoteCounterpart`, one step later than the name `FetchFailed` would
/// suggest. That variant is still reachable, by a repo that has a remote it
/// cannot reach; this one simply is not it.
#[tokio::test]
async fn a_repository_with_no_remote_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    let before = sha(tmp.path(), "main");
    let repo = Repository::open(tmp.path()).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    assert_eq!(outcome, Freshened::Skipped(SkipReason::NoRemoteCounterpart));
    assert_eq!(sha(tmp.path(), "main"), before);
}

/// A remote that is configured but unreachable — the offline case. This is
/// what `FetchFailed` is actually for.
#[tokio::test]
async fn an_unreachable_remote_is_skipped_at_the_fetch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_repo(tmp.path());
    let missing = tmp.path().join("no-such-repo");
    run_git(
        tmp.path(),
        &["remote", "add", "origin", missing.to_str().unwrap()],
    );
    let before = sha(tmp.path(), "main");
    let repo = Repository::open(tmp.path()).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    assert!(
        matches!(outcome, Freshened::Skipped(SkipReason::FetchFailed(_))),
        "got {outcome:?}"
    );
    assert_eq!(sha(tmp.path(), "main"), before);
}

/// A remote that exists but has no branch by that name. Distinct from "no
/// remote": the fetch succeeds and there is simply nothing to compare against.
#[tokio::test]
async fn a_remote_without_that_branch_is_skipped_after_a_successful_fetch() {
    let (_tmp, clone, _upstream) = clone_behind_by_one();
    let before = sha(&clone, "main");
    let repo = Repository::open(&clone).await.expect("open");

    // `develop` exists nowhere. The fetch still succeeds.
    let outcome = freshen_default_branch(&repo, "develop").await;

    assert_eq!(outcome, Freshened::Skipped(SkipReason::NoRemoteCounterpart));
    assert_eq!(sha(&clone, "main"), before, "an unrelated branch was touched");
}

/// Diverged, not merely behind: both sides have commits the other lacks.
/// `merge --ff-only` would refuse; this refuses first, and for the reason a
/// person would give.
#[tokio::test]
async fn a_diverged_branch_is_skipped_and_never_merged() {
    let (_tmp, clone, upstream) = clone_behind_by_one();
    commit(&clone, "mine\n", "local divergence");
    let local_before = sha(&clone, "main");
    let upstream_sha = sha(&upstream, "main");
    assert_ne!(local_before, upstream_sha);
    let repo = Repository::open(&clone).await.expect("open");

    let outcome = freshen_default_branch(&repo, "main").await;

    assert_eq!(outcome, Freshened::Skipped(SkipReason::LocalOnlyCommits));
    assert_eq!(sha(&clone, "main"), local_before);
    // The decisive assertion: no merge commit was created. A `--ff-only`
    // refusal and a successful merge both leave the command "finished".
    let parents = Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", "main"])
        .current_dir(&clone)
        .output()
        .expect("git");
    let line = String::from_utf8_lossy(&parents.stdout);
    assert_eq!(
        line.split_whitespace().count(),
        2,
        "main gained a second parent — something merged: {line}"
    );
}
