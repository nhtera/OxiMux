//! Integration tests for Phase 8's power verbs: branch from a stash, restore
//! one file out of one, and read every file a stash touches.
//!
//! Kept out of `stash_ops.rs` (already past 1000 lines) and out of
//! `stash_partial.rs` (which is the path-scoped-push story) for the same
//! reason: these are a self-contained set of claims about three commands that
//! behave differently from the rest of `git stash`.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::Repository;
use std::path::Path;

/// `git status --porcelain=v1`, sorted, XY columns included. The index column
/// is the point for restore: `git checkout <sha> -- <path>` writes the index
/// too, and a name-only listing would not show it.
fn status(cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

fn branches(cwd: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines.sort();
    lines
}

fn head_branch(cwd: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Sorted paths of the `FileDiff`s a query returned.
fn paths_of(diffs: &[oximux_core::FileDiff]) -> Vec<String> {
    let mut out: Vec<String> = diffs
        .iter()
        .map(|d| d.path.to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// A repo with `a.txt` and `keep.txt` committed.
fn seed(p: &Path) {
    init_repo(p);
    write(&p.join("a.txt"), "a\n");
    write(&p.join("keep.txt"), "k\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
}

// ── stash_all_files ──────────────────────────────────────────────────────

#[tokio::test]
async fn stash_all_files_includes_the_untracked_side_that_commit_files_drops() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "a2\n");
    write(&p.join("fresh.txt"), "new\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("with-u"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    // The whole reason `stash_all_files` exists. `commit_files` is
    // `--first-parent`, so on a stash commit it reports the tracked side and
    // is silent about the untracked one — which the expanded row DOES list.
    // Pinning both halves here means a future "simplification" back to one
    // call fails loudly instead of quietly hiding files.
    assert_eq!(
        paths_of(&repo.commit_files(&sha).await.unwrap()),
        vec!["a.txt".to_string()],
        "commit_files must keep reporting the tracked side only",
    );
    assert_eq!(
        paths_of(&repo.stash_all_files(&sha).await.unwrap()),
        vec!["a.txt".to_string(), "fresh.txt".to_string()],
    );
}

#[tokio::test]
async fn stash_all_files_on_a_stash_without_untracked_files_is_not_an_error() {
    // A stash pushed without `-u` has no `^3` at all and `git show` hard-errors
    // on it. That is the normal shape, not a failure — see
    // `untracked_commit_files`.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "a2\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("no-u"), false, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    assert_eq!(
        paths_of(&repo.stash_all_files(&sha).await.unwrap()),
        vec!["a.txt".to_string()],
    );
}

#[tokio::test]
async fn an_untracked_file_arrives_as_a_whole_file_addition() {
    // `^3` is a ROOT commit, so `git show -p` on it emits a `new file mode`
    // diff per path. That is what makes reusing `commit_files` on it correct
    // rather than merely convenient — the parser sees the shape it expects.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("fresh.txt"), "one\ntwo\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("u-only"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    let diffs = repo.stash_all_files(&sha).await.unwrap();
    let fresh = diffs
        .iter()
        .find(|d| d.path.ends_with("fresh.txt"))
        .expect("fresh.txt in the stash");
    assert!(
        matches!(fresh.status, oximux_core::DiffStatus::Added),
        "expected an addition, got {:?}",
        fresh.status
    );
}

// ── is_valid_branch_name ─────────────────────────────────────────────────

#[tokio::test]
async fn branch_name_validation_defers_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    let repo = Repository::open(p).await.unwrap();

    for good in ["fix-parser", "feature/thing", "a.b", "wip2"] {
        assert!(repo.is_valid_branch_name(good).await, "{good} should pass");
    }
    for bad in ["", "has space", "x..y", "a@{b}", "ends.lock", "/leading"] {
        assert!(!repo.is_valid_branch_name(bad).await, "{bad} should fail");
    }
}

#[tokio::test]
async fn a_leading_dash_is_rejected_without_asking_git() {
    // `check-ref-format --branch --help` exits 0 and prints help, so deferring
    // to git here would "validate" `--help` and then make the very command
    // being guarded print help instead of branching. Rejected locally.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    let repo = Repository::open(p).await.unwrap();

    assert!(!repo.is_valid_branch_name("--help").await);
    assert!(!repo.is_valid_branch_name("-f").await);
}

// ── stash_branch ─────────────────────────────────────────────────────────

#[tokio::test]
async fn branching_from_a_stash_consumes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "a2\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    assert!(status(p).is_empty(), "the push should have cleaned the tree");

    let stash_ref = oximux_core::StashRef { index: 0 };
    repo.stash_branch("fix-parser", &stash_ref).await.unwrap();

    assert_eq!(head_branch(p), "fix-parser");
    assert!(branches(p).contains(&"fix-parser".to_string()));
    // The surprise the dialog copy exists to disclose.
    assert!(
        repo.stash_list(true).await.unwrap().is_empty(),
        "stash branch drops the stash on success",
    );
    // Measured, not assumed: `stash branch` restores the staged/unstaged
    // SPLIT the stash was taken with — it applies with `--index`, and it can
    // do so safely because the branch it just created is at the stash's base
    // commit, so there is never an index to conflict with. This change was
    // unstaged when stashed, so it comes back unstaged. The first draft of
    // this test asserted `M  ` and was wrong.
    assert_eq!(
        status(p),
        vec![" M a.txt".to_string()],
        "the stashed change should be back on the new branch, unstaged as it was",
    );
}

#[tokio::test]
async fn branching_from_a_stash_preserves_the_staged_side_too() {
    // The other half of the split above: a change that was STAGED when
    // stashed comes back staged. Pinned separately so a future switch away
    // from `--index` semantics cannot pass by only half-mattering.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "a2\n");
    run_git(p, &["add", "a.txt"]);

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("staged-wip"), false, &[]).await.unwrap();
    repo.stash_branch("fix-parser", &oximux_core::StashRef { index: 0 })
        .await
        .unwrap();

    assert_eq!(status(p), vec!["M  a.txt".to_string()]);
}

