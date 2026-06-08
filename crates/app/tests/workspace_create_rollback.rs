//! Integration test for the workspace create-with-rollback flow.
//!
//! Sets up a real git repo + an in-memory storage DB, pre-inserts a
//! workspace with the slug we are about to derive (forcing a UNIQUE
//! conflict), runs the orchestration, and asserts the rollback removed
//! the freshly-created worktree directory and `oximux/<slug>` branch.

use std::path::Path;
use std::process::Command;

use oximux_app::shell::workspace_ops::{CreateOutcome, create_workspace_with_rollback};
use oximux_git::Repository;
use oximux_storage::{ProjectRepo, WorkspaceRepo, open_memory};

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

#[tokio::test]
async fn rollback_on_insert_conflict_removes_worktree_and_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);

    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    // Pre-insert a workspace with the slug we are about to derive — this
    // forces the UNIQUE conflict on `(project_id, slug)` when the
    // orchestration tries to insert after the git step.
    let slug = "fix-login";
    workspace_repo
        .insert(
            &project.id,
            "Pre-existing",
            slug,
            "oximux/fix-login",
            "/dummy",
        )
        .expect("pre-insert");

    let worktree_path = tmp.path().join("worktrees").join(slug);

    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Fix Login",
        slug,
        &worktree_path,
        None,
        &workspace_repo,
    )
    .await;

    match outcome {
        CreateOutcome::StorageFailedRollbackClean(_) => {
            // Expected outcome: storage insert raised Conflict, rollback succeeded.
        }
        other => panic!("expected StorageFailedRollbackClean, got {other:?}"),
    }

    // Rollback assertions:
    // 1. Worktree directory removed from disk.
    assert!(
        !worktree_path.exists(),
        "worktree dir should be removed: {}",
        worktree_path.display()
    );

    // 2. `oximux/<slug>` branch absent from `git branch`.
    let repo = Repository::open(project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("list branches");
    let branch_names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(
        !branch_names.contains(&"oximux/fix-login"),
        "branch should be deleted; got: {branch_names:?}"
    );
}

#[tokio::test]
async fn create_workspace_happy_path_inserts_row_and_keeps_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);

    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");
    let slug = "new-feat";
    let worktree_path = tmp.path().join("worktrees").join(slug);

    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "New Feat",
        slug,
        &worktree_path,
        None,
        &workspace_repo,
    )
    .await;

    let workspace = match outcome {
        CreateOutcome::Created(ws) => ws,
        other => panic!("expected Created, got {other:?}"),
    };

    assert_eq!(workspace.slug, slug);
    assert_eq!(workspace.branch, "oximux/new-feat");
    assert!(worktree_path.exists(), "worktree dir should exist on disk");

    let repo = Repository::open(project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("list");
    assert!(
        branches.iter().any(|b| b.name == "oximux/new-feat"),
        "branch should be present"
    );
}
