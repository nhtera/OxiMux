//! Integration tests for `Repository::commit_files(sha)` — multi-file
//! patch retrieval for a single commit, backing the commit-detail tab
//! in the SCM panel.

mod common;

use common::{init_repo, run_git, write};
use oximux_core::DiffStatus;
use oximux_git::Repository;
use std::path::Path;

#[tokio::test]
async fn commit_files_returns_per_file_diff_for_normal_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);

    // Base commit (single file).
    write(&p.join("readme.md"), "v1\n");
    run_git(p, &["add", "readme.md"]);
    run_git(p, &["commit", "-m", "base"]);

    // Target commit touches two files: edit existing + add new.
    write(&p.join("readme.md"), "v2\n");
    write(&p.join("src.rs"), "fn main() {}\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "target"]);

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.commit_files("HEAD").await.unwrap();

    assert_eq!(diffs.len(), 2, "expected 2 files in commit, got {diffs:?}");

    let readme = diffs
        .iter()
        .find(|d| d.path == Path::new("readme.md"))
        .expect("readme.md");
    assert!(matches!(readme.status, DiffStatus::Modified));
    let src = diffs
        .iter()
        .find(|d| d.path == Path::new("src.rs"))
        .expect("src.rs");
    assert!(matches!(src.status, DiffStatus::Added));
}

#[tokio::test]
async fn commit_files_handles_root_commit() {
    // git show --format= -p works for root commits natively (diffs
    // against the empty tree). No --root fallback needed.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);

    write(&p.join("readme.md"), "first\n");
    write(&p.join("src.rs"), "fn root() {}\n");
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "root"]);

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.commit_files("HEAD").await.unwrap();

    assert_eq!(diffs.len(), 2, "root commit should surface both files");
    for d in &diffs {
        assert!(
            matches!(d.status, DiffStatus::Added),
            "root commit files should classify as Added, got {:?}",
            d.status
        );
    }
}

#[tokio::test]
async fn commit_files_accepts_short_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);

    write(&p.join("a.txt"), "x\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    // Capture short SHA via `git rev-parse --short HEAD`.
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(p)
        .output()
        .expect("rev-parse");
    let short = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.commit_files(&short).await.unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, Path::new("a.txt"));
}

#[tokio::test]
async fn commit_files_handles_merge_commit_via_first_parent() {
    // Merge commits emit combined-diff format by default which
    // parse_unified_diff doesn't speak. `--first-parent` (added to
    // the git show invocation in repository.rs) makes the output a
    // standard unified diff against mainline.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);

    // base on main
    write(&p.join("readme.md"), "v1\n");
    run_git(p, &["add", "readme.md"]);
    run_git(p, &["commit", "-m", "base"]);

    // feature branch with its own change
    run_git(p, &["checkout", "-b", "feature"]);
    write(&p.join("feature.rs"), "fn feature() {}\n");
    run_git(p, &["add", "feature.rs"]);
    run_git(p, &["commit", "-m", "add feature"]);

    // diverge mainline
    run_git(p, &["checkout", "main"]);
    write(&p.join("readme.md"), "v2\n");
    run_git(p, &["add", "readme.md"]);
    run_git(p, &["commit", "-m", "bump readme"]);

    // merge feature into main → creates a merge commit at HEAD
    run_git(p, &["merge", "--no-ff", "feature", "-m", "merge feature"]);

    let repo = Repository::open(p).await.unwrap();
    let diffs = repo.commit_files("HEAD").await.unwrap();

    // First-parent diff against the most recent mainline tip shows
    // only the change brought in from `feature` — `feature.rs` added.
    // The pre-existing readme on the mainline side isn't surfaced
    // because it was already in the first parent.
    assert!(
        diffs.iter().any(|d| d.path == Path::new("feature.rs")),
        "merge commit's first-parent diff should surface feature.rs, got: {diffs:?}"
    );
}

#[tokio::test]
async fn commit_files_errors_on_unknown_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "x\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    // Random SHA that doesn't exist — git show should error.
    let result = repo
        .commit_files("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        .await;
    assert!(result.is_err(), "expected error for unknown sha, got Ok");
}
