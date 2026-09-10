//! Integration test for the workspace create-with-rollback flow.
//!
//! Sets up a real git repo + an in-memory storage DB, pre-inserts a
//! workspace with the slug we are about to derive (forcing a UNIQUE
//! conflict), runs the orchestration, and asserts the rollback removed
//! the freshly-created worktree directory and `oximux/<slug>` branch.

use std::path::Path;
use std::process::Command;

use oximux_app::shell::workspace_ops::{
    CreateBase, CreateOutcome, Provision, create_workspace_with_rollback,
};
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

/// The base every pre-existing test in this file means: a new branch named the
/// shipped way, cut where `git worktree add -b` used to cut it. Keeping these
/// on `new_branch` rather than `new_branch_from` is deliberate — it is the
/// no-start-point argv, so the guards these tests pin stay pinned against the
/// same git invocation they were written for.
fn base_of(slug: &str) -> CreateBase {
    CreateBase::new_branch(branch_of(slug))
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
        .insert(&project.id, "Pre-existing", slug, "oximux/fix-login", "/dummy", true)
        .expect("pre-insert");

    let worktree_path = tmp.path().join("worktrees").join(slug);

    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Fix Login",
        slug,
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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
        &base_of(slug),
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

// ---------------------------------------------------------------------------
// Base ref + existing branch (phase 2)
// ---------------------------------------------------------------------------

/// A repo whose committed setup script writes a marker file, plus a `side`
/// branch that carries the same script but is NOT an ancestor of `main`.
///
/// The marker is the whole point: "did setup run" has to be a fact on disk,
/// not an absence in a log. `create_workspace_with_rollback` reports a setup
/// failure loudly and a setup *skip* quietly, so a test that only checked the
/// outcome would pass whether or not the guard did anything.
fn repo_with_a_marker_script_and_a_side_branch(root: &Path) {
    init_repo(root);
    commit_scripts(
        root,
        "auto_setup = true\nsetup = \"touch setup-ran.marker\"\n",
    );
    // `side` diverges: it commits something `main` does not have, so it is not
    // an ancestor of `main` and reads as an unreviewed base.
    run_git(root, &["checkout", "-q", "-b", "side"]);
    std::fs::write(root.join("theirs.txt"), "contributed\n").expect("write");
    run_git(root, &["add", "theirs.txt"]);
    run_git(root, &["commit", "-m", "their work"]);
    run_git(root, &["checkout", "-q", "main"]);
}

fn seed_project(root: &Path) -> (WorkspaceRepo, oximux_core::Project) {
    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    let project = project_repo
        .insert("Acme", root.to_str().unwrap(), "main")
        .expect("project");
    (workspace_repo, project)
}

/// The guard: provisioning runs the *worktree's own committed* setup script,
/// which is safe only while every worktree branches off the user's own HEAD.
/// A base the user has not reviewed must not run its author's script.
#[tokio::test]
async fn a_base_that_is_not_an_ancestor_of_the_default_skips_the_setup_script() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    let slug = "review-theirs";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Review Theirs",
        slug,
        &CreateBase::new_branch_from(branch_of(slug), "side"),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(
        matches!(outcome, CreateOutcome::Created(_)),
        "the guard skips setup; it does not fail the create: {outcome:?}"
    );
    assert!(
        !worktree_path.join("setup-ran.marker").exists(),
        "an unreviewed base ran its own committed setup script"
    );
    // The worktree is real and based where it was asked to be — skipping setup
    // must not have skipped the create.
    assert!(worktree_path.join("theirs.txt").exists());
}

/// The other arm, and the one that proves the guard is not simply "never run
/// setup any more": a base already contained in the default branch is one the
/// user lives on, and provisioning is unchanged for it.
#[tokio::test]
async fn a_base_that_is_an_ancestor_of_the_default_still_runs_setup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    let slug = "ordinary";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Ordinary",
        slug,
        &CreateBase::new_branch_from(branch_of(slug), "main"),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(matches!(outcome, CreateOutcome::Created(_)), "{outcome:?}");
    assert!(
        worktree_path.join("setup-ran.marker").exists(),
        "a reviewed base must provision exactly as before"
    );
}

/// The skip is a default, not a prohibition — `Run setup` from the row menu is
/// how a user opts in after reading the script, and it has to actually win.
#[tokio::test]
async fn an_explicit_run_setup_overrides_the_unreviewed_base_guard() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    let slug = "opted-in";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Opted In",
        slug,
        &CreateBase::new_branch_from(branch_of(slug), "side"),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision {
            setup: oximux_settings::SetupDecision::Run,
            ..Provision::default()
        },
    )
    .await;

    assert!(matches!(outcome, CreateOutcome::Created(_)), "{outcome:?}");
    assert!(
        worktree_path.join("setup-ran.marker").exists(),
        "an explicit Run must override the guard"
    );
}

