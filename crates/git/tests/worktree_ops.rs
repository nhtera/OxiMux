//! Integration tests for worktree operations on `Repository`: `add_worktree`,
//! `list_worktrees`, `remove_worktree`. Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::{GitError, Repository};

#[tokio::test]
async fn list_worktrees_main_only() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let ws = repo.list_worktrees().await.unwrap();
    assert_eq!(ws.len(), 1);
    assert!(ws[0].is_main);
    assert_eq!(ws[0].branch.as_deref(), Some("main"));
    assert!(!ws[0].is_locked);
}

#[tokio::test]
async fn add_worktree_creates_dir_and_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    // Worktree directory must be OUTSIDE the repo root (git refuses nested).
    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("feat-x");

    let repo = Repository::open(p).await.unwrap();
    let info = repo.add_worktree(&wt_path, "oximux/feat-x").await.unwrap();
    assert!(wt_path.exists());
    assert!(!info.is_main);
    assert_eq!(info.branch.as_deref(), Some("oximux/feat-x"));

    let bs = repo.list_branches().await.unwrap();
    assert!(bs.iter().any(|b| b.name == "oximux/feat-x"));
}

#[tokio::test]
async fn add_worktree_appears_in_list() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "oximux/wt-slug").await.unwrap();

    let ws = repo.list_worktrees().await.unwrap();
    assert_eq!(ws.len(), 2);
    let linked = ws.iter().find(|w| !w.is_main).unwrap();
    assert_eq!(linked.branch.as_deref(), Some("oximux/wt-slug"));
}

#[tokio::test]
async fn remove_worktree_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "oximux/remove-clean").await.unwrap();
    repo.remove_worktree(&wt_path, false).await.unwrap();

    let ws = repo.list_worktrees().await.unwrap();
    assert_eq!(ws.len(), 1);
    assert!(!wt_path.exists());
}

#[tokio::test]
async fn remove_worktree_dirty_without_force_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "oximux/dirty-no-force").await.unwrap();
    // Dirty the worktree (modify the checked-out copy of a.txt).
    write(&wt_path.join("a.txt"), "modified\n");

    let err = repo.remove_worktree(&wt_path, false).await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
    assert!(wt_path.exists(), "worktree should still be present");
}

#[tokio::test]
async fn remove_worktree_dirty_with_force_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "oximux/dirty-force").await.unwrap();
    write(&wt_path.join("a.txt"), "modified\n");

    repo.remove_worktree(&wt_path, true).await.unwrap();
    assert!(!wt_path.exists());
}

#[tokio::test]
async fn remove_main_worktree_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let main = repo.workdir().to_path_buf();
    let err = repo.remove_worktree(&main, false).await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn add_worktree_path_already_exists_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");
    std::fs::create_dir(&wt_path).unwrap();
    write(&wt_path.join("blocker"), "exists\n");

    let repo = Repository::open(p).await.unwrap();
    let err = repo.add_worktree(&wt_path, "oximux/exists").await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

/// `add_worktree` now names a *branch*, so `foo/bar` is legal where it used to
/// be rejected — one prefix segment is the whole point of the configurable
/// prefix. What must still be refused, before any side effect, is a name with
/// more structure than a prefix and a slug, or ref syntax in either half.
#[tokio::test]
async fn add_worktree_rejects_an_unusable_branch_name_before_the_git_call() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    for bad in ["a/b/c", "oximux/feat^1", "oximux/../escape", "/feat", "oximux/"] {
        let err = repo.add_worktree(&wt_path, bad).await.unwrap_err();
        assert!(matches!(err, GitError::InvalidInput { .. }), "{bad:?} got {err:?}");
        // Defense-in-depth: validation runs before any side effect.
        assert!(!wt_path.exists(), "{bad:?} created something");
    }

    // ...and the one that used to be rejected is now the ordinary case.
    repo.add_worktree(&wt_path, "nhtera/bar").await.expect("one prefix segment is legal");
    let listed = repo.list_worktrees().await.unwrap();
    assert!(listed.iter().any(|w| w.branch.as_deref() == Some("nhtera/bar")));
}

