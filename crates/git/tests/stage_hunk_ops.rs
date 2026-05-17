//! Integration tests for hunk-level stage operations on `Repository`:
//! `stage_hunks`, `unstage_hunks`, and the round-trip of `build_patch`
//! through real `git apply --cached`. Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::{DiffStatus, IndexStatus};
use oximux_git::Repository;
use std::path::Path;

#[tokio::test]
async fn stage_hunk_partial_stages_only_selected_hunk() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // 10-line file so two distant changes produce two separate hunks.
    let base = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
    write(&p.join("multi.txt"), &base);
    run_git(p, &["add", "multi.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let mut modified = base.clone();
    modified = modified.replace("line 1\n", "LINE 1\n");
    modified = modified.replace("line 10\n", "LINE 10\n");
    write(&p.join("multi.txt"), &modified);

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.diff_unstaged().await.unwrap();
    assert_eq!(diffs.len(), 1);
    let file = &diffs[0];
    assert!(
        file.hunks.len() >= 2,
        "expected 2 hunks, got {}",
        file.hunks.len()
    );

    repo.stage_hunks(file, &[0]).await.unwrap();

    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1, "exactly one file staged");
    // Use whole-line equality to avoid the "LINE 1" / "LINE 10" prefix trap.
    let staged_lines: Vec<&str> = staged[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.content.as_str())
        .collect();
    assert!(
        staged_lines.contains(&"LINE 1"),
        "staged hunk lost line-1 change: {staged_lines:?}"
    );
    assert!(
        !staged_lines.contains(&"LINE 10"),
        "staged hunk leaked line-10 change: {staged_lines:?}"
    );

    let unstaged = repo.diff_unstaged().await.unwrap();
    assert_eq!(unstaged.len(), 1, "exactly one file still unstaged");
    let unstaged_lines: Vec<&str> = unstaged[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.content.as_str())
        .collect();
    assert!(
        unstaged_lines.contains(&"LINE 10"),
        "unstaged should still have line-10 change: {unstaged_lines:?}"
    );
    assert!(
        !unstaged_lines.contains(&"LINE 1"),
        "unstaged should no longer carry the already-staged line-1: {unstaged_lines:?}"
    );
}

#[tokio::test]
async fn unstage_hunk_partial_reverts_selection_only() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    let base = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
    write(&p.join("multi.txt"), &base);
    run_git(p, &["add", "multi.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let mut modified = base.clone();
    modified = modified.replace("line 1\n", "LINE 1\n");
    modified = modified.replace("line 10\n", "LINE 10\n");
    write(&p.join("multi.txt"), &modified);
    run_git(p, &["add", "multi.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1);
    let file = &staged[0];
    assert!(
        file.hunks.len() >= 2,
        "expected 2 staged hunks, got {}",
        file.hunks.len()
    );

    repo.unstage_hunks(file, &[0]).await.unwrap();

    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1, "one file still partially staged");
    let staged_lines: Vec<&str> = staged[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.content.as_str())
        .collect();
    assert!(
        staged_lines.contains(&"LINE 10"),
        "line-10 change should still be staged: {staged_lines:?}"
    );
    assert!(
        !staged_lines.contains(&"LINE 1"),
        "line-1 change should have been unstaged: {staged_lines:?}"
    );
}

#[tokio::test]
async fn stage_hunk_handles_no_newline_at_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // No trailing newline — triggers the `\ No newline at end of file` marker.
    write(&p.join("tail.txt"), "first\nsecond");
    run_git(p, &["add", "tail.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("tail.txt"), "first\nSECOND");

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.diff_unstaged().await.unwrap();
    assert_eq!(diffs.len(), 1);
    repo.stage_hunks(&diffs[0], &[0]).await.unwrap();

    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1, "no-newline hunk should have been staged");
}

#[tokio::test]
async fn stage_hunk_for_deleted_file() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("dead.rs"), "fn dead() {}\n");
    run_git(p, &["add", "dead.rs"]);
    run_git(p, &["commit", "-m", "init"]);

    std::fs::remove_file(p.join("dead.rs")).unwrap();

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.diff_unstaged().await.unwrap();
    assert_eq!(diffs.len(), 1);
    let file = &diffs[0];
    assert!(matches!(file.status, DiffStatus::Deleted));
    repo.stage_hunks(file, &[0]).await.unwrap();

    let st = repo.status().await.unwrap();
    let dead = st
        .files
        .iter()
        .find(|f| f.path == Path::new("dead.rs"))
        .expect("dead.rs in status");
    assert_eq!(
        dead.index,
        IndexStatus::Deleted,
        "deletion should be staged"
    );
    // Sanity: no phantom `dev/null` entry — the `deleted file mode` extended
    // header in the patch keeps git from interpreting /dev/null as a real
    // path. (Regression guard for the bug found during step 5 implementation.)
    assert!(
        !st.files.iter().any(|f| f.path == Path::new("dev/null")),
        "phantom dev/null entry leaked into index: {:?}",
        st.files
    );
}

