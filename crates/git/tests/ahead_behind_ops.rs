//! Real-git tests for `ahead_behind_vs_base`: the base-resolution order and
//! the one property the rail depends on — "cannot compute" is `None`, never
//! a zero pair.

mod common;

use std::path::Path;

use common::{init_repo, run_git, write};
use oximux_git::{AheadBehind, ahead_behind_against, ahead_behind_vs_base};

fn commit(p: &Path, name: &str) {
    write(&p.join(name), name);
    run_git(p, &["add", name]);
    run_git(p, &["commit", "-q", "-m", name]);
}

/// A repo with `main` (2 commits) and `topic` (branched at 1, +2 own commits,
/// so 1 behind main and 2 ahead), checked out on `topic`.
fn diverged_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    commit(p, "base");
    run_git(p, &["checkout", "-q", "-b", "topic"]);
    commit(p, "t1");
    commit(p, "t2");
    run_git(p, &["checkout", "-q", "main"]);
    commit(p, "m2");
    run_git(p, &["checkout", "-q", "topic"]);
    tmp
}

#[tokio::test]
async fn a_branch_with_no_upstream_is_measured_against_the_default_branch() {
    let tmp = diverged_repo();
    let got = ahead_behind_vs_base(tmp.path(), None, "main").await.unwrap();
    assert_eq!(
        got,
        Some(AheadBehind {
            base: "main".into(),
            ahead: 2,
            behind: 1
        })
    );
}

#[tokio::test]
async fn a_pinned_base_wins_over_everything_else() {
    let tmp = diverged_repo();
    let p = tmp.path();
    // A second ref one commit further along main, so pinning it changes the
    // number — proof the pin was honoured rather than main being found first.
    run_git(p, &["branch", "release", "main"]);
    run_git(p, &["checkout", "-q", "release"]);
    commit(p, "r1");
    run_git(p, &["checkout", "-q", "topic"]);
    let got = ahead_behind_vs_base(p, Some("release"), "main").await.unwrap().unwrap();
    assert_eq!((got.base.as_str(), got.ahead, got.behind), ("release", 2, 2));
}

#[tokio::test]
async fn an_upstream_outranks_the_default_branch_and_is_named() {
    let tmp = diverged_repo();
    let p = tmp.path();
    // Stand in for a remote with a plain local ref and `branch.<n>.remote=.`,
    // which is how git itself represents a local-tracking upstream.
    run_git(p, &["branch", "shared", "main"]);
    run_git(p, &["branch", "--set-upstream-to=shared", "topic"]);
    let got = ahead_behind_vs_base(p, None, "main").await.unwrap().unwrap();
    assert_eq!(got.base, "shared");
    assert_eq!((got.ahead, got.behind), (2, 1));
}

#[tokio::test]
async fn nothing_resolvable_is_none_not_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    commit(p, "only");
    // No upstream, and the default branch the caller believes in does not
    // exist here under either name.
    assert_eq!(ahead_behind_vs_base(p, None, "develop").await.unwrap(), None);
    assert_eq!(ahead_behind_vs_base(p, Some("nope"), "").await.unwrap(), None);
    assert_eq!(ahead_behind_against(p, "nope").await.unwrap(), None);
}

#[tokio::test]
async fn level_with_the_base_is_a_zero_pair_not_none() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    commit(p, "only");
    assert_eq!(
        ahead_behind_vs_base(p, None, "main").await.unwrap(),
        Some(AheadBehind {
            base: "main".into(),
            ahead: 0,
            behind: 0
        })
    );
}

/// The upstream step is one process: a branch whose upstream is *gone* (the
/// remote branch deleted after a merge) must fall through to the default
/// branch rather than reporting against a ref that no longer exists.
#[tokio::test]
async fn a_gone_upstream_falls_through_to_the_default_branch() {
    let tmp = diverged_repo();
    let p = tmp.path();
    run_git(p, &["branch", "shared", "main"]);
    run_git(p, &["branch", "--set-upstream-to=shared", "topic"]);
    run_git(p, &["branch", "-D", "shared"]);
    let got = ahead_behind_vs_base(p, None, "main").await.unwrap().unwrap();
    assert_eq!((got.base.as_str(), got.ahead, got.behind), ("main", 2, 1));
}

/// The upstream step follows the worktree's HEAD, not what a caller believes
/// is checked out: after a `git switch` the app did not make, the numbers
/// describe the branch that is actually there.
#[tokio::test]
async fn the_upstream_step_follows_head_not_a_remembered_branch() {
    let tmp = diverged_repo();
    let p = tmp.path();
    run_git(p, &["branch", "shared", "main"]);
    run_git(p, &["branch", "--set-upstream-to=shared", "topic"]);
    // Leave `topic` (which has an upstream) for `main` (which has none).
    run_git(p, &["checkout", "-q", "main"]);
    let got = ahead_behind_vs_base(p, None, "main").await.unwrap().unwrap();
    assert_eq!((got.base.as_str(), got.ahead, got.behind), ("main", 0, 0));
    // And a detached HEAD has no branch to have an upstream: fallback again.
    run_git(p, &["checkout", "-q", "--detach", "topic"]);
    let got = ahead_behind_vs_base(p, None, "main").await.unwrap().unwrap();
    assert_eq!((got.base.as_str(), got.ahead, got.behind), ("main", 2, 1));
}

#[tokio::test]
async fn a_default_branch_held_only_by_the_remote_is_found_under_origin() {
    let tmp = diverged_repo();
    let p = tmp.path();
    // Move `main` to `origin/main` so only the remote-tracking name exists,
    // the shape of a fresh worktree whose default branch was never checked
    // out locally.
    run_git(p, &["update-ref", "refs/remotes/origin/main", "main"]);
    run_git(p, &["branch", "-D", "main"]);
    let got = ahead_behind_vs_base(p, None, "main").await.unwrap().unwrap();
    assert_eq!((got.base.as_str(), got.ahead, got.behind), ("origin/main", 2, 1));
}