#[tokio::test]
async fn add_worktree_existing_branch_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    // Pre-create the branch — `worktree add -b` will refuse.
    run_git(p, &["branch", "oximux/already-here"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("wt");

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .add_worktree(&wt_path, "oximux/already-here")
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

/// The spawn path asks this on every chat, so it runs against a worktree git
/// actually created rather than a hand-built directory — the pointer files it
/// reads are git's, and a fixture that guessed their shape would pass while the
/// real thing failed.
#[tokio::test]
async fn a_linked_worktree_resolves_back_to_its_main_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("oximux-wt-feat-x");
    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, "oximux/feat-x").await.unwrap();

    let main = oximux_git::main_worktree_of(&wt_path).expect("a linked worktree resolves");
    assert_eq!(main, p.canonicalize().unwrap());

    // The worktree is a *sibling* of the repo, not a child — which is why
    // containment alone cannot decide this and the lookup has to exist.
    assert!(!wt_path.starts_with(p));
}

#[tokio::test]
async fn a_primary_worktree_is_not_a_linked_one() {
    // `.git` is a directory here, so the question answers itself and no
    // capability is inherited from a repository that is already itself.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    assert_eq!(oximux_git::main_worktree_of(p), None);
}

#[tokio::test]
async fn a_directory_that_is_not_a_repository_resolves_to_nothing() {
    // Fails closed: an uncertain answer withholds the capability.
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(oximux_git::main_worktree_of(tmp.path()), None);
    assert_eq!(
        oximux_git::main_worktree_of(std::path::Path::new("/definitely/not/here")),
        None
    );
}

// ---------------------------------------------------------------------------
// Rename primitives: `move_worktree`, `rename_branch`, `upstream_of`.
// ---------------------------------------------------------------------------

/// Init a repo with one commit and one linked worktree, and return
/// `(repo_root_tempdir, worktree_root_tempdir, worktree_path)`.
async fn repo_with_worktree(
    slug: &str,
) -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join(slug);
    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree(&wt_path, &format!("oximux/{slug}")).await.unwrap();
    (tmp, wt_root, wt_path)
}

#[tokio::test]
async fn move_worktree_relocates_the_directory_and_git_agrees() {
    let (tmp, wt_root, from) = repo_with_worktree("fix-lgoin").await;
    let to = wt_root.path().join("fix-login");

    let repo = Repository::open(tmp.path()).await.unwrap();
    repo.move_worktree(&from, &to).await.unwrap();

    assert!(!from.exists(), "old directory must be gone");
    assert!(to.join("a.txt").exists(), "content must have moved");

    // The point of using `git worktree move` rather than `mv`: git's own
    // bookkeeping follows, so the worktree is still a worktree afterwards.
    let listed = repo.list_worktrees().await.unwrap();
    let moved = listed
        .iter()
        .find(|w| !w.is_main)
        .expect("linked worktree still listed");
    assert_eq!(
        std::fs::canonicalize(&moved.path).unwrap(),
        std::fs::canonicalize(&to).unwrap(),
    );
    assert_eq!(moved.branch.as_deref(), Some("oximux/fix-lgoin"));
}

#[tokio::test]
async fn move_worktree_refuses_an_existing_destination() {
    let (tmp, wt_root, from) = repo_with_worktree("feat-a").await;
    let to = wt_root.path().join("occupied");
    std::fs::create_dir(&to).unwrap();

    let repo = Repository::open(tmp.path()).await.unwrap();
    let err = repo.move_worktree(&from, &to).await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
    // Refused before touching anything.
    assert!(from.join("a.txt").exists(), "source must be untouched");
}

#[tokio::test]
async fn move_worktree_refuses_the_main_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    let dest = tempfile::tempdir().unwrap().path().join("elsewhere");

    let repo = Repository::open(p).await.unwrap();
    let err = repo.move_worktree(p, &dest).await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
    assert!(p.join("a.txt").exists());
}

