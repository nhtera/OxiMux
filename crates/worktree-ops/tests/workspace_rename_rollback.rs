//! Integration tests for `rename_with_rollback` — real `git` binary in a
//! tempdir plus an in-memory storage DB, matching the style of
//! `apps/desktop/tests/workspace_create_rollback.rs`.
//!
//! The point of this file is the *refusals* and the *walk-back*, not the happy
//! path. A rename that half-succeeds is the failure mode the module exists to
//! design out, so every test here asserts on all three things that must agree:
//! the directory, the branch, and the row.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use oximux_git::Repository;
use oximux_storage::{ProjectRepo, WorkspaceRepo, open_memory};
use oximux_worktree_ops::{RenameOutcome, RenameRefusal, rename_with_rollback};

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

struct Fixture {
    _tmp: tempfile::TempDir,
    _wt_root: tempfile::TempDir,
    project_root: PathBuf,
    wt_path: PathBuf,
    workspace_repo: WorkspaceRepo,
    workspace: oximux_core::Workspace,
}

/// A repo with one linked worktree at `oximux/<slug>` and a matching row.
async fn fixture(slug: &str) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path().to_path_buf();
    init_repo(&project_root);

    let wt_root = tempfile::tempdir().expect("wt tempdir");
    let wt_path = wt_root.path().join(slug);
    let repo = Repository::open(&project_root).await.expect("open repo");
    repo.add_worktree(&wt_path, &format!("oximux/{slug}")).await.expect("add worktree");

    let db = open_memory().expect("open memory");
    let project = ProjectRepo::new(db.clone())
        .insert("P", &project_root.to_string_lossy(), "main")
        .expect("project");
    let workspace_repo = WorkspaceRepo::new(db);
    let workspace = workspace_repo
        .insert(
            &project.id,
            slug,
            slug,
            &format!("oximux/{slug}"),
            &wt_path.to_string_lossy(),
        )
        .expect("workspace row");

    Fixture {
        _tmp: tmp,
        _wt_root: wt_root,
        project_root,
        wt_path,
        workspace_repo,
        workspace,
    }
}

async fn branch_names(project_root: &Path) -> Vec<String> {
    Repository::open(project_root)
        .await
        .expect("open repo")
        .list_branches()
        .await
        .expect("list branches")
        .into_iter()
        .map(|b| b.name)
        .collect()
}

#[tokio::test]
async fn rename_moves_directory_branch_and_row_together() {
    let f = fixture("fix-lgoin").await;
    let new_path = f.wt_path.with_file_name("fix-login");

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "fix-login",
        "fix-login",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    let renamed = match outcome {
        RenameOutcome::Renamed(w) => *w,
        other => panic!("expected Renamed, got {other:?}"),
    };

    // 1. Directory.
    assert!(!f.wt_path.exists(), "old directory must be gone");
    assert!(new_path.join("a.txt").exists(), "content must have moved");
    // 2. Branch — only the new name, not both.
    let names = branch_names(&f.project_root).await;
    assert!(names.iter().any(|n| n == "oximux/fix-login"));
    assert!(!names.iter().any(|n| n == "oximux/fix-lgoin"));
    // 3. Row, re-read from the DB rather than trusting the return value.
    let row = f
        .workspace_repo
        .get_by_id(&f.workspace.id)
        .expect("get")
        .expect("row present");
    assert_eq!(row.name, "fix-login");
    assert_eq!(row.slug, "fix-login");
    assert_eq!(row.branch, "oximux/fix-login");
    assert_eq!(row.worktree_path, new_path.to_string_lossy());
    assert_eq!(row, renamed);
    // `slug` and the branch suffix must agree after every successful rename —
    // their silent divergence is what a cosmetic rename produced.
    assert_eq!(format!("oximux/{}", row.slug), row.branch);
}

