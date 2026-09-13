//! Integration tests for the auto-rename path — a real `git` binary in a
//! tempdir plus an in-memory storage DB, in the style of
//! `workspace_rename_rollback.rs`.
//!
//! What these pin, beyond the unit tests in `auto_rename.rs`:
//! - the directory does NOT move, and a live holder inside it does not block
//!   the rename (the reason the phase works at all);
//! - a pushed branch is refused by the engine's own upstream check;
//! - running it twice is a no-op the second time, with no column recording it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use oximux_git::Repository;
use oximux_storage::{ProjectRepo, WorkspaceRepo, open_memory};
use oximux_worktree_ops::{
    Ineligible, RenameOutcome, RenameRefusal, auto_rename_with_rollback, propose_auto_rename,
    rename_with_rollback,
};

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
async fn fixture(slug: &str, minted: bool) -> Fixture {
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
            minted,
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
async fn auto_rename_moves_branch_and_row_but_leaves_the_directory() {
    let f = fixture("amber", true).await;
    let proposal = propose_auto_rename(&f.workspace, "Fix login redirect").unwrap();

    let outcome =
        auto_rename_with_rollback(&f.project_root, &f.workspace, &proposal, &f.workspace_repo)
            .await;
    let renamed = match outcome {
        RenameOutcome::Renamed(ws) => ws,
        other => panic!("expected Renamed, got {other:?}"),
    };

    // Branch: renamed. Directory: exactly where it was.
    let branches = branch_names(&f.project_root).await;
    assert!(branches.contains(&"oximux/fix-login-redirect".to_string()), "{branches:?}");
    assert!(!branches.contains(&"oximux/amber".to_string()), "{branches:?}");
    assert!(f.wt_path.is_dir(), "the codename directory must still exist");
    assert_eq!(renamed.worktree_path, f.wt_path.to_string_lossy());

    // The worktree is checked out on the renamed branch.
    let repo = Repository::open(&f.project_root).await.unwrap();
    let wt = repo
        .list_worktrees()
        .await
        .unwrap()
        .into_iter()
        .find(|w| std::fs::canonicalize(&w.path).ok() == std::fs::canonicalize(&f.wt_path).ok())
        .expect("worktree listed");
    assert_eq!(wt.branch.as_deref(), Some("oximux/fix-login-redirect"));

    // Row: name, slug and branch all follow.
    let row = f.workspace_repo.get_by_id(&f.workspace.id).unwrap().unwrap();
    assert_eq!(row.name, "Fix login redirect");
    assert_eq!(row.slug, "fix-login-redirect");
    assert_eq!(row.branch, "oximux/fix-login-redirect");
    assert_eq!(row.worktree_path, f.wt_path.to_string_lossy());
}

/// The condition auto-rename fires under: an agent live inside the worktree.
/// A same-path rename proceeds; a moving rename with the same holder is
/// still refused, so Phase 3's guard has not been weakened for the case it
/// exists for.
#[tokio::test]
async fn a_live_holder_blocks_a_move_but_not_a_same_path_rename() {
    let f = fixture("amber", true).await;
    let holders: HashSet<PathBuf> = [f.wt_path.join("src")].into_iter().collect();
    std::fs::create_dir_all(f.wt_path.join("src")).unwrap();

    let moving = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "fix-login",
        "fix-login",
        &f.wt_path.with_file_name("fix-login"),
        &holders,
        &f.workspace_repo,
    )
    .await;
    assert!(
        matches!(moving, RenameOutcome::Refused(RenameRefusal::InUse { .. })),
        "a moving rename under a holder must still be refused: {moving:?}"
    );

    let same_path = rename_with_rollback(
        &f.project_root,
        &f.workspace,
        "Fix login",
        "fix-login",
        &f.wt_path,
        &holders,
        &f.workspace_repo,
    )
    .await;
    assert!(matches!(same_path, RenameOutcome::Renamed(_)), "{same_path:?}");
    assert!(f.wt_path.is_dir());
    assert!(branch_names(&f.project_root).await.contains(&"oximux/fix-login".to_string()));
}

#[tokio::test]
async fn auto_rename_runs_at_most_once_per_workspace() {
    let f = fixture("amber", true).await;
    let first = propose_auto_rename(&f.workspace, "Fix login redirect").unwrap();
    let renamed = match auto_rename_with_rollback(
        &f.project_root,
        &f.workspace,
        &first,
        &f.workspace_repo,
    )
    .await
    {
        RenameOutcome::Renamed(ws) => ws,
        other => panic!("expected Renamed, got {other:?}"),
    };

    // A second summary for the same row — read fresh from the DB, as a host
    // would — is declined without touching git: the slug is no longer a
    // codename, and that is the entire record of "already renamed".
    let fresh = f.workspace_repo.get_by_id(&renamed.id).unwrap().unwrap();
    assert_eq!(
        propose_auto_rename(&fresh, "Something else entirely"),
        Err(Ineligible::NotACodename)
    );
    let branches = branch_names(&f.project_root).await;
    assert!(branches.contains(&"oximux/fix-login-redirect".to_string()));
    assert!(!branches.iter().any(|b| b.contains("something-else")), "{branches:?}");
}

#[tokio::test]
async fn a_pushed_codename_branch_is_refused_by_the_engine() {
    let f = fixture("amber", true).await;
    let remote = tempfile::tempdir().expect("remote tempdir");
    run_git(remote.path(), &["init", "--bare", "-q"]);
    run_git(
        &f.project_root,
        &["remote", "add", "origin", &remote.path().to_string_lossy()],
    );
    run_git(&f.project_root, &["push", "-q", "-u", "origin", "oximux/amber"]);

    let proposal = propose_auto_rename(&f.workspace, "Fix login redirect").unwrap();
    let outcome =
        auto_rename_with_rollback(&f.project_root, &f.workspace, &proposal, &f.workspace_repo)
            .await;
    match outcome {
        RenameOutcome::Refused(RenameRefusal::Pushed { upstream }) => {
            assert_eq!(upstream, "origin/oximux/amber");
        }
        other => panic!("expected Pushed refusal, got {other:?}"),
    }
    let row = f.workspace_repo.get_by_id(&f.workspace.id).unwrap().unwrap();
    assert_eq!(row.slug, "amber", "nothing was touched");
}

#[tokio::test]
async fn a_user_named_or_adopted_row_is_never_proposed() {
    let typed = fixture("fix-login", true).await;
    assert_eq!(
        propose_auto_rename(&typed.workspace, "Auth flow"),
        Err(Ineligible::NotACodename)
    );
    let adopted = fixture("amber", false).await;
    assert_eq!(
        propose_auto_rename(&adopted.workspace, "Auth flow"),
        Err(Ineligible::NotMinted)
    );
}