#[tokio::test]
async fn rename_branch_renames_and_is_reversible() {
    let (tmp, _wt_root, _wt) = repo_with_worktree("fix-lgoin").await;
    let repo = Repository::open(tmp.path()).await.unwrap();

    repo.rename_branch("oximux/fix-lgoin", "oximux/fix-login")
        .await
        .unwrap();
    let names: Vec<String> = repo
        .list_branches()
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(names.iter().any(|n| n == "oximux/fix-login"));
    assert!(
        !names.iter().any(|n| n == "oximux/fix-lgoin"),
        "old name must be gone, not aliased"
    );

    // Reversibility is what lets a rollback walk this step back.
    repo.rename_branch("oximux/fix-login", "oximux/fix-lgoin")
        .await
        .unwrap();
    let names: Vec<String> = repo
        .list_branches()
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(names.iter().any(|n| n == "oximux/fix-lgoin"));
}

#[tokio::test]
async fn rename_branch_refuses_to_overwrite_an_existing_branch() {
    let (tmp, _wt_root, _wt) = repo_with_worktree("feat-a").await;
    let repo = Repository::open(tmp.path()).await.unwrap();
    repo.create_branch("oximux/feat-b", None).await.unwrap();

    let err = repo
        .rename_branch("oximux/feat-a", "oximux/feat-b")
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
    // Both branches survive an attempted collision.
    let names: Vec<String> = repo
        .list_branches()
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(names.iter().any(|n| n == "oximux/feat-a"));
    assert!(names.iter().any(|n| n == "oximux/feat-b"));
}

#[tokio::test]
async fn upstream_of_is_none_for_a_local_only_branch() {
    let (tmp, _wt_root, _wt) = repo_with_worktree("feat-a").await;
    let repo = Repository::open(tmp.path()).await.unwrap();
    // No upstream configured is a normal answer, not an error — this is the
    // only state in which renaming the branch is safe.
    assert_eq!(repo.upstream_of("oximux/feat-a").await.unwrap(), None);
}

#[tokio::test]
async fn upstream_of_names_the_remote_ref_once_the_branch_is_pushed() {
    let (tmp, _wt_root, _wt) = repo_with_worktree("feat-a").await;
    let p = tmp.path();

    // A bare repo on disk stands in for `origin`; no network involved.
    let remote = tempfile::tempdir().unwrap();
    run_git(remote.path(), &["init", "--bare", "-q"]);
    run_git(
        p,
        &["remote", "add", "origin", &remote.path().to_string_lossy()],
    );
    run_git(p, &["push", "-q", "-u", "origin", "oximux/feat-a"]);

    let repo = Repository::open(p).await.unwrap();
    assert_eq!(
        repo.upstream_of("oximux/feat-a").await.unwrap().as_deref(),
        Some("origin/oximux/feat-a"),
    );
}

/// The default branch is what a merge lands in, and it is NOT "main" by
/// convention — a repo initialised on `master`, or a remote that declares
/// something else, has to be read rather than assumed. Getting this wrong makes
/// the merge action refuse with the name of a branch that does not exist.
#[tokio::test]
async fn default_branch_reads_master_when_that_is_what_the_repo_has() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    run_git(root, &["init", "-b", "master"]);
    run_git(root, &["config", "user.name", "Test"]);
    run_git(root, &["config", "user.email", "test@example.com"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.txt"), "v1\n").expect("seed");
    run_git(root, &["add", "a.txt"]);
    run_git(root, &["commit", "-m", "init"]);

    let repo = Repository::open(root).await.expect("open");
    assert_eq!(
        repo.default_branch().await.expect("detect"),
        Some("master".to_string()),
        "a master-default repo must not be reported as main"
    );
}

/// `origin/HEAD` outranks the local-name guess, and its remote prefix is
/// stripped — a merge target is a LOCAL branch name.
#[tokio::test]
async fn default_branch_prefers_what_the_remote_declares() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin.git");
    run_git(tmp.path(), &["init", "--bare", "-b", "trunk", "origin.git"]);

    let work = tmp.path().join("work");
    std::fs::create_dir(&work).expect("mkdir");
    run_git(&work, &["init", "-b", "trunk"]);
    run_git(&work, &["config", "user.name", "Test"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "commit.gpgsign", "false"]);
    std::fs::write(work.join("a.txt"), "v1\n").expect("seed");
    run_git(&work, &["add", "a.txt"]);
    run_git(&work, &["commit", "-m", "init"]);
    run_git(&work, &["remote", "add", "origin", &origin.to_string_lossy()]);
    run_git(&work, &["push", "-u", "origin", "trunk"]);
    run_git(&work, &["remote", "set-head", "origin", "trunk"]);
    // A local `main` exists too, so the local-name fallback would answer wrong.
    run_git(&work, &["branch", "main"]);

    let repo = Repository::open(&work).await.expect("open");
    assert_eq!(
        repo.default_branch().await.expect("detect"),
        Some("trunk".to_string()),
        "origin/HEAD outranks the local main/master guess"
    );
}

