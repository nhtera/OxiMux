//! Integration tests for vectorised path ops invoked with N > 1 paths
//! in one call — the shape `GitPanel::bulk_stage_selected` /
//! `bulk_unstage_selected` use after Phase 03 Slice B.
//!
//! The single-path codepath is already covered by `stage_file_ops.rs`.
//! These tests lock down the N>1 contract: every path in the vector
//! must flip to the target side in a single git invocation, and
//! `discard_paths` must restore every committed path's content in one
//! shot.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::{IndexStatus, WorktreeStatus};
use oximux_git::Repository;
use std::path::Path;

/// Vectorised stage on three modified files: all three flip to
/// `IndexStatus::Modified` (committed file modified then staged) /
/// `WorktreeStatus::Unmodified` (no remaining worktree diff after
/// stage) in one `git add` call.
#[tokio::test]
async fn stage_paths_three_modified_files_all_staged() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    write(&p.join("b.txt"), "v1\n");
    write(&p.join("c.txt"), "v1\n");
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);
    // Dirty all three identically so each row has the same shape pre-stage.
    write(&p.join("a.txt"), "v2\n");
    write(&p.join("b.txt"), "v2\n");
    write(&p.join("c.txt"), "v2\n");

    let repo = Repository::open(p).await.unwrap();
    repo.stage_paths(&[
        Path::new("a.txt"),
        Path::new("b.txt"),
        Path::new("c.txt"),
    ])
    .await
    .unwrap();

    let st = repo.status().await.unwrap();
    assert_eq!(st.files.len(), 3, "exactly three rows expected");
    for f in &st.files {
        assert_eq!(
            f.index,
            IndexStatus::Modified,
            "{:?} should be staged Modified",
            f.path
        );
        assert_eq!(
            f.worktree,
            WorktreeStatus::Unmodified,
            "{:?} worktree should be clean after stage",
            f.path
        );
    }
}

/// Vectorised unstage on three staged-modified files: every path
/// returns to unstaged-only in one `git restore --staged` call.
#[tokio::test]
async fn unstage_paths_three_staged_files_all_unstaged() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    write(&p.join("b.txt"), "v1\n");
    write(&p.join("c.txt"), "v1\n");
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);
    write(&p.join("a.txt"), "v2\n");
    write(&p.join("b.txt"), "v2\n");
    write(&p.join("c.txt"), "v2\n");
    run_git(p, &["add", "."]);

    let repo = Repository::open(p).await.unwrap();
    repo.unstage_paths(&[
        Path::new("a.txt"),
        Path::new("b.txt"),
        Path::new("c.txt"),
    ])
    .await
    .unwrap();

    let st = repo.status().await.unwrap();
    assert_eq!(st.files.len(), 3);
    for f in &st.files {
        assert_eq!(
            f.index,
            IndexStatus::Unmodified,
            "{:?} should leave index unchanged",
            f.path
        );
        assert_eq!(
            f.worktree,
            WorktreeStatus::Modified,
            "{:?} should remain modified in worktree",
            f.path
        );
    }
}

/// Vectorised discard on three modified files: every committed path's
/// content is restored from HEAD in one `git restore` call. No row
/// should remain in the status output.
#[tokio::test]
async fn discard_paths_three_modified_files_all_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "original-a\n");
    write(&p.join("b.txt"), "original-b\n");
    write(&p.join("c.txt"), "original-c\n");
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);
    write(&p.join("a.txt"), "scratch-a\n");
    write(&p.join("b.txt"), "scratch-b\n");
    write(&p.join("c.txt"), "scratch-c\n");

    let repo = Repository::open(p).await.unwrap();
    repo.discard_paths(&[
        Path::new("a.txt"),
        Path::new("b.txt"),
        Path::new("c.txt"),
    ])
    .await
    .unwrap();

    let st = repo.status().await.unwrap();
    assert!(
        st.files.is_empty(),
        "all rows should be clean after bulk discard; got {:?}",
        st.files
    );
    // Spot-check content was actually restored, not just unmarked.
    let a = std::fs::read_to_string(p.join("a.txt")).unwrap();
    let c = std::fs::read_to_string(p.join("c.txt")).unwrap();
    assert_eq!(a, "original-a\n");
    assert_eq!(c, "original-c\n");
}

/// `delete_untracked_paths` removes pure-untracked files via
/// `git clean -f --`. Locks down the Slice C dispatch: a Phase 01
/// single-row Delete (or Slice C area Delete) on an untracked file
/// MUST physically remove the worktree file. Previously the panel
/// called `discard_paths` which errors on untracked paths.
#[tokio::test]
async fn delete_untracked_paths_removes_files_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("seed.txt"), "seed\n");
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);
    write(&p.join("a.txt"), "a\n");
    write(&p.join("b.txt"), "b\n");
    write(&p.join("c.txt"), "c\n");

    let repo = Repository::open(p).await.unwrap();
    repo.delete_untracked_paths(&[
        Path::new("a.txt"),
        Path::new("b.txt"),
        Path::new("c.txt"),
    ])
    .await
    .unwrap();

    assert!(!p.join("a.txt").exists());
    assert!(!p.join("b.txt").exists());
    assert!(!p.join("c.txt").exists());
    assert!(p.join("seed.txt").exists(), "tracked file untouched");
    let st = repo.status().await.unwrap();
    assert!(
        st.files.is_empty(),
        "no untracked rows remain; got {:?}",
        st.files
    );
}

