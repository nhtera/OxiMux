//! Integration test for the workspace create-with-rollback flow.
//!
//! Sets up a real git repo + an in-memory storage DB, pre-inserts a
//! workspace with the slug we are about to derive (forcing a UNIQUE
//! conflict), runs the orchestration, and asserts the rollback removed
//! the freshly-created worktree directory and `oximux/<slug>` branch.

use std::path::Path;
use std::process::Command;

use oximux_app::shell::workspace_ops::{CreateOutcome, Provision, create_workspace_with_rollback};
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

/// The branch these tests expect: the shipped prefix, spelled once.
///
/// The tests assert on `oximux/<slug>` throughout — they are about the
/// rollback ladder, not about naming — so they pin the shipped prefix rather
/// than resolving settings that no headless test has.
fn branch_of(slug: &str) -> String {
    format!("{}/{slug}", oximux_settings::git::DEFAULT_PREFIX)
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
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
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
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    let workspace = match outcome {
        CreateOutcome::Created(ws) => ws,
        other => panic!("expected Created, got {other:?}"),
    };

    assert_eq!(workspace.slug, slug);
    assert_eq!(workspace.branch, "oximux/new-feat");
    assert!(worktree_path.exists(), "worktree dir should exist on disk");

    // The row is enumerable by the sidebar's `list_for_project` gather — this
    // is what makes a New-Agent worktree show up as a first-class workspace
    // card + ⌘J entry (round-7 worktree-as-workspace).
    let listed = workspace_repo
        .list_for_project(&project.id)
        .expect("list_for_project");
    assert!(
        listed.iter().any(|w| w.id == workspace.id && w.slug == slug),
        "new workspace row should be enumerable for the sidebar; got: {:?}",
        listed.iter().map(|w| &w.slug).collect::<Vec<_>>()
    );

    let repo = Repository::open(project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("list");
    assert!(
        branches.iter().any(|b| b.name == "oximux/new-feat"),
        "branch should be present"
    );
}

/// Commit a `.oximux/scripts.toml` so the *worktree* carries it — the file is
/// committed by design, which is why a fresh worktree of the branch has it.
fn commit_scripts(cwd: &Path, body: &str) {
    let dir = cwd.join(".oximux");
    std::fs::create_dir_all(&dir).expect("mkdir .oximux");
    std::fs::write(dir.join("scripts.toml"), body).expect("write scripts.toml");
    run_git(cwd, &["add", ".oximux/scripts.toml"]);
    run_git(cwd, &["commit", "-m", "scripts"]);
}

/// The phase's central claim: a worktree that reports created is one the user
/// can work in. A setup script that fails must leave nothing behind — not the
/// directory, not the branch, and not a row the sidebar would list.
#[tokio::test]
async fn a_failing_setup_script_rolls_back_worktree_branch_and_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);
    commit_scripts(
        project_root,
        "auto_setup = true\nsetup = \"echo installing deps; exit 2\"\n",
    );

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "bad-setup";
    let worktree_path = tmp.path().join("worktrees").join(slug);

    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Bad Setup",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    let transcript = match outcome {
        CreateOutcome::SetupFailed {
            transcript,
            rollback_error,
        } => {
            assert!(rollback_error.is_none(), "rollback should be clean");
            transcript
        }
        other => panic!("expected SetupFailed, got {other:?}"),
    };
    // The script's own output is what the user needs; a generic failure would
    // send them back to a terminal to reproduce it by hand.
    assert!(
        transcript.output.contains("installing deps"),
        "transcript should carry the script's output: {:?}",
        transcript.output
    );

    assert!(
        !worktree_path.exists(),
        "worktree dir should be removed: {}",
        worktree_path.display()
    );
    let repo = Repository::open(project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("list branches");
    let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(
        !names.contains(&"oximux/bad-setup"),
        "branch should be deleted; got: {names:?}"
    );
    let rows = workspace_repo
        .list_for_project(&project.id)
        .expect("list workspaces");
    assert!(
        rows.is_empty(),
        "no row may be left behind for a worktree that never provisioned: {rows:?}"
    );
}

/// `auto_setup` defaults off, so a project that never opted in cannot have its
/// creation broken by a setup script that happens to be defined. This is the
/// upgrade-safety assertion for the whole phase.
#[tokio::test]
async fn a_failing_setup_script_is_not_run_when_the_project_did_not_opt_in() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);
    // Same failing script, but no `auto_setup`.
    commit_scripts(project_root, "setup = \"exit 2\"\n");

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "opted-out";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Opted Out",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(
        matches!(outcome, CreateOutcome::Created(_)),
        "default-off must keep today's behavior, got {outcome:?}"
    );
    assert!(worktree_path.exists());
}