/// Nothing resolvable means `None`, never a guess. A caller that receives a
/// name it cannot verify would refuse merges against a branch that is not
/// there.
#[tokio::test]
async fn default_branch_is_none_when_nothing_conventional_resolves() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    run_git(root, &["init", "-b", "trunk"]);
    run_git(root, &["config", "user.name", "Test"]);
    run_git(root, &["config", "user.email", "test@example.com"]);
    run_git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.txt"), "v1\n").expect("seed");
    run_git(root, &["add", "a.txt"]);
    run_git(root, &["commit", "-m", "init"]);

    let repo = Repository::open(root).await.expect("open");
    assert_eq!(repo.default_branch().await.expect("detect"), None);
}

// ---------------------------------------------------------------------------
// Base ref + existing branch (phase 2)
// ---------------------------------------------------------------------------

/// `git <args>` with its stdout captured and trimmed. `common::run_git` only
/// asserts the exit status, and these tests assert on *values* — a merge-base,
/// a branch tip — so they need the output rather than the status.
fn git_out(cwd: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    assert!(out.status.success(), "git {args:?} failed in {cwd:?}");
    String::from_utf8(out.stdout).expect("git printed non-utf8").trim().to_string()
}

/// A repo whose `main` has two commits and whose `side` branch points at the
/// FIRST of them. Basing a worktree on `side` is therefore observably different
/// from basing it on HEAD, which is what makes the merge-base assertions below
/// mean something.
fn repo_with_a_divergent_base(p: &std::path::Path) -> String {
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "first"]);
    let first = git_out(p, &["rev-parse", "HEAD"]);
    run_git(p, &["branch", "side"]);
    write(&p.join("b.txt"), "v2\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "second"]);
    first
}

#[tokio::test]
async fn add_worktree_from_bases_the_new_branch_on_the_named_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let first = repo_with_a_divergent_base(p);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("feat-x");

    let repo = Repository::open(p).await.unwrap();
    let info = repo
        .add_worktree_from(&wt_path, "oximux/feat-x", "side")
        .await
        .unwrap();
    assert_eq!(info.branch.as_deref(), Some("oximux/feat-x"));

    // The new branch's tip IS the first commit — it was cut from `side`, not
    // from the main checkout's HEAD two commits along.
    let tip = git_out(p, &["rev-parse", "oximux/feat-x"]);
    assert_eq!(tip, first, "worktree did not branch from `side`");

    // And the merge-base with HEAD is that same commit, which is the property
    // a reviewer actually reads: the work starts where the user asked.
    let base = git_out(p, &["merge-base", "oximux/feat-x", "main"]);
    assert_eq!(base, first);
}

