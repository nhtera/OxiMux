use super::*;
use std::path::Path;

async fn git(dir: &Path, args: &[&str]) {
    GitCmd::new(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .run()
        .await
        .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
}

async fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]).await;
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-q", "-m", "init"]).await;
}

async fn engine(dir: &Path) -> CheckpointEngine {
    CheckpointEngine::new(dir).await.unwrap().expect("is a repo")
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

#[tokio::test]
async fn roundtrip_restores_tracked_and_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commit(tmp.path()).await;
    let eng = engine(tmp.path()).await;

    std::fs::write(tmp.path().join("a.txt"), "two\n").unwrap();
    std::fs::write(tmp.path().join("new.txt"), "untracked\n").unwrap();
    let cp = eng.create().await.unwrap();

    std::fs::write(tmp.path().join("a.txt"), "three\n").unwrap();
    std::fs::write(tmp.path().join("new.txt"), "mutated\n").unwrap();
    eng.restore(&cp).await.unwrap();

    assert_eq!(read(tmp.path(), "a.txt"), "two\n");
    assert_eq!(read(tmp.path(), "new.txt"), "untracked\n", "untracked captured");
}

#[tokio::test]
async fn no_refs_created_and_index_branch_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commit(tmp.path()).await;
    let eng = engine(tmp.path()).await;

    // Stage something so we can assert staged state survives.
    std::fs::write(tmp.path().join("staged.txt"), "staged\n").unwrap();
    git(tmp.path(), &["add", "staged.txt"]).await;
    let staged_before = GitCmd::new(tmp.path())
        .args(["status", "--porcelain=v1"])
        .run()
        .await
        .unwrap()
        .stdout;

    let cp = eng.create().await.unwrap();
    eng.restore(&cp).await.unwrap();

    let refs = GitCmd::new(tmp.path())
        .args(["for-each-ref"])
        .run()
        .await
        .unwrap();
    let refs = String::from_utf8_lossy(&refs.stdout);
    assert_eq!(refs.matches('\n').count(), 1, "only main ref exists: {refs}");

    let head = GitCmd::new(tmp.path())
        .args(["symbolic-ref", "HEAD"])
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "refs/heads/main");

    let staged_after = GitCmd::new(tmp.path())
        .args(["status", "--porcelain=v1"])
        .run()
        .await
        .unwrap()
        .stdout;
    assert_eq!(staged_before, staged_after, "staged set unchanged");
}

#[tokio::test]
async fn oversized_untracked_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commit(tmp.path()).await;
    let eng = engine(tmp.path()).await;

    let big = vec![0u8; (MAX_UNTRACKED_BYTES as usize) + 1];
    std::fs::write(tmp.path().join("big.bin"), &big).unwrap();
    let cp = eng.create().await.unwrap();

    std::fs::write(tmp.path().join("big.bin"), b"replaced").unwrap();
    eng.restore(&cp).await.unwrap();
    assert_eq!(
        std::fs::read(tmp.path().join("big.bin")).unwrap(),
        b"replaced",
        "oversized file not captured, restore leaves it alone"
    );
    // Exclude file back to pristine (absent or without our block).
    let exclude = tmp.path().join(".git/info/exclude");
    if exclude.exists() {
        assert!(!read(tmp.path(), ".git/info/exclude").contains("oximux"));
    }
}

#[tokio::test]
async fn empty_repo_root_commit_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]).await;
    let eng = engine(tmp.path()).await;

    std::fs::write(tmp.path().join("only.txt"), "x\n").unwrap();
    let cp = eng.create().await.unwrap();
    std::fs::write(tmp.path().join("only.txt"), "y\n").unwrap();
    eng.restore(&cp).await.unwrap();
    assert_eq!(read(tmp.path(), "only.txt"), "x\n");
}

#[tokio::test]
async fn not_a_repo_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(CheckpointEngine::new(tmp.path()).await.unwrap().is_none());
}

#[tokio::test]
async fn post_checkpoint_new_file_survives_restore() {
    // Accepted limitation: restore does not delete files created after the
    // checkpoint (git restore only rewrites files present in the source tree).
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commit(tmp.path()).await;
    let eng = engine(tmp.path()).await;

    let cp = eng.create().await.unwrap();
    std::fs::write(tmp.path().join("later.txt"), "created after\n").unwrap();
    eng.restore(&cp).await.unwrap();
    assert!(tmp.path().join("later.txt").exists());
}

#[tokio::test]
async fn differs_and_dirty_count() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commit(tmp.path()).await;
    let eng = engine(tmp.path()).await;

    let cp1 = eng.create().await.unwrap();
    let cp_same = eng.create().await.unwrap();
    assert!(!eng.differs(&cp1, &cp_same).await.unwrap());

    std::fs::write(tmp.path().join("a.txt"), "changed\n").unwrap();
    let cp2 = eng.create().await.unwrap();
    assert!(eng.differs(&cp1, &cp2).await.unwrap());

    assert_eq!(eng.worktree_dirty_since(&cp1).await.unwrap(), 1);
    assert_eq!(eng.worktree_dirty_since(&cp2).await.unwrap(), 0);
}

#[tokio::test]
async fn pruned_object_reports_missing() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo_with_commit(tmp.path()).await;
    let eng = engine(tmp.path()).await;

    std::fs::write(tmp.path().join("a.txt"), "snap\n").unwrap();
    let cp = eng.create().await.unwrap();
    git(tmp.path(), &["prune", "--expire=now"]).await;
    // The dangling commit chain is gone; restore must degrade, not panic.
    match eng.restore(&cp).await {
        Err(CheckpointError::ObjectMissing) => {}
        other => panic!("expected ObjectMissing, got {other:?}"),
    }
}

#[tokio::test]
async fn submodule_contents_not_captured() {
    // Accepted limitation: `add --all` stages a submodule as a gitlink; its
    // inner working tree is not part of the parent checkpoint.
    let tmp = tempfile::tempdir().unwrap();
    let sub_src = tmp.path().join("subsrc");
    std::fs::create_dir(&sub_src).unwrap();
    init_repo_with_commit(&sub_src).await;

    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    init_repo_with_commit(&main).await;
    // `-c protocol.file.allow=always` required since git 2.38 for local-path submodules.
    git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.to_str().unwrap(),
            "sub",
        ],
    )
    .await;
    git(&main, &["commit", "-q", "-m", "add sub"]).await;

    let eng = engine(&main).await;
    let cp = eng.create().await.unwrap();

    std::fs::write(main.join("sub/a.txt"), "inner change\n").unwrap();
    eng.restore(&cp).await.unwrap();
    assert_eq!(
        read(&main, "sub/a.txt"),
        "inner change\n",
        "submodule contents untouched by parent restore"
    );
}

#[test]
fn version_gate_parses() {
    assert!(supports_restore("git version 2.23.0"));
    assert!(supports_restore("git version 2.47.1 (Apple Git-153)"));
    assert!(supports_restore("git version 3.0.0"));
    assert!(!supports_restore("git version 2.22.9"));
    assert!(!supports_restore("nonsense"));
}