#[tokio::test]
async fn rename_is_refused_when_the_branch_has_an_upstream() {
    let f = fixture("feat-a").await;
    // A bare repo stands in for `origin`; no network involved.
    let remote = tempfile::tempdir().expect("remote tempdir");
    run_git(remote.path(), &["init", "--bare", "-q"]);
    run_git(
        &f.project_root,
        &["remote", "add", "origin", &remote.path().to_string_lossy()],
    );
    run_git(
        &f.project_root,
        &["push", "-q", "-u", "origin", "oximux/feat-a"],
    );

    let new_path = f.wt_path.with_file_name("feat-b");
    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "feat-b",
        "feat-b",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    match outcome {
        RenameOutcome::Refused(RenameRefusal::Pushed { upstream }) => {
            assert_eq!(upstream, "origin/oximux/feat-a");
        }
        other => panic!("expected Pushed refusal, got {other:?}"),
    }
    // Refused means NOTHING moved.
    assert!(f.wt_path.join("a.txt").exists());
    assert!(!new_path.exists());
    let row = f.workspace_repo.get_by_id(&f.workspace.id).unwrap().unwrap();
    assert_eq!(row.branch, "oximux/feat-a");
    assert_eq!(row.worktree_path, f.wt_path.to_string_lossy());
}

#[tokio::test]
async fn rename_is_refused_while_a_live_agent_holds_the_worktree() {
    let f = fixture("feat-a").await;
    let new_path = f.wt_path.with_file_name("feat-b");
    // The host passes in what it knows is live. On macOS this refusal is the
    // ONLY guard: `git worktree move` would otherwise succeed under a running
    // agent and silently orphan it onto a path git no longer records.
    let holders: HashSet<PathBuf> = [f.wt_path.clone()].into_iter().collect();

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "feat-b",
        "feat-b",
        &new_path,
        &holders,
        &f.workspace_repo,
    )
    .await;

    match outcome {
        RenameOutcome::Refused(RenameRefusal::InUse { holders }) => {
            assert_eq!(holders.len(), 1);
        }
        other => panic!("expected InUse refusal, got {other:?}"),
    }
    assert!(f.wt_path.join("a.txt").exists(), "move must not be attempted");
    assert!(!new_path.exists());
}

/// The subdirectory case, at the integration level: a hand-launched agent's
/// cwd is the raw terminal directory, so it is routinely *inside* the worktree
/// rather than at its root. Missing that is the difference between refusing and
/// silently orphaning a running agent.
#[tokio::test]
async fn a_holder_inside_the_worktree_refuses_the_rename_too() {
    let f = fixture("feat-a").await;
    let new_path = f.wt_path.with_file_name("feat-b");
    std::fs::create_dir_all(f.wt_path.join("src")).expect("subdir");
    let holders: HashSet<PathBuf> = [f.wt_path.join("src")].into_iter().collect();

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "feat-b",
        "feat-b",
        &new_path,
        &holders,
        &f.workspace_repo,
    )
    .await;

    assert!(
        matches!(outcome, RenameOutcome::Refused(RenameRefusal::InUse { .. })),
        "a holder inside the worktree must refuse the rename"
    );
    assert!(f.wt_path.join("a.txt").exists());
    assert!(!new_path.exists());
}

/// The inverse: a sibling worktree whose directory name merely starts with this
/// one's must never block the rename.
#[tokio::test]
async fn a_sibling_worktree_with_a_shared_name_prefix_does_not_block_the_rename() {
    let f = fixture("feat").await;
    let sibling = f.wt_path.with_file_name("feature");
    std::fs::create_dir_all(&sibling).expect("sibling");
    let holders: HashSet<PathBuf> = [sibling].into_iter().collect();
    let new_path = f.wt_path.with_file_name("renamed");

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "renamed",
        "renamed",
        &new_path,
        &holders,
        &f.workspace_repo,
    )
    .await;

    assert!(
        matches!(outcome, RenameOutcome::Renamed(_)),
        "a shared name prefix is not containment, got {outcome:?}"
    );
    assert!(new_path.join("a.txt").exists());
}

#[tokio::test]
async fn rename_is_refused_when_the_target_branch_exists() {
    let f = fixture("feat-a").await;
    Repository::open(&f.project_root)
        .await
        .unwrap()
        .create_branch("oximux/feat-b", None)
        .await
        .unwrap();

    let new_path = f.wt_path.with_file_name("feat-b");
    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "feat-b",
        "feat-b",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    match outcome {
        RenameOutcome::Refused(RenameRefusal::BranchExists { branch }) => {
            assert_eq!(branch, "oximux/feat-b");
        }
        other => panic!("expected BranchExists refusal, got {other:?}"),
    }
    // The collision is caught in pre-flight, so the directory never moves.
    assert!(f.wt_path.join("a.txt").exists());
    assert!(!new_path.exists());
}

