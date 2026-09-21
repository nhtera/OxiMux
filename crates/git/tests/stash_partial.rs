//! Integration tests for a PATH-SCOPED `git stash push` — the "stash the
//! files I selected" flow.
//!
//! Kept out of `stash_ops.rs` because that file is already past 1000 lines and
//! these cases are a self-contained story: what a pathspec can and cannot
//! reach, and the one sequence that gets a staged rename into a stash without
//! corrupting the index.
//!
//! The app-side planner that decides which paths and flags to hand these APIs
//! lives in `oximux_app::shell::git_panel::stash_selection`, with its own unit
//! tests; these prove the git behaviour that planner is written against.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::Repository;
use std::path::Path;

/// `git status --porcelain=v1`, sorted, one entry per line including the XY
/// columns. The XY columns are the point: a corrupted rename shows up as a
/// stray `D ` in the index column or a `DU`/`AA` unmerged pair, neither of
/// which a name-only listing would reveal.
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

/// Every path in a revision's tree, sorted. Used for the stash's `^3` — the
/// untracked commit — which is PARENTLESS, so diffing it against `^1` reports
/// the whole tree as deleted and tells you nothing.
fn tree_paths(cwd: &Path, rev: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", rev])
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

/// Name-status of one revision range, sorted — what actually landed in the
/// stash commit.
fn diff_name_status(cwd: &Path, from: &str, to: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-status", from, to])
        .current_dir(cwd)
        .output()
        .expect("git not on PATH");
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.replace('\t', " "))
        .collect();
    lines.sort();
    lines
}

/// A repo with `a.txt`, `c.txt` and `keep.txt` committed.
fn seed(p: &Path) {
    init_repo(p);
    write(&p.join("a.txt"), "a\n");
    write(&p.join("c.txt"), "c\n");
    write(&p.join("keep.txt"), "k\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
}

#[tokio::test]
async fn a_path_scoped_push_leaves_unselected_files_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "a2\n");
    write(&p.join("c.txt"), "c2\n");
    write(&p.join("keep.txt"), "k2\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(
        Some("subset"),
        false,
        &[Path::new("a.txt"), Path::new("c.txt")],
    )
    .await
    .unwrap();

    assert_eq!(
        status(p),
        vec![" M keep.txt".to_string()],
        "only the unselected file should still be dirty"
    );
    assert_eq!(
        diff_name_status(p, "stash@{0}^1", "stash@{0}"),
        vec!["M a.txt".to_string(), "M c.txt".to_string()],
    );
}

#[tokio::test]
async fn a_path_scoped_push_carries_the_staged_unstaged_split() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    // a.txt unstaged, c.txt staged — the split that has to survive.
    write(&p.join("a.txt"), "a2\n");
    write(&p.join("c.txt"), "c2\n");
    run_git(p, &["add", "c.txt"]);

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(
        Some("split"),
        false,
        &[Path::new("a.txt"), Path::new("c.txt")],
    )
    .await
    .unwrap();

    // `^2` is the index commit the stash was pushed with. Only the staged
    // side is in it — that difference is what `stash_apply(.., true)` replays.
    assert_eq!(
        diff_name_status(p, "stash@{0}^1", "stash@{0}^2"),
        vec!["M c.txt".to_string()],
        "the index commit should hold the staged side only"
    );
    assert_eq!(
        diff_name_status(p, "stash@{0}^1", "stash@{0}"),
        vec!["M a.txt".to_string(), "M c.txt".to_string()],
    );
}

#[tokio::test]
async fn an_untracked_path_in_the_selection_needs_the_untracked_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "a2\n");
    write(&p.join("fresh.txt"), "new\n");

    let repo = Repository::open(p).await.unwrap();
    let selection = [Path::new("a.txt"), Path::new("fresh.txt")];

    // Measured on git 2.55.0: naming an untracked path WITHOUT `-u` is not a
    // silent skip — git refuses the pathspec outright and stashes nothing, so
    // the tracked file in the same selection does not move either. Same
    // failure shape as naming a staged rename's original path. That makes the
    // planner's auto-`-u` rule a correctness requirement, not just a guard
    // against losing the untracked file quietly.
    let err = repo
        .stash_push(Some("no-u"), false, &selection)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("did not match any file"),
        "expected a pathspec error, got: {err}"
    );
    assert!(
        repo.stash_list(true).await.unwrap().is_empty(),
        "nothing should have been stashed"
    );
    assert_eq!(
        status(p),
        vec![" M a.txt".to_string(), "?? fresh.txt".to_string()],
        "the worktree must be untouched by the refused push"
    );

    // With `-u` both land: the tracked side in the stash commit, the
    // untracked one in its parentless `^3`.
    repo.stash_push(Some("with-u"), true, &selection)
        .await
        .unwrap();
    assert!(
        status(p).is_empty(),
        "with -u both selected paths leave the worktree, got {:?}",
        status(p)
    );
    assert_eq!(
        diff_name_status(p, "stash@{0}^1", "stash@{0}"),
        vec!["M a.txt".to_string()],
    );
    assert_eq!(tree_paths(p, "stash@{0}^3"), vec!["fresh.txt".to_string()]);
}

