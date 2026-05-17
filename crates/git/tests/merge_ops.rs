//! Integration tests for merge operations on `Repository`: `merge_branch`
//! with auto-stash recovery. Tempdir + real `git` binary on PATH.
//!
//! Tests `merge_dirty_auto_stash_conflict` and
//! `merge_dirty_auto_stash_ref_pop_restores_work` are the critical-mitigation
//! tests for "Merge auto-stash recovery loses work" (Low likelihood,
//! **Critical** impact per phase-02-git-core.md risk table).

mod common;

use common::{init_repo, run_git, write};
use oximux_core::MergeOutcome;
use oximux_git::{GitError, Repository};

/// Helper: init repo with one base commit on `main` and a `feature` branch
/// pointing at the same SHA. Sync (no Repository::open) — each test opens
/// its own Repository inside its async body after running this.
fn init_with_main_and_feature(p: &std::path::Path) {
    init_repo(p);
    write(&p.join("a.txt"), "base\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "base"]);
    run_git(p, &["branch", "feature"]);
}

#[tokio::test]
async fn merge_fast_forward_returns_ff() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    // Add a commit on feature so main can fast-forward to it.
    run_git(p, &["checkout", "feature"]);
    write(&p.join("b.txt"), "feature\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "feature commit"]);
    run_git(p, &["checkout", "main"]);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    assert!(
        matches!(outcome, MergeOutcome::FastForward),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn merge_true_merge_returns_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    // Diverge: commit on feature AND on main, so a true merge is required.
    run_git(p, &["checkout", "feature"]);
    write(&p.join("b.txt"), "feature\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);
    write(&p.join("c.txt"), "main\n");
    run_git(p, &["add", "c.txt"]);
    run_git(p, &["commit", "-m", "main"]);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    assert!(matches!(outcome, MergeOutcome::Merged), "got {outcome:?}");
}

#[tokio::test]
async fn merge_conflicts_returns_conflicted_with_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    // Both sides edit a.txt at the same line → conflict.
    run_git(p, &["checkout", "feature"]);
    write(&p.join("a.txt"), "feature edit\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);
    write(&p.join("a.txt"), "main edit\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "main"]);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    match outcome {
        MergeOutcome::Conflicted {
            conflicts,
            auto_stash,
        } => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0], std::path::Path::new("a.txt"));
            assert!(auto_stash.is_none(), "clean main → no auto-stash");
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
}

#[tokio::test]
async fn merge_dirty_main_auto_stash_ff() {
    // Dirty main + FF-able feature → stash, FF, pop → AutoStashed { FF }.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    run_git(p, &["checkout", "feature"]);
    write(&p.join("b.txt"), "feature\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);
    // Dirty main with a change in an UNRELATED file (a.txt) — the stash will
    // restore cleanly after the FF.
    write(&p.join("a.txt"), "local dirty\n");

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    match outcome {
        MergeOutcome::AutoStashed {
            stash_ref: _,
            inner,
            pop_failed,
        } => {
            assert!(matches!(*inner, MergeOutcome::FastForward));
            assert!(!pop_failed, "clean stash pop should succeed");
        }
        other => panic!("expected AutoStashed, got {other:?}"),
    }
    // Pop happened on user's behalf → worktree is dirty again with our edit.
    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "local dirty\n");
    // The b.txt from feature is present (the merge landed).
    assert!(p.join("b.txt").exists());
    // Stash stack is empty (auto-stash was popped).
    assert_eq!(repo.stash_list().await.unwrap().len(), 0);
}

#[tokio::test]
async fn merge_dirty_auto_stash_conflict() {
    // CRITICAL test 6: dirty main + feature changes the SAME line in a.txt →
    // stash, attempt merge, conflicts arise, stash MUST NOT be auto-popped
    // (the conflict markers in the working tree would clash with it). The
    // ref must be present in `Conflicted.auto_stash`.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    run_git(p, &["checkout", "feature"]);
    write(&p.join("a.txt"), "feature edit\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);
    write(&p.join("a.txt"), "main edit\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "main"]);
    // Dirty main in a DIFFERENT file so the auto-stash captures real work.
    write(&p.join("scratch.txt"), "important scratch\n");
    run_git(p, &["add", "scratch.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    match outcome {
        MergeOutcome::Conflicted {
            conflicts,
            auto_stash,
        } => {
            assert!(!conflicts.is_empty(), "must report conflicts");
            assert!(
                auto_stash.is_some(),
                "dirty main → auto-stash ref must be returned"
            );
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
    // The stash is still on the stack (NOT auto-popped on conflict).
    assert_eq!(
        repo.stash_list().await.unwrap().len(),
        1,
        "stash must survive conflict — caller resolves first"
    );
}

#[tokio::test]
async fn merge_dirty_auto_stash_ref_pop_restores_work() {
    // CRITICAL test 7: the auto_stash ref returned in `Conflicted` must be
    // poppable AFTER the user resolves the conflict, and the popped content
    // must match what was on disk before the merge attempt.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    run_git(p, &["checkout", "feature"]);
    write(&p.join("a.txt"), "feature edit\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);
    write(&p.join("a.txt"), "main edit\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "main"]);
    let stashed_content = "important scratch\n";
    write(&p.join("scratch.txt"), stashed_content);
    run_git(p, &["add", "scratch.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    let stash_ref = match outcome {
        MergeOutcome::Conflicted {
            auto_stash: Some(r),
            ..
        } => r,
        other => panic!("expected Conflicted with auto_stash, got {other:?}"),
    };
    // User-side conflict resolution: pick `main edit` and commit the merge.
    write(&p.join("a.txt"), "resolved\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "resolve merge"]);
    // Now pop the auto-stash — the user's scratch.txt work must come back.
    repo.stash_pop(&stash_ref).await.unwrap();
    let on_disk = std::fs::read_to_string(p.join("scratch.txt")).unwrap();
    assert_eq!(on_disk, stashed_content, "stashed work must round-trip");
}

#[tokio::test]
async fn merge_nonexistent_branch_errors_and_stash_is_clean() {
    // Q1 = pop-on-failure (atomic). Dirty main + unknown branch → stash gets
    // pushed, merge fails NonZero, stash gets popped, error propagates.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    let scratch = "pre-merge scratch\n";
    write(&p.join("scratch.txt"), scratch);
    run_git(p, &["add", "scratch.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.merge_branch("does-not-exist").await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
    // Stash stack is clean — the auto-stash was popped on failure.
    assert_eq!(repo.stash_list().await.unwrap().len(), 0);
    // The user's scratch.txt is back on disk.
    let on_disk = std::fs::read_to_string(p.join("scratch.txt")).unwrap();
    assert_eq!(on_disk, scratch);
}

#[tokio::test]
async fn merge_already_up_to_date_distinguished_from_merge() {
    // Merging a branch with no new commits → git emits "Already up to date.".
    // Distinguishing this from a real merge lets the UI show "nothing to do"
    // rather than a misleading "merge complete."
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    assert!(
        matches!(outcome, MergeOutcome::AlreadyUpToDate),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn merge_branch_empty_name_errors_before_git_call() {
    // Defense-in-depth: empty branch name is rejected without spawning git
    // (which would error with a cryptic message anyway).
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.merge_branch("").await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn merge_clean_main_no_conflicts_no_stash() {
    // Symmetric to the dirty-main variants: clean main + non-conflicting
    // feature → no stash should appear in the outcome.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    run_git(p, &["checkout", "feature"]);
    write(&p.join("b.txt"), "feature\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);

    let repo = Repository::open(p).await.unwrap();
    let outcome = repo.merge_branch("feature").await.unwrap();
    // Should NOT be AutoStashed since main was clean.
    assert!(
        !matches!(outcome, MergeOutcome::AutoStashed { .. }),
        "clean main must not produce AutoStashed; got {outcome:?}"
    );
}

#[tokio::test]
async fn merge_autostash_ref_survives_conflict_resolution() {
    // Belt-and-suspenders for the critical path: even after the user does
    // their own `git stash list` / `git stash show <ref>`, the ref string we
    // returned is still valid for `stash_apply` / `stash_drop`. (Distinct
    // from test 7 which does a pop; here we do apply + drop.)
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_with_main_and_feature(p);
    run_git(p, &["checkout", "feature"]);
    write(&p.join("a.txt"), "feature\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "feature"]);
    run_git(p, &["checkout", "main"]);
    write(&p.join("a.txt"), "main\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "main"]);
    write(&p.join("scratch.txt"), "scratch\n");
    run_git(p, &["add", "scratch.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let stash_ref = match repo.merge_branch("feature").await.unwrap() {
        MergeOutcome::Conflicted {
            auto_stash: Some(r),
            ..
        } => r,
        other => panic!("expected Conflicted, got {other:?}"),
    };
    // Resolve and commit the merge first so the working tree is clean.
    write(&p.join("a.txt"), "resolved\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "resolve"]);
    repo.stash_apply(&stash_ref).await.unwrap();
    assert!(p.join("scratch.txt").exists());
    // Reset for the drop step so the stash content doesn't clash on a
    // subsequent worktree op (apply leaves entry on stack — see stash_ops).
    run_git(p, &["checkout", "--", "scratch.txt"]);
    let _ = std::fs::remove_file(p.join("scratch.txt"));
    repo.stash_drop(&stash_ref).await.unwrap();
    assert_eq!(repo.stash_list().await.unwrap().len(), 0);
}
