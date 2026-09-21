//! Integration tests for `Repository::stash_apply_file` — bringing ONE file
//! out of a stash as a patch.
//!
//! Its own file rather than another section of `stash_power_verbs.rs`,
//! because what has to be proved here is the opposite of what that file
//! proves about restore. Restore is destructive and its tests pin *what it
//! overwrites*; apply is offered without a confirm dialog and its tests have
//! to pin *that a refusal costs nothing* — same file, same stash, two
//! contracts that must not drift into each other.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::StashFileOrigin;
use oximux_git::Repository;
use std::path::Path;

/// `git status --porcelain=v1`, sorted. The index column is the point: apply
/// writes the worktree and must leave the index alone, which a name-only
/// listing would not show.
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

fn read(p: &Path, name: &str) -> String {
    std::fs::read_to_string(p.join(name)).expect("read file")
}

/// A repo with `a.txt` and `keep.txt` committed.
fn seed(p: &Path) {
    init_repo(p);
    write(&p.join("a.txt"), "a\n");
    write(&p.join("keep.txt"), "k\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
}

#[tokio::test]
async fn applying_one_file_writes_the_worktree_only_and_keeps_the_stash() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "stashed-a\n");
    write(&p.join("keep.txt"), "stashed-keep\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    repo.stash_apply_file(&sha, Path::new("a.txt"), StashFileOrigin::Tracked)
        .await
        .unwrap();

    assert_eq!(read(p, "a.txt"), "stashed-a\n");
    assert_eq!(read(p, "keep.txt"), "k\n", "the unnamed file must not move");
    // ` M` — worktree modified, index clean. This is the line that separates
    // apply from restore, which stages what it writes (`M  a.txt`).
    assert_eq!(status(p), vec![" M a.txt".to_string()]);
    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        1,
        "apply copies out; it must not consume the stash",
    );
}

#[tokio::test]
async fn a_local_edit_that_conflicts_is_refused_without_touching_the_file() {
    // The claim that lets this verb ship with no confirm dialog: a plain
    // `git apply` is atomic, so a refusal leaves the worktree byte-identical.
    // With `--3way` this same case would write conflict markers INTO the
    // file and still report failure.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "stashed-a\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    write(&p.join("a.txt"), "local-a\n");

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let err = repo
        .stash_apply_file(&sha, Path::new("a.txt"), StashFileOrigin::Tracked)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("patch does not apply"), "got: {err}");
    assert_eq!(read(p, "a.txt"), "local-a\n", "a refusal must cost nothing");
    assert!(
        !p.join("a.txt.rej").exists(),
        "no `.rej` litter — apply runs without --reject",
    );
}

#[tokio::test]
async fn an_untracked_file_applies_where_restore_can_only_fail() {
    // The asymmetry with `stash_restore_file`, which errors on this exact
    // row: an untracked file is in the parentless `^3`, not in the stash
    // commit's tree, so apply reads the patch from there instead.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("fresh.txt"), "new\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("u"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    assert!(!p.join("fresh.txt").exists(), "push -u took the file away");

    repo.stash_apply_file(&sha, Path::new("fresh.txt"), StashFileOrigin::Untracked)
        .await
        .unwrap();

    assert_eq!(read(p, "fresh.txt"), "new\n");
    assert_eq!(status(p), vec!["?? fresh.txt".to_string()], "still untracked");
}

#[tokio::test]
async fn an_untracked_file_whose_path_is_occupied_is_refused_cleanly() {
    // The patch is a creation, so git checks the path before writing: the
    // local copy survives verbatim rather than being silently replaced.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("fresh.txt"), "new\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("u"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    write(&p.join("fresh.txt"), "mine\n");

    let err = repo
        .stash_apply_file(&sha, Path::new("fresh.txt"), StashFileOrigin::Untracked)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("already exists"), "got: {err}");
    assert_eq!(read(p, "fresh.txt"), "mine\n");
}

#[tokio::test]
async fn a_bracketed_path_does_not_apply_over_its_glob_sibling() {
    // Same hazard `stash_restore_file` routes around: a bare pathspec makes
    // `a[1].txt` also select `a1.txt`, a file the user never named.
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

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    repo.stash_apply_file(&sha, Path::new("a[1].txt"), StashFileOrigin::Tracked)
        .await
        .unwrap();

    assert_eq!(read(p, "a[1].txt"), "stashed-bracket\n");
    assert_eq!(
        read(p, "a1.txt"),
        "sibling\n",
        "the glob-looking sibling must not have been written",
    );
}

#[tokio::test]
async fn a_rename_arrives_as_the_new_path_and_leaves_the_old_one_alone() {
    // `--no-renames` is why. With rename detection on, this patch would carry
    // a `rename from`/`rename to` pair and applying it would DELETE
    // `old.txt` — a path the user never named and cannot see in the row they
    // right-clicked.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("old.txt"), "the quick brown fox\njumps over\nthe lazy dog\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
    run_git(p, &["mv", "old.txt", "new.txt"]);

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("renamed"), false, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    repo.stash_apply_file(&sha, Path::new("new.txt"), StashFileOrigin::Tracked)
        .await
        .unwrap();

    assert_eq!(read(p, "new.txt"), "the quick brown fox\njumps over\nthe lazy dog\n");
    assert!(
        p.join("old.txt").exists(),
        "the path the user did not name must survive",
    );
}

#[tokio::test]
async fn a_binary_file_applies_rather_than_being_refused_as_a_placeholder() {
    // `--binary` is why. Without it git emits `Binary files ... differ`,
    // which is not a patch and which `git apply` rejects outright.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    std::fs::write(p.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "base"]);
    std::fs::write(p.join("blob.bin"), [9u8, 8, 7, 0, 6]).unwrap();

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("bin"), false, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    repo.stash_apply_file(&sha, Path::new("blob.bin"), StashFileOrigin::Tracked)
        .await
        .unwrap();

    assert_eq!(std::fs::read(p.join("blob.bin")).unwrap(), [9u8, 8, 7, 0, 6]);
}

#[tokio::test]
async fn a_path_the_stash_does_not_touch_is_reported_not_silently_ignored() {
    // `git apply` exits 0 on empty input, so without the explicit check this
    // would toast success having done nothing at all.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    seed(p);
    write(&p.join("a.txt"), "stashed-a\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("wip"), false, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    let err = repo
        .stash_apply_file(&sha, Path::new("keep.txt"), StashFileOrigin::Tracked)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("records no change to keep.txt"), "got: {err}");
}