#[tokio::test]
async fn a_name_collision_fails_before_touching_anything() {
    // The one clean refusal: git checks the name first, so nothing moves.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    run_git(p, &["branch", "taken"]);
    write(&p.join("a.txt"), "a2\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    let before = head_branch(p);

    let err = repo
        .stash_branch("taken", &oximux_core::StashRef { index: 0 })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "got: {err}");
    assert_eq!(head_branch(p), before, "HEAD must not have moved");
    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        1,
        "the stash must survive a refused branch",
    );
}

// ── stash_restore_file ───────────────────────────────────────────────────

#[tokio::test]
async fn restoring_one_file_leaves_the_stash_and_every_other_file_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "stashed-a\n");
    write(&p.join("keep.txt"), "stashed-keep\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    // Diverge both files AFTER the stash so a restore of one is visible and a
    // stray restore of the other would be too.
    write(&p.join("a.txt"), "local-a\n");
    write(&p.join("keep.txt"), "local-keep\n");

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    repo.stash_restore_file(&sha, Path::new("a.txt"))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "stashed-a\n",
    );
    assert_eq!(
        std::fs::read_to_string(p.join("keep.txt")).unwrap(),
        "local-keep\n",
        "the unnamed file must not have moved",
    );
    // The index column is why this assertion reads XY and not just names:
    // `checkout <sha> -- <path>` stages what it restores, which the confirm
    // dialog has to say out loud or the user commits it by accident.
    assert_eq!(
        status(p),
        vec!["M  a.txt".to_string(), " M keep.txt".to_string()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        1,
        "restore copies out; it must not consume the stash",
    );
}

#[tokio::test]
async fn a_bracketed_path_does_not_restore_its_glob_sibling() {
    // The hazard `run_pathspec_op` exists for: a bare `git checkout <sha> --
    // 'a[1].txt'` also overwrites `a1.txt`, a file the user never named and
    // never confirmed.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a[1].txt"), "bracket\n");
    write(&p.join("a1.txt"), "sibling\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
    write(&p.join("a[1].txt"), "stashed-bracket\n");
    write(&p.join("a1.txt"), "stashed-sibling\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    write(&p.join("a1.txt"), "local-sibling\n");

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    repo.stash_restore_file(&sha, Path::new("a[1].txt"))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(p.join("a[1].txt")).unwrap(),
        "stashed-bracket\n",
    );
    assert_eq!(
        std::fs::read_to_string(p.join("a1.txt")).unwrap(),
        "local-sibling\n",
        "the glob-looking sibling must NOT have been overwritten",
    );
}

#[tokio::test]
async fn restoring_an_untracked_file_fails_which_is_why_the_menu_omits_it() {
    // An untracked file lives in the parentless `^3`, not in the stash
    // commit's tree, so this can only ever fail. Recorded as a test because it
    // is the whole justification for gating the menu item on origin rather
    // than offering a confirm dialog that leads nowhere.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("fresh.txt"), "new\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("u"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    // It IS listed by `stash_files`, which is exactly how a user reaches the
    // row that must not offer Restore.
    assert!(
        repo.stash_files(&sha)
            .await
            .unwrap()
            .iter()
            .any(|f| f.path.ends_with("fresh.txt")),
    );
    let err = repo
        .stash_restore_file(&sha, Path::new("fresh.txt"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("did not match any file"),
        "expected a pathspec error, got: {err}"
    );
}