/// The reason reaches whoever is watching. A silent skip is indistinguishable
/// from a project with no setup script, which is exactly the confusion the
/// event exists to prevent.
#[tokio::test]
async fn the_skip_reason_reaches_the_provisioning_transcript() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let slug = "watched";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Watched",
        slug,
        &CreateBase::new_branch_from(branch_of(slug), "side"),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::new(oximux_settings::SetupDecision::Inherit, tx),
    )
    .await;
    assert!(matches!(outcome, CreateOutcome::Created(_)), "{outcome:?}");

    let mut reason = None;
    while let Ok(event) = rx.try_recv() {
        if let oximux_app::shell::workspace_ops::ProvisionEvent::SetupSkipped(text) = event {
            reason = Some(text);
        }
    }
    let reason = reason.expect("no SetupSkipped event was emitted");
    assert!(
        reason.contains("side") && reason.contains("main"),
        "the reason must name both refs so the user can judge it: {reason}"
    );
    assert!(
        reason.contains("Run setup"),
        "the reason must name the way out: {reason}"
    );
}

/// An adopted branch is the row's branch. No `oximux/`-prefixed branch is
/// minted, because the user named the branch and we did not.
#[tokio::test]
async fn an_adopted_branch_becomes_the_rows_branch_with_no_prefix_applied() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    let slug = "adopted";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Adopted",
        slug,
        &CreateBase::existing("side"),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    match outcome {
        CreateOutcome::Created(row) => assert_eq!(row.branch, "side"),
        other => panic!("expected Created, got {other:?}"),
    }
    let repo = Repository::open(project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("branches");
    assert!(
        !branches.iter().any(|b| b.name.starts_with("oximux/")),
        "adopting a branch minted one anyway: {branches:?}"
    );
}

/// **The data-loss guard.** Rollback force-deletes the branch — correct for one
/// this create minted a moment ago, and a week of someone's work for one it
/// merely adopted. A failed create must leave an adopted branch exactly as it
/// found it.
#[tokio::test]
async fn a_failed_create_never_deletes_the_branch_it_adopted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    // Force the UNIQUE conflict on `(project_id, slug)` so the insert fails
    // AFTER the git step — the exact window the rollback ladder exists for.
    let slug = "adopted-conflict";
    workspace_repo
        .insert(&project.id, "Pre-existing", slug, "side", "/dummy", false)
        .expect("pre-insert");

    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "Adopted Conflict",
        slug,
        &CreateBase::existing("side"),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    assert!(
        matches!(outcome, CreateOutcome::StorageFailedRollbackClean(_)),
        "expected a clean rollback, got {outcome:?}"
    );
    // The worktree is gone — the rollback did run.
    assert!(!worktree_path.exists(), "rollback left the worktree behind");
    // And the branch survived it.
    let repo = Repository::open(project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("branches");
    assert!(
        branches.iter().any(|b| b.name == "side"),
        "rollback deleted the adopted branch: {branches:?}"
    );
    // Its commit is still reachable, which is the thing that actually matters.
    run_git(project_root, &["rev-parse", "--verify", "side"]);
}

/// **The unattended path.** The chat pill creates a worktree from a draft the
/// user clicked once; it runs with no dialog, no preview, and nobody watching.
/// Basing it on "wherever the main checkout's HEAD happened to be" is the
/// defect with the fewest ways for anyone to notice — the work looks fine
/// until review, when it turns out to carry somebody's half-finished feature
/// branch underneath.
///
/// `workspace_root/render.rs` passes `project.default_branch` explicitly for
/// exactly this reason. This pins the property that choice buys, at the layer
/// where it can be asserted: the render-path listener itself is a GPUI closure
/// with no seam to call.
#[tokio::test]
async fn an_unattended_create_does_not_inherit_a_feature_branch_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    init_repo(project_root);
    let main_tip = {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project_root)
            .output()
            .expect("git");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    // The main checkout wanders off onto a feature branch, as it does all day.
    run_git(project_root, &["checkout", "-q", "-b", "wip"]);
    std::fs::write(project_root.join("half-done.txt"), "wip\n").expect("write");
    run_git(project_root, &["add", "half-done.txt"]);
    run_git(project_root, &["commit", "-m", "half done"]);

    let (workspace_repo, project) = seed_project(project_root);
    let slug = "from-chat";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        project_root,
        &project.id,
        "From Chat",
        slug,
        // What the chat pill passes.
        &CreateBase::new_branch_from(branch_of(slug), &project.default_branch),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;
    assert!(matches!(outcome, CreateOutcome::Created(_)), "{outcome:?}");

    // The new branch sits on `main`'s tip, not on `wip`'s.
    let out = Command::new("git")
        .args(["rev-parse", &branch_of(slug)])
        .current_dir(project_root)
        .output()
        .expect("git");
    let tip = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(tip, main_tip, "an unattended create inherited the wip HEAD");
    assert!(
        !worktree_path.join("half-done.txt").exists(),
        "the feature branch's work leaked into a worktree that never asked for it"
    );
}