#[tokio::test]
async fn unstage_hunk_for_renamed_file_with_content_change() {
    // git only surfaces renames in the *staged* diff (rename detection runs
    // against the index), so this exercises `unstage_hunks` on a
    // `DiffStatus::Renamed` FileDiff — the only place a rename actually
    // shows up via our diff_* methods. Verifies the rename extended headers
    // (`similarity index` + `rename from` + `rename to`) emitted by
    // build_patch are accepted by `git apply --cached --reverse`.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // 10-line file so a single-line change leaves enough surrounding text
    // for git's similarity detector to call it a rename rather than
    // delete-plus-add.
    let base = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
    write(&p.join("from.rs"), &base);
    run_git(p, &["add", "from.rs"]);
    run_git(p, &["commit", "-m", "init"]);

    // Rename + minor edit, then stage everything so git sees a rename.
    std::fs::rename(p.join("from.rs"), p.join("to.rs")).unwrap();
    let modified = base.replace("line 5\n", "LINE 5\n");
    write(&p.join("to.rs"), &modified);
    run_git(p, &["add", "-A"]);

    let repo = Repository::open(p).await.unwrap();
    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1, "expected exactly one staged rename");
    let file = &staged[0];
    let DiffStatus::Renamed { ref from, .. } = file.status else {
        panic!("expected Renamed, got {:?}", file.status);
    };
    assert_eq!(from, Path::new("from.rs"));
    assert_eq!(file.path, Path::new("to.rs"));
    assert!(!file.hunks.is_empty(), "rename should carry content hunks");

    // Unstage the lone content hunk. The rename itself stays staged
    // (we only ship the content diff in the reverse patch).
    repo.unstage_hunks(file, &[0]).await.unwrap();

    // The line-5 change should now be unstaged. Whether git still classifies
    // the file as a rename in the staged diff depends on similarity score —
    // assert on the absence of the content edit rather than on status, which
    // is the load-bearing property of the unstage.
    let staged_after = repo.diff_staged().await.unwrap();
    let staged_lines: Vec<&str> = staged_after
        .iter()
        .flat_map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()))
        .map(|l| l.content.as_str())
        .collect();
    assert!(
        !staged_lines.contains(&"LINE 5"),
        "content edit should have been unstaged, but: {staged_lines:?}"
    );
}

#[tokio::test]
async fn stage_hunk_handles_filename_with_spaces() {
    // build_patch emits `diff --git a/my file.txt b/my file.txt` and git
    // apply parses it identically. No quoting needed on caller side.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("my file.txt"), "first\nsecond\nthird\n");
    run_git(p, &["add", "my file.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("my file.txt"), "first\nSECOND\nthird\n");
    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.diff_unstaged().await.unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, Path::new("my file.txt"));
    repo.stage_hunks(&diffs[0], &[0]).await.unwrap();

    let st = repo.status().await.unwrap();
    let entry = st
        .files
        .iter()
        .find(|f| f.path == Path::new("my file.txt"))
        .expect("'my file.txt' present in status");
    assert_eq!(entry.index, IndexStatus::Modified);
}

#[tokio::test]
async fn build_patch_roundtrips_through_real_git() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    let base = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
    write(&p.join("f.txt"), &base);
    run_git(p, &["add", "f.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let mut modified = base.clone();
    modified = modified.replace("line 1\n", "ONE\n");
    modified = modified.replace("line 10\n", "TEN\n");
    modified = modified.replace("line 20\n", "TWENTY\n");
    write(&p.join("f.txt"), &modified);

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.diff_unstaged().await.unwrap();
    let file = &diffs[0];
    assert!(file.hunks.len() >= 3, "expected ≥3 hunks");

    repo.stage_hunks(file, &[1]).await.unwrap();
    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1);
    let lines: Vec<&str> = staged[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.content.as_str())
        .collect();
    assert!(lines.contains(&"TEN"), "got staged lines: {lines:?}");
    assert!(!lines.contains(&"ONE"));
    assert!(!lines.contains(&"TWENTY"));
}