/// Staged-area discard sequence end-to-end: 3 files staged with
/// committed-vs-staged drift. The Slice C `confirmed_discard_area`
/// for `Staged` first runs `unstage_paths` (so the index drops the
/// staged version), THEN `discard_paths` (so the worktree picks up
/// the HEAD content). The combined effect on each file: index back
/// to Unmodified, worktree back to HEAD.
#[tokio::test]
async fn staged_area_sequence_unstages_then_restores_to_head() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "head-a\n");
    write(&p.join("b.txt"), "head-b\n");
    write(&p.join("c.txt"), "head-c\n");
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);
    // Each file has BOTH a staged change and the same worktree
    // content (whole-file edit staged in one shot). Without the
    // unstage-first step, `git restore --` on a fully-staged path
    // can't pick up the HEAD version because the index hasn't been
    // cleared.
    write(&p.join("a.txt"), "staged-a\n");
    write(&p.join("b.txt"), "staged-b\n");
    write(&p.join("c.txt"), "staged-c\n");
    run_git(p, &["add", "."]);

    let repo = Repository::open(p).await.unwrap();
    let refs = &[
        Path::new("a.txt"),
        Path::new("b.txt"),
        Path::new("c.txt"),
    ];
    // Pins the BACKEND contract `confirmed_discard_area(Staged)`
    // depends on — unstage_paths(refs).await? + discard_paths(refs)
    // together restore HEAD content for every staged path. The
    // app-side `run_area_discard_sequence` wraps these in a
    // `?`-chain; if the unstage-first ordering or the dispatch ever
    // breaks at the wrapper level, the wrapper's own (future) test
    // would catch it — this test is the foundation.
    repo.unstage_paths(refs).await.unwrap();
    repo.discard_paths(refs).await.unwrap();

    let st = repo.status().await.unwrap();
    assert!(
        st.files.is_empty(),
        "all rows clean after staged-area sequence; got {:?}",
        st.files
    );
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "head-a\n");
    assert_eq!(std::fs::read_to_string(p.join("b.txt")).unwrap(), "head-b\n");
    assert_eq!(std::fs::read_to_string(p.join("c.txt")).unwrap(), "head-c\n");
}

/// Bulk stage on a partial-stage path: file has both staged and
/// unstaged hunks before; after `stage_paths` the remaining unstaged
/// hunks land in the index. Locks down that the bulk path treats
/// partial-stage as "stage the rest", matching what the
/// BulkActionBar's "Stage (N)" button does when one of N rows is a
/// partial-stage mirror.
#[tokio::test]
async fn stage_paths_promotes_partial_stage_to_fully_staged() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("doc.txt"), "line-a\nline-b\nline-c\n");
    run_git(p, &["add", "."]);
    run_git(p, &["commit", "-m", "seed"]);

    // Set up partial-stage without `git add --patch` (can't drive
    // interactively from a test): stage one whole-file edit, then
    // re-edit the worktree on top. After this:
    //   HEAD blob  = "line-a\nline-b\nline-c\n"
    //   index blob = "STAGED-a\nline-b\nUNSTAGED-c\n"
    //   worktree   = "STAGED-a\nline-b\nRE-EDITED-c\n"
    // Index ≠ HEAD → is_staged(); worktree ≠ index → is_unstaged().
    write(&p.join("doc.txt"), "STAGED-a\nline-b\nUNSTAGED-c\n");
    run_git(p, &["add", "doc.txt"]);
    write(&p.join("doc.txt"), "STAGED-a\nline-b\nRE-EDITED-c\n");

    let repo = Repository::open(p).await.unwrap();
    let st = repo.status().await.unwrap();
    let doc = st
        .files
        .iter()
        .find(|f| f.path == Path::new("doc.txt"))
        .expect("doc.txt visible pre-bulk-stage");
    assert!(doc.is_staged(), "should be staged pre-bulk");
    assert!(
        doc.is_unstaged(),
        "should be partial-stage with unstaged residue"
    );

    // Bulk stage as the panel would: one call with the partial-stage
    // path in the vector. Result: doc.txt index = Modified (the full
    // worktree content lands in the index), no unstaged residue.
    repo.stage_paths(&[Path::new("doc.txt")]).await.unwrap();
    let st = repo.status().await.unwrap();
    let doc = st
        .files
        .iter()
        .find(|f| f.path == Path::new("doc.txt"))
        .expect("doc.txt still visible (now fully staged)");
    assert_eq!(doc.index, IndexStatus::Modified);
    assert_eq!(
        doc.worktree,
        WorktreeStatus::Unmodified,
        "no unstaged residue after bulk stage"
    );
}