#[tokio::test]
async fn add_worktree_from_defaults_are_unaffected_by_a_feature_branch_head() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let first = repo_with_a_divergent_base(p);

    // The main checkout sits on a feature branch — the exact state that used to
    // leak into every new worktree.
    run_git(p, &["checkout", "-q", "-b", "wip"]);
    write(&p.join("c.txt"), "wip\n");
    run_git(p, &["add", "c.txt"]);
    run_git(p, &["commit", "-m", "wip work"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("feat-y");
    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree_from(&wt_path, "oximux/feat-y", "side")
        .await
        .unwrap();

    let tip = git_out(p, &["rev-parse", "oximux/feat-y"]);
    assert_eq!(tip, first, "new worktree inherited the wip HEAD");
}

#[tokio::test]
async fn add_worktree_existing_checks_out_without_creating_a_prefixed_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    repo_with_a_divergent_base(p);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("adopted");

    let repo = Repository::open(p).await.unwrap();
    let info = repo.add_worktree_existing(&wt_path, "side").await.unwrap();
    assert_eq!(info.branch.as_deref(), Some("side"));

    // No `oximux/`-prefixed branch was minted anywhere: the worktree adopted
    // the branch under the name it already had.
    let branches = repo.list_branches().await.unwrap();
    assert!(
        !branches.iter().any(|b| b.name.starts_with("oximux/")),
        "existing-branch mode minted a prefixed branch: {branches:?}"
    );
}

#[tokio::test]
async fn add_worktree_existing_surfaces_gits_own_already_checked_out_message() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    repo_with_a_divergent_base(p);

    let wt_root = tempfile::tempdir().unwrap();
    let repo = Repository::open(p).await.unwrap();
    repo.add_worktree_existing(&wt_root.path().join("first"), "side")
        .await
        .unwrap();

    // `side` is now live in another worktree. Git refuses, and its refusal
    // names the worktree holding it — the detail the user needs to act.
    let err = repo
        .add_worktree_existing(&wt_root.path().join("second"), "side")
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("already used by worktree") || text.contains("already checked out"),
        "git's own reason did not survive: {text}"
    );
}

#[tokio::test]
async fn base_ref_arguments_shaped_like_flags_are_refused_before_git_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    repo_with_a_divergent_base(p);
    let wt_root = tempfile::tempdir().unwrap();
    let repo = Repository::open(p).await.unwrap();

    // Every one of these is refused by `validate_ref_name`, not by git — the
    // worktree directory is never created, which is the observable difference
    // between "rejected" and "git happened to fail".
    for (i, bad) in [
        "-B main",
        "--force",
        "-f",
        "main..side",
        "main@{1}",
        "side~1",
        "side^",
        "refs/heads/side:x",
        "feature/.hidden",
        "feature/x.lock",
        "side branch",
        "a//b",
        "",
        "@",
    ]
    .into_iter()
    .enumerate()
    {
        // Keyed by index, not by length: `-B main` and `--force` are both 7
        // characters, and a shared path would make a regression name the wrong
        // input in the failure message.
        let wt_path = wt_root.path().join(format!("wt-{i}"));
        let err = repo
            .add_worktree_from(&wt_path, "oximux/probe", bad)
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::InvalidInput { .. }),
            "start_point {bad:?} reached git instead of being refused: {err}"
        );
        assert!(!wt_path.exists(), "start_point {bad:?} created a directory");

        let err = repo.add_worktree_existing(&wt_path, bad).await.unwrap_err();
        assert!(
            matches!(err, GitError::InvalidInput { .. }),
            "branch {bad:?} reached git instead of being refused: {err}"
        );
        assert!(!wt_path.exists(), "branch {bad:?} created a directory");
    }
}

#[tokio::test]
async fn a_multi_segment_ref_is_accepted_where_a_minted_branch_name_would_not_be() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    repo_with_a_divergent_base(p);
    run_git(p, &["branch", "feature/api/retry", "side"]);

    let wt_root = tempfile::tempdir().unwrap();
    let repo = Repository::open(p).await.unwrap();

    // `validate_branch_name` would refuse this (more than one prefix segment),
    // and refusing it here would refuse the feature: an adopted branch is
    // somebody else's name.
    let info = repo
        .add_worktree_existing(&wt_root.path().join("adopted"), "feature/api/retry")
        .await
        .unwrap();
    assert_eq!(info.branch.as_deref(), Some("feature/api/retry"));
}