/// Ordering, asserted by the setup script itself: the include copy runs first,
/// so a script can read the `.env` it needs. A test that only checked the file
/// exists afterwards would pass even if the order were reversed.
#[tokio::test]
async fn included_files_are_present_before_the_setup_script_runs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);
    commit_scripts(
        project_root,
        "auto_setup = true\nsetup = \"test -f .env && cat .env\"\n",
    );
    // Untracked on purpose — this is precisely the file a worktree does not
    // inherit from git, and the reason `.oximuxinclude` exists.
    std::fs::write(project_root.join(".env"), "TOKEN=local\n").expect("write .env");
    std::fs::write(project_root.join(".oximuxinclude"), ".env\n").expect("write include");

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "with-env";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "With Env",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(
        matches!(outcome, CreateOutcome::Created(_)),
        "setup should have found .env, got {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree_path.join(".env")).expect("copied .env"),
        "TOKEN=local\n"
    );
}

/// The include copy is best-effort: a pattern that matches nothing is reported,
/// never fatal. A worktree is still created — it is the setup script, not the
/// copy, that decides whether a missing file actually mattered.
#[tokio::test]
async fn an_include_pattern_matching_nothing_does_not_fail_creation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);
    std::fs::write(project_root.join(".oximuxinclude"), "never-existed.env\n")
        .expect("write include");

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "no-match";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "No Match",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(
        matches!(outcome, CreateOutcome::Created(_)),
        "a missing include must not fail creation, got {outcome:?}"
    );
}

/// The wedge that provisioning made reachable: killing the app during a long
/// setup leaves the worktree and branch on disk with no row naming them. The
/// workspace is then invisible in the rail *and* un-creatable, because
/// `add_worktree` refuses a path that already exists. A retry must clear the
/// debris and succeed rather than failing forever.
#[tokio::test]
async fn an_orphaned_worktree_from_an_interrupted_create_is_reclaimed_on_retry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "interrupted";
    let worktree_path = tmp.path().join("worktrees").join(slug);

    // First create succeeds, leaving worktree + branch + row.
    let first = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Interrupted",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;
    let created = match first {
        CreateOutcome::Created(ws) => ws,
        other => panic!("expected Created, got {other:?}"),
    };

    // Simulate the crash: the row never made it (or was lost), but git's half
    // is on disk. This is exactly the state a kill during setup leaves behind.
    workspace_repo.delete(&created.id).expect("drop the row");
    assert!(worktree_path.exists(), "precondition: the orphan is on disk");

    let retry = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Interrupted",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default().reclaiming_orphans(),
    )
    .await;

    match retry {
        CreateOutcome::Created(ws) => {
            assert_eq!(ws.slug, slug);
            assert!(worktree_path.exists(), "the retry's worktree should be on disk");
        }
        // Before the reclaim this was `GitFailed("add_worktree: ... already
        // exists")`, permanently, with no way out of the UI.
        other => panic!("retry after an interrupted create must succeed, got {other:?}"),
    }
}

/// The reclaim is a delete, so it verifies rather than trusts. A caller that
/// opts in but is wrong — a workspace row *does* name this path — must not lose
/// the user's worktree. Without the in-function row check this test destroys a
/// live workspace and its uncommitted work.
#[tokio::test]
async fn reclaim_refuses_a_path_a_workspace_row_still_names() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "live-work";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let first = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Live Work",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;
    assert!(matches!(first, CreateOutcome::Created(_)), "{first:?}");

    // Uncommitted work the user would lose if the reclaim went ahead.
    std::fs::write(worktree_path.join("wip.txt"), "hours of work\n").expect("write wip");

    // The row is still there, so opting in must not be enough.
    let second = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Live Work",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default().reclaiming_orphans(),
    )
    .await;

    assert!(
        matches!(second, CreateOutcome::GitFailed(_)),
        "a claimed path must fail, not be reclaimed: {second:?}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree_path.join("wip.txt")).expect("wip survives"),
        "hours of work\n",
        "the reclaim deleted a live workspace"
    );
}

/// A caller that has NOT opted in must never have its target path touched, even
/// when no row names it. This is the chat-initiated worktree's protection: it
/// targets a sibling directory beside the project root, where a person's own
/// worktree can legitimately live.
#[tokio::test]
async fn a_caller_that_did_not_opt_in_never_has_its_path_reclaimed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    // A directory the user owns, at a sibling path, that no row knows about.
    let slug = "mine";
    let worktree_path = tmp.path().join("oximux-wt-mine");
    std::fs::create_dir_all(&worktree_path).expect("mkdir");
    std::fs::write(worktree_path.join("notes.txt"), "not yours\n").expect("write");

    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Mine",
        slug,
        &branch_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(
        matches!(outcome, CreateOutcome::GitFailed(_)),
        "an occupied path must fail without opt-in: {outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree_path.join("notes.txt")).expect("survives"),
        "not yours\n",
        "a non-opted-in create deleted a directory it did not own"
    );
}
