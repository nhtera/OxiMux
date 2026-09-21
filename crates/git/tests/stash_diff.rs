//! How a file inside a stash gets diffed — the routing that decides whether
//! clicking an expanded stash row shows a patch or a blank tab.
//!
//! Two shapes, and they are not interchangeable:
//!
//! * a **tracked** file is the range `<sha>^ .. <sha>`;
//! * an **untracked** file is absent from the stash commit's own tree, so
//!   that range is empty and it must be read from the parentless `<sha>^3`
//!   with no base at all ([`Repository::diff_in_rev`]).
//!
//! The first test below asserts the empty range directly, because that is the
//! fact the whole `origin` branch exists for — if it ever stops being true,
//! the extra routing is dead weight and should be deleted rather than kept
//! out of superstition.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::Repository;
use std::path::Path;

/// A repo with one commit, a modified tracked file and one untracked file,
/// all stashed with `-u`. Returns the repo and the stash sha.
async fn stashed_mixed(p: &Path) -> (Repository, String) {
    init_repo(p);
    write(&p.join("tracked.txt"), "one\ntwo\nthree\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("tracked.txt"), "one\nTWO\nthree\n");
    write(&p.join("untracked.txt"), "brand new\nsecond line\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("mixed"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    (repo, sha)
}

#[tokio::test]
async fn a_tracked_file_diffs_across_the_stash_range() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, sha) = stashed_mixed(tmp.path()).await;

    let diffs = repo
        .diff_for_range(&format!("{sha}^"), &sha, Path::new("tracked.txt"))
        .await
        .unwrap();

    assert_eq!(diffs.len(), 1, "expected one file, got {diffs:?}");
    let added: Vec<_> = diffs[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| matches!(l.kind, oximux_core::DiffLineKind::Added))
        .map(|l| l.content.trim().to_string())
        .collect();
    assert_eq!(added, vec!["TWO".to_string()]);
}

/// The fact the untracked routing exists for. `git stash push -u` puts an
/// untracked file in the third parent only, so the stash commit's own range
/// says nothing about it — and a UI that routes it like a tracked file paints
/// an empty tab with no error to explain it.
#[tokio::test]
async fn the_stash_range_says_nothing_about_an_untracked_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, sha) = stashed_mixed(tmp.path()).await;

    let diffs = repo
        .diff_for_range(&format!("{sha}^"), &sha, Path::new("untracked.txt"))
        .await
        .unwrap();

    assert!(
        diffs.is_empty(),
        "the range diff is supposed to be EMPTY for an untracked file; \
         if this now returns content, `StashFileOrigin` routing is obsolete: {diffs:?}"
    );
}

#[tokio::test]
async fn an_untracked_file_diffs_as_a_whole_file_add_from_the_third_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, sha) = stashed_mixed(tmp.path()).await;

    let diffs = repo
        .diff_in_rev(&format!("{sha}^3"), Path::new("untracked.txt"))
        .await
        .unwrap();

    assert_eq!(diffs.len(), 1, "expected one file, got {diffs:?}");
    assert_eq!(diffs[0].status, oximux_core::DiffStatus::Added);
    let added: Vec<_> = diffs[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| matches!(l.kind, oximux_core::DiffLineKind::Added))
        .map(|l| l.content.trim_end().to_string())
        .collect();
    assert_eq!(
        added,
        vec!["brand new".to_string(), "second line".to_string()],
        "every line of the file should arrive as an addition"
    );
}

/// The reason [`Repository::diff_in_rev`] names no base instead of diffing
/// against the empty tree: the empty tree's hash depends on the repo's hash
/// function, and a SHA-1 constant is an *error* in a SHA-256 repo, not a
/// degraded result. Run the whole routing here rather than trusting that the
/// SHA-1 case generalises.
#[tokio::test]
async fn both_halves_work_in_a_sha256_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    run_git(p, &["init", "-b", "main", "--object-format=sha256"]);
    run_git(p, &["config", "commit.gpgsign", "false"]);
    run_git(p, &["config", "user.name", "Test"]);
    run_git(p, &["config", "user.email", "test@example.com"]);
    write(&p.join("tracked.txt"), "one\ntwo\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "init"]);
    write(&p.join("tracked.txt"), "one\nTWO\n");
    write(&p.join("untracked.txt"), "new file\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("sha256"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    assert_eq!(sha.len(), 64, "expected a sha256 oid, got {sha}");

    let tracked = repo
        .diff_for_range(&format!("{sha}^"), &sha, Path::new("tracked.txt"))
        .await
        .unwrap();
    assert_eq!(tracked.len(), 1, "tracked half: {tracked:?}");

    let untracked = repo
        .diff_in_rev(&format!("{sha}^3"), Path::new("untracked.txt"))
        .await
        .unwrap();
    assert_eq!(untracked.len(), 1, "untracked half: {untracked:?}");
    assert_eq!(untracked[0].status, oximux_core::DiffStatus::Added);
}

/// A path that the revision does not touch is an empty result, not an error —
/// the panel only ever asks about paths `stash_files` listed, but a stale
/// expansion clicked after a refresh can still ask about one that has gone.
#[tokio::test]
async fn a_path_absent_from_the_revision_is_empty_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, sha) = stashed_mixed(tmp.path()).await;

    let diffs = repo
        .diff_in_rev(&format!("{sha}^3"), Path::new("nope.txt"))
        .await
        .unwrap();
    assert!(diffs.is_empty(), "got {diffs:?}");
}