#[tokio::test]
async fn a_bracketed_filename_is_matched_literally_not_as_a_glob() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // `new[1].txt` as a glob would also match `new1.txt`.
    write(&p.join("new[1].txt"), "bracket\n");
    write(&p.join("new1.txt"), "sibling\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
    write(&p.join("new[1].txt"), "bracket2\n");
    write(&p.join("new1.txt"), "sibling2\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("literal"), false, &[Path::new("new[1].txt")])
        .await
        .unwrap();

    assert_eq!(
        status(p),
        vec![" M new1.txt".to_string()],
        "the glob-looking sibling must NOT have been swept into the stash"
    );
    assert_eq!(
        diff_name_status(p, "stash@{0}^1", "stash@{0}"),
        vec!["M new[1].txt".to_string()],
    );
}

#[tokio::test]
async fn naming_a_staged_renames_original_path_fails_and_stashes_nothing() {
    // The negative control for the fix that looks obvious and is not: adding
    // the rename's counterpart to the pathspec. After `git mv`, `a.txt` is in
    // HEAD alone — no index entry, no worktree file — so the pathspec matches
    // nothing, git exits non-zero, and the OTHER selected files are not
    // stashed either. Recorded as a test so it is never re-litigated from
    // intuition.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    run_git(p, &["mv", "a.txt", "b.txt"]);
    write(&p.join("c.txt"), "c2\n");

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .stash_push(
            Some("naive"),
            false,
            &[Path::new("a.txt"), Path::new("b.txt"), Path::new("c.txt")],
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("did not match any file"),
        "expected a pathspec error, got: {msg}"
    );
    assert!(
        repo.stash_list(true).await.unwrap().is_empty(),
        "nothing should have been stashed — including the innocent c.txt"
    );
    assert!(
        status(p).iter().any(|l| l.starts_with("R  ")),
        "the rename should still be staged, got {:?}",
        status(p)
    );
}

#[tokio::test]
async fn unstaging_a_rename_first_stashes_it_cleanly() {
    // The verified sequence: unstage the pair, which turns the rename into a
    // worktree deletion plus an untracked file, then push both with -u.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    run_git(p, &["mv", "a.txt", "b.txt"]);
    write(&p.join("c.txt"), "c2\n");
    run_git(p, &["add", "c.txt"]);
    write(&p.join("keep.txt"), "k2\n");

    let repo = Repository::open(p).await.unwrap();
    let pair = [Path::new("a.txt"), Path::new("b.txt")];
    repo.unstage_paths(&pair).await.unwrap();
    repo.stash_push(
        Some("rename"),
        true,
        &[Path::new("a.txt"), Path::new("b.txt"), Path::new("c.txt")],
    )
    .await
    .unwrap();

    // The unselected file is untouched, and nothing is left behind: no
    // orphaned `D ` in the index column, no unmerged pair.
    assert_eq!(status(p), vec![" M keep.txt".to_string()]);
    assert_eq!(
        diff_name_status(p, "stash@{0}^1", "stash@{0}"),
        vec!["D a.txt".to_string(), "M c.txt".to_string()],
    );
    assert_eq!(
        tree_paths(p, "stash@{0}^3"),
        vec!["b.txt".to_string()],
        "the rename's target rides in as an untracked file"
    );

    repo.stash_pop(&oximux_core::StashRef { index: 0 })
        .await
        .unwrap();
    let after = status(p);
    assert!(
        after.contains(&" D a.txt".to_string()) && after.contains(&"?? b.txt".to_string()),
        "the rename should come back as a deletion + an untracked file, got {after:?}"
    );
    assert!(
        !after.iter().any(|l| l.starts_with("DU")
            || l.starts_with("UD")
            || l.starts_with("AA")
            || l.starts_with("UU")),
        "no path may come back unmerged, got {after:?}"
    );
    assert_eq!(
        std::fs::read_to_string(p.join("b.txt")).unwrap(),
        "a\n",
        "the renamed file's content must survive the round trip"
    );
}

#[tokio::test]
async fn restaging_the_pair_undoes_the_unstage_when_the_push_fails() {
    // The rollback the app runs when `unstage_paths` succeeds and the push
    // then fails: `stage_paths` on both sides restores the staged rename, so
    // the user is not left holding a half-unstaged worktree.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    run_git(p, &["mv", "a.txt", "b.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let pair = [Path::new("a.txt"), Path::new("b.txt")];
    repo.unstage_paths(&pair).await.unwrap();
    assert_eq!(
        status(p),
        vec![" D a.txt".to_string(), "?? b.txt".to_string()],
    );

    repo.stage_paths(&pair).await.unwrap();
    assert_eq!(
        status(p),
        vec!["R  a.txt -> b.txt".to_string()],
        "re-staging both sides lets git re-detect the rename"
    );
}
