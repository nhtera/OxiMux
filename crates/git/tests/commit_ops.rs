//! Integration tests for commit operations on `Repository`: `commit` (staged
//! changes only) and `commit_paths` (stage-and-commit specific files).
//! Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::{GitError, Repository};
use std::path::Path;

/// Read HEAD SHA via the shell — independent of the method under test.
fn read_head_sha(p: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(p)
        .output()
        .expect("git rev-parse HEAD");
    assert!(out.status.success(), "rev-parse failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Read the full commit message body of HEAD.
fn read_head_message(p: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%B"])
        .current_dir(p)
        .output()
        .expect("git log");
    assert!(out.status.success(), "git log failed");
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

#[tokio::test]
async fn commit_staged_returns_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let sha = repo.commit("fix: initial").await.unwrap();
    assert_eq!(sha, read_head_sha(p), "returned SHA must equal HEAD");
    assert_eq!(read_head_message(p), "fix: initial");
}

#[tokio::test]
async fn commit_with_multiline_message_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);

    let msg = "subject line\n\nbody line 1\nbody line 2";
    let repo = Repository::open(p).await.unwrap();
    repo.commit(msg).await.unwrap();
    assert_eq!(read_head_message(p), msg);
}

#[tokio::test]
async fn commit_empty_message_errors_invalid_input() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.commit("").await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn commit_whitespace_only_message_errors_invalid_input() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.commit("   \n\n\t  ").await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn commit_nothing_staged_errors_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    // Now nothing is staged.

    let repo = Repository::open(p).await.unwrap();
    let err = repo.commit("nope").await.unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

#[tokio::test]
async fn commit_paths_stages_and_commits_selected_only() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // Start with a base commit so we can observe selective staging on top.
    write(&p.join("base.txt"), "base\n");
    run_git(p, &["add", "base.txt"]);
    run_git(p, &["commit", "-m", "base"]);
    // Two modifications — `commit_paths` must only commit p1.
    write(&p.join("p1.txt"), "one\n");
    write(&p.join("p2.txt"), "two\n");

    let repo = Repository::open(p).await.unwrap();
    let sha = repo
        .commit_paths("feat: only p1", &[Path::new("p1.txt")])
        .await
        .unwrap();
    assert_eq!(sha, read_head_sha(p));

    // p1 landed in the commit; p2 remains untracked (not in any commit, not in index).
    let st = repo.status().await.unwrap();
    let p2 = st
        .files
        .iter()
        .find(|f| f.path == Path::new("p2.txt"))
        .expect("p2.txt still untracked in status");
    use oximux_core::IndexStatus;
    assert_eq!(p2.index, IndexStatus::Untracked, "p2 must remain untracked");
    // And p1 is no longer in status (it's committed and clean).
    assert!(
        !st.files.iter().any(|f| f.path == Path::new("p1.txt")),
        "p1.txt should be clean post-commit"
    );
}

#[tokio::test]
async fn commit_paths_leaves_independently_staged_other_file_alone() {
    // M2 coverage: the trailing `-- <paths>` on `git commit` constrains the
    // commit to ONLY those paths, leaving other independently-staged changes
    // in the index. Stages p2 manually, then commit_paths(p1) — p2 must
    // remain staged after.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("base.txt"), "base\n");
    run_git(p, &["add", "base.txt"]);
    run_git(p, &["commit", "-m", "base"]);
    write(&p.join("p1.txt"), "one\n");
    write(&p.join("p2.txt"), "two\n");
    run_git(p, &["add", "p2.txt"]); // independently stage p2

    let repo = Repository::open(p).await.unwrap();
    repo.commit_paths("feat: only p1", &[Path::new("p1.txt")])
        .await
        .unwrap();

    // p2 should STILL be staged (Added in index), not folded into the commit.
    let st = repo.status().await.unwrap();
    use oximux_core::IndexStatus;
    let p2 = st
        .files
        .iter()
        .find(|f| f.path == Path::new("p2.txt"))
        .expect("p2.txt still present in status");
    assert_eq!(
        p2.index,
        IndexStatus::Added,
        "independently-staged p2 must survive commit_paths(p1)"
    );
}

#[tokio::test]
async fn commit_paths_empty_slice_errors_invalid_input() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.commit_paths("msg", &[]).await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn commit_paths_handles_filename_with_spaces() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    // Base commit so `commit_paths` has something to commit ON TOP of.
    write(&p.join("base.txt"), "base\n");
    run_git(p, &["add", "base.txt"]);
    run_git(p, &["commit", "-m", "base"]);
    write(&p.join("my file.txt"), "hi\n");

    let repo = Repository::open(p).await.unwrap();
    let sha = repo
        .commit_paths("add spaced", &[Path::new("my file.txt")])
        .await
        .unwrap();
    assert_eq!(sha, read_head_sha(p));
    // Verify the file is now tracked at HEAD.
    let out = std::process::Command::new("git")
        .args(["ls-tree", "--name-only", "HEAD"])
        .current_dir(p)
        .output()
        .unwrap();
    let names = String::from_utf8(out.stdout).unwrap();
    assert!(names.contains("my file.txt"), "tree: {names}");
}

#[tokio::test]
async fn commit_message_with_special_chars_no_shell_injection() {
    // `!` `"` `'` `$VAR` go through as separate arg to `GitCmd::arg`, so they
    // round-trip literally without any shell expansion.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);

    let msg = r#"fix!: "shouldn't" break 'parser' $HOME `id`"#;
    let repo = Repository::open(p).await.unwrap();
    repo.commit(msg).await.unwrap();
    assert_eq!(read_head_message(p), msg);
}

#[tokio::test]
async fn cherry_pick_applies_commit_onto_head() {
    // Set up: main has one commit; a feature branch adds a second commit;
    // switch back to main; cherry-pick the feature commit onto main.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    run_git(p, &["checkout", "-b", "feat"]);
    write(&p.join("b.txt"), "feat\n");
    run_git(p, &["add", "b.txt"]);
    run_git(p, &["commit", "-m", "feat: add b"]);
    let feat_sha = read_head_sha(p);
    run_git(p, &["checkout", "main"]);

    let repo = Repository::open(p).await.unwrap();
    repo.cherry_pick(&feat_sha).await.unwrap();

    // HEAD should now carry the same subject as the feature commit and
    // the file should exist in the working tree.
    assert_eq!(read_head_message(p), "feat: add b");
    assert!(p.join("b.txt").exists(), "cherry-picked file must land");
}

#[tokio::test]
async fn cherry_pick_empty_sha_errors_invalid_input() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    let repo = Repository::open(p).await.unwrap();
    let err = repo.cherry_pick("").await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}

#[tokio::test]
async fn revert_commit_creates_inverse_commit() {
    // Two commits on main; revert the latest one. HEAD should advance by
    // one commit (the auto-generated "Revert …") and the touched file
    // should be back to its pre-commit content.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);
    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "bump to v2"]);
    let bump_sha = read_head_sha(p);

    let repo = Repository::open(p).await.unwrap();
    repo.revert_commit(&bump_sha).await.unwrap();

    // git's auto-message starts with "Revert ".
    let head_msg = read_head_message(p);
    assert!(
        head_msg.starts_with("Revert"),
        "revert subject must start with 'Revert', got {head_msg:?}"
    );
    let content = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(content, "v1\n", "file must roll back to pre-bump content");
}

#[tokio::test]
async fn revert_commit_empty_sha_errors_invalid_input() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    let repo = Repository::open(p).await.unwrap();
    let err = repo.revert_commit("").await.unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");
}
