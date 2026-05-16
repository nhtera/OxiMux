//! Fixture-driven integration tests for `parse_unified_diff` + an end-to-end
//! smoke test that exercises `Repository::diff_staged` against a real `git`
//! binary on a tempdir-backed repo (mirrors `repository_smoke.rs`).

use oximux_core::{DiffLineKind, DiffStatus};
use oximux_git::{Repository, parse_unified_diff};
use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/diff")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

#[test]
fn simple_add() {
    let diffs = parse_unified_diff(&fixture("simple_add.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("src/new.rs"));
    assert!(matches!(f.status, DiffStatus::Added));
    assert_eq!(f.hunks.len(), 1);
    let h = &f.hunks[0];
    assert_eq!(h.old_start, 0);
    assert_eq!(h.old_lines, 0);
    assert_eq!(h.new_start, 1);
    assert_eq!(h.new_lines, 3);
    assert_eq!(h.lines.len(), 3);
    assert!(h.lines.iter().all(|l| l.kind == DiffLineKind::Added));
    assert!(!f.large);
}

#[test]
fn simple_delete() {
    let diffs = parse_unified_diff(&fixture("simple_delete.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("src/old.rs"));
    assert!(matches!(f.status, DiffStatus::Deleted));
    assert_eq!(f.hunks.len(), 1);
    assert_eq!(f.hunks[0].lines.len(), 2);
    assert!(
        f.hunks[0]
            .lines
            .iter()
            .all(|l| l.kind == DiffLineKind::Removed)
    );
}

#[test]
fn multi_hunk_and_multi_file() {
    let diffs = parse_unified_diff(&fixture("multi_hunk.diff")).unwrap();
    assert_eq!(diffs.len(), 2, "expected 2 files");
    assert_eq!(diffs[0].path, PathBuf::from("src/a.rs"));
    assert!(matches!(diffs[0].status, DiffStatus::Modified));
    assert_eq!(diffs[0].hunks.len(), 2, "a.rs has 2 hunks");
    assert_eq!(diffs[1].path, PathBuf::from("src/b.rs"));
    assert_eq!(diffs[1].hunks.len(), 1);
    // hunk 2 of a.rs carries the function-context suffix.
    assert_eq!(diffs[0].hunks[1].header_suffix, "fn second() {");
}

#[test]
fn rename_with_similarity() {
    let diffs = parse_unified_diff(&fixture("rename_with_score.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("src/new_name.rs"));
    match &f.status {
        DiffStatus::Renamed { from, similarity } => {
            assert_eq!(from, &PathBuf::from("src/old_name.rs"));
            assert_eq!(*similarity, 94);
        }
        other => panic!("expected Renamed, got {other:?}"),
    }
    assert!(!f.hunks.is_empty(), "rename with content still has hunks");
}

#[test]
fn binary_files_no_hunks() {
    let diffs = parse_unified_diff(&fixture("binary.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("assets/logo.png"));
    assert!(matches!(f.status, DiffStatus::Binary));
    assert!(f.hunks.is_empty());
    assert!(!f.large);
}

#[test]
fn mode_change_only() {
    let diffs = parse_unified_diff(&fixture("mode_change.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("scripts/build.sh"));
    match f.status {
        DiffStatus::ModeChanged { old_mode, new_mode } => {
            assert_eq!(old_mode, 0o100644);
            assert_eq!(new_mode, 0o100755);
        }
        ref other => panic!("expected ModeChanged, got {other:?}"),
    }
    assert!(f.hunks.is_empty());
}

#[test]
fn rename_no_content_change() {
    let diffs = parse_unified_diff(&fixture("rename_no_content.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("src/new_path.rs"));
    match &f.status {
        DiffStatus::Renamed { from, similarity } => {
            assert_eq!(from, &PathBuf::from("src/old_path.rs"));
            assert_eq!(*similarity, 100);
        }
        other => panic!("expected Renamed, got {other:?}"),
    }
    assert!(f.hunks.is_empty(), "pure rename has no hunks");
    assert!(!f.large);
}

#[test]
fn mode_change_with_content_hunks() {
    let diffs = parse_unified_diff(&fixture("mode_change_with_content.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let f = &diffs[0];
    assert_eq!(f.path, PathBuf::from("scripts/run.sh"));
    match f.status {
        DiffStatus::ModeChanged { old_mode, new_mode } => {
            assert_eq!(old_mode, 0o100644);
            assert_eq!(new_mode, 0o100755);
        }
        ref other => panic!("expected ModeChanged (winning over Modified), got {other:?}"),
    }
    assert_eq!(f.hunks.len(), 1, "hunks preserved alongside mode change");
}

#[test]
fn no_newline_at_end_of_file() {
    let diffs = parse_unified_diff(&fixture("no_newline_eof.diff")).unwrap();
    assert_eq!(diffs.len(), 1);
    let lines = &diffs[0].hunks[0].lines;
    // Two NoNewlineHint markers — one for removed final line, one for added.
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::NoNewlineHint)
            .count(),
        2
    );
}

#[test]
fn large_flag_trips_above_threshold() {
    let mut raw = String::from(
        "diff --git a/big.txt b/big.txt\nindex 1..2 100644\n--- a/big.txt\n+++ b/big.txt\n",
    );
    // 1001 added lines so total body > LARGE_DIFF_LINE_THRESHOLD (1000).
    raw.push_str("@@ -1,0 +1,1001 @@\n");
    for i in 0..1001 {
        raw.push_str(&format!("+row {i}\n"));
    }
    let diffs = parse_unified_diff(&raw).unwrap();
    assert_eq!(diffs.len(), 1);
    assert!(diffs[0].large, "expected large flag with 1001 lines");

    // Sanity: 1000 lines exactly should NOT trip large.
    let mut raw_small = String::from(
        "diff --git a/small.txt b/small.txt\nindex 1..2 100644\n--- a/small.txt\n+++ b/small.txt\n",
    );
    raw_small.push_str("@@ -1,0 +1,1000 @@\n");
    for i in 0..1000 {
        raw_small.push_str(&format!("+row {i}\n"));
    }
    let diffs = parse_unified_diff(&raw_small).unwrap();
    assert!(!diffs[0].large);
}

#[test]
fn crlf_input_parses_same_as_lf() {
    let lf = fixture("simple_add.diff");
    let crlf = lf.replace('\n', "\r\n");
    let a = parse_unified_diff(&lf).unwrap();
    let b = parse_unified_diff(&crlf).unwrap();
    assert_eq!(a, b);
}

#[test]
fn malformed_hunk_header_errors() {
    let raw = "\
diff --git a/x b/x
index 1..2 100644
--- a/x
+++ b/x
@@ broken @@
";
    let err = parse_unified_diff(raw).unwrap_err();
    use oximux_git::DiffParseError;
    match err {
        DiffParseError::BadHunkHeader { line_no, .. } => {
            assert_eq!(line_no, 5, "@@ broken @@ is line 5");
        }
    }
}

// ---------------------------------------------------------------------------
// End-to-end smoke: drives `git` against a tempdir and parses real output.
// MSRV-git: 2.28+ (uses `git init -b main`, same constraint as
// `repository_smoke.rs`).
// ---------------------------------------------------------------------------

fn run_git(cwd: &std::path::Path, args: &[&str]) {
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

#[tokio::test]
async fn diff_staged_against_real_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path();
    run_git(repo_path, &["init", "-b", "main"]);
    std::fs::write(repo_path.join("hello.txt"), "hello\n").unwrap();
    run_git(repo_path, &["add", "hello.txt"]);

    let repo = Repository::open(repo_path).await.unwrap();
    let staged = repo.diff_staged().await.unwrap();
    assert_eq!(staged.len(), 1);
    let f = &staged[0];
    assert_eq!(f.path, PathBuf::from("hello.txt"));
    assert!(matches!(f.status, DiffStatus::Added));
    let unstaged = repo.diff_unstaged().await.unwrap();
    assert!(
        unstaged.is_empty(),
        "no unstaged changes — file is fully staged"
    );
}

#[tokio::test]
async fn diff_for_path_filters_to_single_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_path = tmp.path();
    run_git(repo_path, &["init", "-b", "main"]);
    std::fs::write(repo_path.join("a.txt"), "a\n").unwrap();
    std::fs::write(repo_path.join("b.txt"), "b\n").unwrap();
    run_git(repo_path, &["add", "a.txt", "b.txt"]);

    let repo = Repository::open(repo_path).await.unwrap();
    let only_a = repo
        .diff_for_path(std::path::Path::new("a.txt"), true)
        .await
        .unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].path, PathBuf::from("a.txt"));
}