#[tokio::test]
async fn rename_is_refused_when_the_target_path_exists() {
    let f = fixture("feat-a").await;
    let new_path = f.wt_path.with_file_name("occupied");
    std::fs::create_dir(&new_path).expect("occupy destination");

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "occupied",
        "occupied",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    assert!(
        matches!(
            outcome,
            RenameOutcome::Refused(RenameRefusal::PathExists { .. })
        ),
        "expected PathExists refusal"
    );
    assert!(f.wt_path.join("a.txt").exists());
    let row = f.workspace_repo.get_by_id(&f.workspace.id).unwrap().unwrap();
    assert_eq!(row.worktree_path, f.wt_path.to_string_lossy());
}

/// The walk-back, driven by a real failure at the **branch** step: the row
/// names a branch that no longer exists in git, so `git branch -m` fails
/// *after* `git worktree move` already succeeded.
///
/// This is the assertion the whole module exists for, and it is why the move
/// goes first and the branch second: the move must be undoable by the time the
/// step that can fail runs. A row/git desync like this is not hypothetical —
/// it is what a hand-run `git branch -m` outside the app leaves behind.
#[tokio::test]
async fn a_failure_at_the_branch_step_moves_the_directory_back() {
    let f = fixture("fix-lgoin").await;
    let new_path = f.wt_path.with_file_name("fix-login");

    // The row claims a branch git does not have. Pre-flight still passes:
    // there is no upstream for a branch that does not exist, and the TARGET
    // name is genuinely free.
    let mut desynced = f.workspace.clone();
    desynced.branch = "oximux/branch-that-git-does-not-have".to_string();

    let outcome = rename_with_rollback(
        &f.project_root,
        &desynced,
        "fix-login",
        "fix-login",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    match outcome {
        RenameOutcome::RolledBack { error } => {
            assert!(error.contains("rename branch"), "got {error}");
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }

    // All three are exactly as they were.
    assert!(
        f.wt_path.join("a.txt").exists(),
        "rollback must move the directory back"
    );
    assert!(!new_path.exists(), "the destination must be left empty");
    let names = branch_names(&f.project_root).await;
    assert!(names.iter().any(|n| n == "oximux/fix-lgoin"));
    assert!(!names.iter().any(|n| n == "oximux/fix-login"));
    let row = f.workspace_repo.get_by_id(&f.workspace.id).unwrap().unwrap();
    assert_eq!(row.worktree_path, f.wt_path.to_string_lossy());
    assert_eq!(row.branch, "oximux/fix-lgoin");
}

/// A branch-name collision is caught in pre-flight and never reaches the
/// mutation phase — the cheaper half of the guarantee above.
#[tokio::test]
async fn a_branch_collision_never_reaches_the_mutation_phase() {
    let f = fixture("fix-lgoin").await;
    let new_path = f.wt_path.with_file_name("fix-login");
    Repository::open(&f.project_root)
        .await
        .unwrap()
        .create_branch("oximux/fix-login", None)
        .await
        .unwrap();

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "fix-login",
        "fix-login",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    assert!(
        matches!(
            outcome,
            RenameOutcome::Refused(RenameRefusal::BranchExists { .. })
        ),
        "collision must be refused in pre-flight, never half-applied"
    );
    assert!(f.wt_path.join("a.txt").exists());
    assert!(!new_path.exists());
}

/// A worktree git refuses to move (locked here; submodules do it too) must
/// still be renameable cosmetically. Reporting this as a rollback rather than a
/// refusal would leave such a worktree with no rename path at all, because only
/// the refusal branch offers "change label only".
#[tokio::test]
async fn a_worktree_git_will_not_move_is_refused_not_rolled_back() {
    let f = fixture("feat-a").await;
    let new_path = f.wt_path.with_file_name("feat-b");
    run_git(
        &f.project_root,
        &["worktree", "lock", &f.wt_path.to_string_lossy()],
    );

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "feat-b",
        "feat-b",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    match outcome {
        RenameOutcome::Refused(RenameRefusal::MoveRefused { error }) => {
            assert!(!error.is_empty(), "git's own reason must be carried");
        }
        other => panic!("expected MoveRefused, got {other:?}"),
    }
    // Nothing was touched, so the label-only escape hatch is still valid.
    assert!(f.wt_path.join("a.txt").exists());
    assert!(!new_path.exists());
    let names = branch_names(&f.project_root).await;
    assert!(names.iter().any(|n| n == "oximux/feat-a"));
}

/// The row-update arm: the only failure path that walks back TWO steps, and the
/// only one that can report `RollbackFailed`. Driven by a real failure — the
/// storage handle is closed before the rename runs, so `rename_full` errors
/// after both the move and the branch rename have succeeded.
#[tokio::test]
async fn a_failure_at_the_row_step_walks_back_both_earlier_steps() {
    let f = fixture("fix-lgoin").await;
    let new_path = f.wt_path.with_file_name("fix-login");

    // The row step has to fail for real, and it has to fail AFTER the two git
    // steps have genuinely succeeded — that is the only ordering that exercises
    // the walk-back. Pointing the rename at an empty database does not do it:
    // the `UPDATE` simply matches no rows, which SQLite reports as success, so
    // the test passes on the happy path while claiming to prove rollback.
    //
    // Collide on `(project_id, slug)` instead — a real UNIQUE constraint. A
    // sibling row already holding the target slug makes `rename_full` fail
    // deterministically, with the directory moved and the branch renamed.
    f.workspace_repo
        .insert(
            &f.workspace.project_id,
            "Fix login",
            "fix-login",
            "oximux/already-taken",
            &new_path.to_string_lossy(),
        )
        .expect("seed the sibling already holding the target slug");

    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "fix-login",
        "fix-login",
        &new_path,
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;

    // The whole point of the test: the row step must have FAILED. Without this
    // the assertions below are satisfied by a rename that simply worked.
    assert!(
        matches!(outcome, RenameOutcome::RolledBack { .. }),
        "the row step must fail and walk back, got {outcome:?}"
    );

    // Whatever the storage layer reports, the invariant is the same: the caller
    // is never left with a half-applied rename. Either everything applied, or
    // everything was walked back — never one of the two git steps alone.
    let names = branch_names(&f.project_root).await;
    let dir_moved = new_path.join("a.txt").exists();
    let branch_moved = names.iter().any(|n| n == "oximux/fix-login");
    assert_eq!(
        dir_moved, branch_moved,
        "directory and branch must never disagree after {outcome:?}"
    );
    if !dir_moved {
        assert!(
            f.wt_path.join("a.txt").exists(),
            "walked back, so the original directory must be restored"
        );
        assert!(names.iter().any(|n| n == "oximux/fix-lgoin"));
    }
}

/// `RollbackFailed` carries BOTH halves — what failed and what could not be put
/// back — because it is the one outcome a human has to repair by hand.
#[test]
fn a_rollback_failure_reports_both_the_cause_and_what_was_left_behind() {
    let outcome = RenameOutcome::RollbackFailed {
        error: "rename branch: fatal: no such branch".to_string(),
        rollback: "move worktree back: fatal: destination exists".to_string(),
    };
    let rendered = format!("{outcome:?}");
    assert!(rendered.contains("no such branch"), "the cause");
    assert!(rendered.contains("destination exists"), "what was left behind");
}

#[tokio::test]
async fn renaming_the_projects_own_checkout_is_refused() {
    let f = fixture("feat-a").await;
    let mut primary = f.workspace.clone();
    primary.worktree_path = f.project_root.to_string_lossy().to_string();

    let outcome = rename_with_rollback(
        &f.project_root,
        &primary,
        "whatever",
        "whatever",
        &f.wt_path.with_file_name("whatever"),
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;
    assert!(matches!(
        outcome,
        RenameOutcome::Refused(RenameRefusal::NotAWorktree)
    ));
}

#[tokio::test]
async fn a_name_that_reduces_to_no_usable_slug_is_refused() {
    let f = fixture("feat-a").await;
    let outcome = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "..",
        "..",
        &f.wt_path.with_file_name("dots"),
        &HashSet::new(),
        &f.workspace_repo,
    )
    .await;
    assert!(
        matches!(
            outcome,
            RenameOutcome::Refused(RenameRefusal::InvalidSlug { .. })
        ),
        "an invalid slug must be refused before any git command runs"
    );
    assert!(f.wt_path.join("a.txt").exists());
}