#[tokio::test]
async fn add_worktree_from_rejects_a_ref_that_does_not_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    repo_with_a_divergent_base(p);
    let wt_root = tempfile::tempdir().unwrap();
    let repo = Repository::open(p).await.unwrap();

    // Well-formed, so it passes `validate_ref_name` — and is then refused by
    // the SHA resolution, before `git worktree add` runs. That ordering is not
    // incidental: resolving first is what stops git's DWIM from reinterpreting
    // an unresolvable-looking name as a remote-tracking branch (see the
    // regression tests below).
    let err = repo
        .add_worktree_from(&wt_root.path().join("nope"), "oximux/nope", "no-such-ref")
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitError::InvalidInput { .. }),
        "an unresolvable start point must be refused, not handed to git: {err}"
    );
    assert!(
        err.to_string().contains("no-such-ref"),
        "the refusal must name the ref: {err}"
    );
}

// ---------------------------------------------------------------------------
// `git worktree add`'s DWIM overrides an explicit `-b` (regression, phase 2)
// ---------------------------------------------------------------------------

/// A repo whose `main` exists **only** as `origin/main` — the state a
/// worktree-centric user is in after deleting the local default branch they
/// never sit on. This is the shape that makes git's DWIM fire.
fn repo_whose_default_is_remote_only(root: &std::path::Path) -> std::path::PathBuf {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "-q", "--bare"]);

    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    init_repo(&work);
    write(&work.join("a.txt"), "v1\n");
    run_git(&work, &["add", "a.txt"]);
    run_git(&work, &["commit", "-m", "init"]);
    run_git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
    run_git(&work, &["push", "-q", "origin", "main"]);
    // Move off `main`, delete it locally, and point `origin/HEAD` at it — so
    // `default_branch()` reports "main" while `refs/heads/main` is absent.
    run_git(&work, &["checkout", "-q", "-b", "dev"]);
    run_git(&work, &["branch", "-D", "main"]);
    run_git(&work, &["remote", "set-head", "origin", "main"]);
    work
}

/// **The regression.** `git worktree add -b <new> -- <path> <start>` DWIMs a
/// start point that names no local branch but exactly one remote-tracking
/// branch into `--track -b <that name>` — and the DWIM **beats the explicit
/// `-b`**. Before the SHA resolution this created `main`, left `oximux/feat`
/// non-existent, and reported success, so the `workspaces` row named a branch
/// that was never made.
///
/// Resolving the start point first closes it from both sides: a name git can
/// resolve is passed as a SHA (no ambiguity for the DWIM to key on), and a name
/// it cannot — which is what a remote-only `main` is to `rev-parse` — is
/// refused outright instead of being reinterpreted. The create path turns that
/// refusal into "base on HEAD"; see
/// `apps/desktop/tests/workspace_create_rollback.rs`.
#[tokio::test]
async fn a_remote_only_start_point_cannot_hijack_the_branch_name() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo_whose_default_is_remote_only(tmp.path());
    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("feat");

    let repo = Repository::open(&work).await.unwrap();
    let err = repo
        .add_worktree_from(&wt_path, "oximux/feat", "main")
        .await
        .unwrap_err();
    assert!(
        matches!(err, GitError::InvalidInput { .. }),
        "a remote-only start point must be refused, not DWIMed into a branch name: {err}"
    );
    // Nothing was created under either name — in particular git did NOT mint
    // the `main` its DWIM wanted.
    assert!(!wt_path.exists());
    let branches = repo.list_branches().await.unwrap();
    assert!(
        !branches.iter().any(|b| b.name == "main" || b.name == "oximux/feat"),
        "a branch was created behind our back: {branches:?}"
    );
}