/// **The end-to-end C1 regression.** A project whose default branch exists only
/// as `origin/main` — the ordinary state of a worktree-centric checkout after
/// the user deletes the local `main` they never sit on.
///
/// `git worktree add` DWIMs such a name into `--track -b <name>`, overriding an
/// explicit `-b`, so an unguarded create landed the worktree on `main`, never
/// made `oximux/<slug>`, and inserted a row naming a branch that did not exist.
///
/// The create path must still produce a working worktree on the branch it
/// promised: an unresolvable default degrades to HEAD rather than failing, per
/// the plan's own risk note ("fall back … rather than failing creation").
#[tokio::test]
async fn a_default_branch_that_exists_only_on_the_remote_still_creates_the_named_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "-q", "--bare"]);

    let project_root = tmp.path().join("work");
    std::fs::create_dir_all(&project_root).unwrap();
    init_repo(&project_root);
    run_git(&project_root, &["remote", "add", "origin", origin.to_str().unwrap()]);
    run_git(&project_root, &["push", "-q", "origin", "main"]);
    run_git(&project_root, &["checkout", "-q", "-b", "dev"]);
    run_git(&project_root, &["branch", "-D", "main"]);
    run_git(&project_root, &["remote", "set-head", "origin", "main"]);

    let db = open_memory().expect("open memory");
    let project_repo = ProjectRepo::new(db.clone());
    let workspace_repo = WorkspaceRepo::new(db);
    // The stored default is `main` — captured when the local branch still
    // existed, which is exactly how this goes stale in the field.
    let project = project_repo
        .insert("Acme", project_root.to_str().unwrap(), "main")
        .expect("project");

    let slug = "feat";
    let worktree_path = tmp.path().join("worktrees").join(slug);
    let outcome = create_workspace_with_rollback(
        &project_root,
        &project.id,
        "Feat",
        slug,
        // What ⌘N with every control left alone produces.
        &base_of(slug),
        &worktree_path,
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await;

    let row = match outcome {
        CreateOutcome::Created(row) => row,
        other => panic!("a stale default branch must not fail the create: {other:?}"),
    };
    // The row names the branch we minted — not the one git's DWIM wanted.
    assert_eq!(row.branch, branch_of(slug));
    // And that branch actually exists, which is the half the DWIM used to break.
    let repo = Repository::open(&project_root).await.expect("open");
    let branches = repo.list_branches().await.expect("branches");
    assert!(
        branches.iter().any(|b| b.name == branch_of(slug)),
        "the row names a branch that was never created: {branches:?}"
    );
    assert!(
        !branches.iter().any(|b| b.name == "main"),
        "git minted a local `main` behind our back: {branches:?}"
    );
    // The worktree is on our branch, not on `main`.
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&worktree_path)
        .output()
        .expect("git");
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), branch_of(slug));
}

/// **H3, closed end to end.** Create-time rollback already refused to delete an
/// adopted branch, but *deleting the workspace later* went through a different
/// door and ran `git branch -D` unconditionally — the same data loss, reached
/// by the gesture a user actually performs.
///
/// The fix is that the create records which it did. This asserts the record is
/// what a delete path would read, in both directions: an adopted branch is
/// marked so the branch survives, and a minted one is marked so cleanup still
/// happens. Without the second half the guard would leak a dangling branch on
/// every ordinary delete, forever.
#[tokio::test]
async fn the_row_records_whether_the_branch_was_minted_or_adopted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path();
    repo_with_a_marker_script_and_a_side_branch(project_root);
    let (workspace_repo, project) = seed_project(project_root);

    // Adopted: `side` existed before OxiMux ever saw it.
    let adopted = match create_workspace_with_rollback(
        project_root,
        &project.id,
        "Adopted",
        "adopted",
        &CreateBase::existing("side"),
        &tmp.path().join("worktrees").join("adopted"),
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await
    {
        CreateOutcome::Created(row) => row,
        other => panic!("expected Created, got {other:?}"),
    };
    assert_eq!(adopted.branch, "side");
    assert!(
        !adopted.branch_minted,
        "adopting a branch must record that we did NOT create it — otherwise \
         deleting the workspace force-deletes the user's branch"
    );

    // Minted: ours, and cleanup must still remove it.
    let minted = match create_workspace_with_rollback(
        project_root,
        &project.id,
        "Minted",
        "minted",
        &base_of("minted"),
        &tmp.path().join("worktrees").join("minted"),
        None,
        &workspace_repo,
        &Provision::default(),
    )
    .await
    {
        CreateOutcome::Created(row) => row,
        other => panic!("expected Created, got {other:?}"),
    };
    assert!(
        minted.branch_minted,
        "an ordinary create must stay cleanable, or every delete leaks a branch"
    );

    // And both survive the trip through storage, which is where the delete
    // paths read them from.
    let reread = |id: &str| {
        workspace_repo
            .get_by_id(id)
            .expect("get")
            .expect("row")
            .branch_minted
    };
    assert!(!reread(&adopted.id));
    assert!(reread(&minted.id));
}