/// And the way the user actually gets what they meant: name the
/// remote-tracking ref, which resolves — the new branch is theirs, based on the
/// remote's tip, with no DWIM in sight.
#[tokio::test]
async fn the_remote_tracking_form_bases_correctly_and_keeps_our_branch_name() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo_whose_default_is_remote_only(tmp.path());
    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("feat");

    let repo = Repository::open(&work).await.unwrap();
    let info = repo
        .add_worktree_from(&wt_path, "oximux/feat", "origin/main")
        .await
        .unwrap();
    assert_eq!(info.branch.as_deref(), Some("oximux/feat"));
    assert_eq!(
        git_out(&work, &["rev-parse", "oximux/feat"]),
        git_out(&work, &["rev-parse", "origin/main"])
    );
    let branches = repo.list_branches().await.unwrap();
    assert!(!branches.iter().any(|b| b.name == "main"), "{branches:?}");
}

/// The sibling: existing-branch mode handed a remote-only name used to
/// **create** a local branch — while `CreateBase::ExistingBranch` tells the
/// rollback "this branch already existed, leave it alone", so every failed
/// adopt-create left an orphan behind.
#[tokio::test]
async fn adopting_refuses_a_branch_that_exists_only_on_the_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo_whose_default_is_remote_only(tmp.path());
    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("adopted");

    let repo = Repository::open(&work).await.unwrap();
    let err = repo.add_worktree_existing(&wt_path, "main").await.unwrap_err();
    assert!(
        matches!(err, GitError::InvalidInput { .. }),
        "a remote-only branch must be refused, not silently created: {err}"
    );
    assert!(err.to_string().contains("--from"), "the refusal names the way out: {err}");
    assert!(!wt_path.exists());
    let branches = repo.list_branches().await.unwrap();
    assert!(!branches.iter().any(|b| b.name == "main"), "{branches:?}");
}

/// The other sibling: a tag resolves, so it passed the old check — and
/// produced a **detached HEAD** whose row claimed a branch the worktree was
/// not on.
#[tokio::test]
async fn adopting_refuses_a_tag() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    repo_with_a_divergent_base(p);
    run_git(p, &["tag", "v1.0", "side"]);

    let wt_root = tempfile::tempdir().unwrap();
    let wt_path = wt_root.path().join("tagged");
    let repo = Repository::open(p).await.unwrap();

    let err = repo.add_worktree_existing(&wt_path, "v1.0").await.unwrap_err();
    assert!(
        matches!(err, GitError::InvalidInput { .. }),
        "a tag must be refused: it detaches HEAD and the row would lie: {err}"
    );
    assert!(!wt_path.exists());

    // `--from` is the documented way to do what the user probably meant, and
    // it still works on the same tag.
    repo.add_worktree_from(&wt_path, "oximux/from-tag", "v1.0")
        .await
        .expect("--from accepts a tag");
    assert_eq!(
        git_out(p, &["rev-parse", "oximux/from-tag"]),
        git_out(p, &["rev-parse", "v1.0^{commit}"])
    );
}

/// **What live verification caught.** `default_branch()` reports the name
/// behind `origin/HEAD`, which on a worktree-centric checkout has no local
/// ref — the user deleted the `main` they never sit on. An earlier fix made
/// `default_branch()` return `None` in that case, which threw the usable answer
/// away: the create path then based new work on whatever branch the checkout
/// was parked on, reintroducing the exact defect base refs exist to close.
///
/// So the name is still reported, and resolving it is the caller's job. Every
/// unit test before this had a local `main`, which is why only driving the real
/// binaries found it.
#[tokio::test]
async fn default_branch_still_names_a_default_that_lives_only_on_the_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo_whose_default_is_remote_only(tmp.path());
    let repo = Repository::open(&work).await.unwrap();

    assert_eq!(
        repo.default_branch().await.unwrap().as_deref(),
        Some("main"),
        "the default branch is `main`; it just lives at origin/main"
    );
    // The bare name does not resolve — which is exactly why a caller that needs
    // something checkoutable has to try the remote-tracking spelling.
    assert!(repo.sha_of("main").await.unwrap().is_none());
    assert!(repo.sha_of("origin/main").await.unwrap().is_some());
}
