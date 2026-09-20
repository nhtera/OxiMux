//! Integration tests for stash operations on `Repository`: `is_dirty`,
//! `stash_push`, `stash_list`, `stash_apply`, `stash_pop`, `stash_drop`.
//! Tempdir + real `git` binary on PATH.

mod common;

use common::{init_repo, run_git, write};
use oximux_git::{GitError, Repository};

#[tokio::test]
async fn is_dirty_clean_repo_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    assert!(!repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn is_dirty_with_unstaged_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    assert!(repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn is_dirty_with_staged_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    run_git(p, &["add", "a.txt"]);
    let repo = Repository::open(p).await.unwrap();
    assert!(repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn is_dirty_ignores_untracked_files() {
    // `is_dirty` uses --untracked-files=no so an untracked-only worktree is
    // considered clean. This mirrors `git stash push` default behavior.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("untracked.txt"), "new\n");
    let repo = Repository::open(p).await.unwrap();
    assert!(!repo.is_dirty().await.unwrap());
}

#[tokio::test]
async fn stash_push_captures_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false, &[]).await.unwrap();
    assert_eq!(r.index, 0);
    // Working tree is clean post-push and the file is back to v1.
    assert!(!repo.is_dirty().await.unwrap());
    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v1\n");
    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn stash_push_with_message() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("my work in progress"), false, &[])
        .await
        .unwrap();

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].branch, "main");
    assert_eq!(list[0].message, "my work in progress");
}

#[tokio::test]
async fn stash_push_include_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("untracked.txt"), "secret\n");
    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some("with untracked"), true, &[]).await.unwrap();

    // Untracked file is gone (stashed), and stash list has the entry.
    assert!(!p.join("untracked.txt").exists());
    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn stash_push_clean_tree_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo.stash_push(None, false, &[]).await.unwrap_err();
    assert!(
        matches!(err, GitError::InvalidInput { .. }),
        "expected InvalidInput on clean tree, got {err:?}"
    );
    assert_eq!(repo.stash_list(true).await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_push_message_special_chars() {
    // `!` `"` `'` are arg-passed (no shell), so they round-trip literally.
    // Newline in a stash message has no good test — git collapses it.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let msg = r#"fix!: "shouldn't" break 'parser'"#;
    let repo = Repository::open(p).await.unwrap();
    repo.stash_push(Some(msg), false, &[]).await.unwrap();
    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list[0].message, msg);
}

#[tokio::test]
async fn stash_list_ordering_most_recent_first() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    // First stash.
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("older"), false, &[]).await.unwrap();
    // Second stash on top.
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("newer"), false, &[]).await.unwrap();

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].stash_ref.index, 0);
    assert_eq!(list[0].message, "newer");
    assert_eq!(list[1].stash_ref.index, 1);
    assert_eq!(list[1].message, "older");
}

#[tokio::test]
async fn stash_apply_leaves_entry_on_stack() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false, &[]).await.unwrap();
    repo.stash_apply(&r).await.unwrap();

    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v2\n", "apply should restore worktree");
    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        1,
        "apply leaves entry on stack"
    );
}

#[tokio::test]
async fn stash_pop_removes_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false, &[]).await.unwrap();
    repo.stash_pop(&r).await.unwrap();

    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v2\n");
    assert_eq!(repo.stash_list(true).await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_drop_removes_without_applying() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    write(&p.join("a.txt"), "v2\n");
    let repo = Repository::open(p).await.unwrap();
    let r = repo.stash_push(None, false, &[]).await.unwrap();
    // stash_push restored worktree to clean. drop must NOT bring v2 back.
    repo.stash_drop(&r).await.unwrap();
    let on_disk = std::fs::read_to_string(p.join("a.txt")).unwrap();
    assert_eq!(on_disk, "v1\n", "drop must not re-apply");
    assert_eq!(repo.stash_list(true).await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_pop_invalid_ref_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    init_repo(p);
    write(&p.join("a.txt"), "v1\n");
    run_git(p, &["add", "a.txt"]);
    run_git(p, &["commit", "-m", "init"]);

    let repo = Repository::open(p).await.unwrap();
    let err = repo
        .stash_pop(&oximux_core::StashRef { index: 99 })
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::NonZero { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Phase 2 — sha identity, file listing, pathspec scoping, TTL cache.
//
// Every git behavior asserted below was executed in a throwaway sandbox before
// being written; see the plan's "Verified git facts" table.
// ---------------------------------------------------------------------------

/// `init_repo` + one commit containing `files`, then open the `Repository`.
async fn repo_with(p: &std::path::Path, files: &[(&str, &str)]) -> Repository {
    init_repo(p);
    for (name, body) in files {
        let path = p.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        write(&path, body);
    }
    run_git(p, &["add", "-A"]);
    run_git(p, &["commit", "-m", "init"]);
    Repository::open(p).await.unwrap()
}

#[tokio::test]
async fn stash_list_carries_sha_timestamp_and_relative() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("dated entry"), false, &[])
        .await
        .unwrap();

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 1);
    let e = &list[0];
    assert_eq!(e.sha.len(), 40, "full sha, got {:?}", e.sha);
    assert!(e.sha.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(e.created_at > 1_600_000_000, "plausible unix ts");
    assert!(!e.relative.is_empty(), "git's %cr must be populated");
    assert_eq!(e.message, "dated entry");
    assert_eq!(e.branch, "main");
    // The sha is a usable revision for the read-only verbs.
    assert!(repo.stash_files(&e.sha).await.is_ok());
}

#[tokio::test]
async fn stash_list_roundtrips_pipe_and_colon_in_message() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    // `|` and `: ` both used to be delimiters in the old parser.
    let msg = "feat(api): pipe | and : colons";
    repo.stash_push(Some(msg), false, &[]).await.unwrap();

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].message, msg);
    assert_eq!(list[0].branch, "main");
}

#[tokio::test]
async fn stash_files_lists_tracked_and_untracked_with_origin() {
    use oximux_core::StashFileOrigin;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n"), ("sub/b.txt", "s\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    write(&p.join("sub/b.txt"), "s2\n");
    write(&p.join("u.txt"), "new\n");
    repo.stash_push(Some("mixed"), true, &[]).await.unwrap();

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let files = repo.stash_files(&sha).await.unwrap();

    let tracked: Vec<_> = files
        .iter()
        .filter(|f| f.origin == StashFileOrigin::Tracked)
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    // Both tracked files must survive. Without `--first-parent` git emits a
    // combined diff and `sub/b.txt` silently vanishes — the regression this
    // asserts against.
    assert!(tracked.contains(&"a.txt".to_string()), "got {tracked:?}");
    assert!(
        tracked.contains(&"sub/b.txt".to_string()),
        "second tracked file dropped (combined-diff regression): {tracked:?}"
    );

    let untracked: Vec<_> = files
        .iter()
        .filter(|f| f.origin == StashFileOrigin::Untracked)
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(untracked, vec!["u.txt".to_string()]);
}

#[tokio::test]
async fn stash_files_without_untracked_returns_tracked_only() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    // No `-u`, so this stash commit has NO `^3` at all and `git show` on it
    // hard-errors. That must read as "no untracked", not as a failure.
    repo.stash_push(Some("tracked only"), false, &[])
        .await
        .unwrap();

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let files = repo.stash_files(&sha).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path.to_string_lossy(), "a.txt");
    assert_eq!(files[0].origin, oximux_core::StashFileOrigin::Tracked);
}

#[tokio::test]
async fn stash_files_reports_rename_as_one_record() {
    use oximux_core::DiffStatus;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("old.txt", "orig\n")]).await;
    run_git(p, &["mv", "old.txt", "new.txt"]);
    repo.stash_push(Some("renamed"), false, &[]).await.unwrap();

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let files = repo.stash_files(&sha).await.unwrap();
    assert_eq!(files.len(), 1, "one rename record, not add+delete: {files:?}");
    assert_eq!(files[0].path.to_string_lossy(), "new.txt");
    match &files[0].status {
        DiffStatus::Renamed { from, .. } => assert_eq!(from.to_string_lossy(), "old.txt"),
        other => panic!("expected Renamed, got {other:?}"),
    }
}

#[tokio::test]
async fn stash_files_path_with_newline_yields_one_record() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let weird = "we\nird.txt";
    let repo = repo_with(p, &[(weird, "nl\n")]).await;
    write(&p.join(weird), "nl2\n");
    repo.stash_push(Some("newline path"), false, &[])
        .await
        .unwrap();

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let files = repo.stash_files(&sha).await.unwrap();
    // Without `-z` this splits into two phantom rows, each of which would
    // render as a clickable file and be handed to a destructive restore.
    assert_eq!(files.len(), 1, "phantom rows from newline path: {files:?}");
    assert_eq!(files[0].path.to_string_lossy(), weird);
}

#[tokio::test]
async fn resolve_stash_index_tracks_drift_and_absence() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;

    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("first"), false, &[]).await.unwrap();
    let first = repo.stash_list(true).await.unwrap()[0].sha.clone();
    assert_eq!(
        repo.resolve_stash_index(&first, None).await.unwrap(),
        Some(oximux_core::StashRef { index: 0 })
    );

    // A second push shifts `first` down to index 1 — the drift a stored
    // `stash@{N}` would get wrong.
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("second"), false, &[]).await.unwrap();
    assert_eq!(
        repo.resolve_stash_index(&first, None).await.unwrap(),
        Some(oximux_core::StashRef { index: 1 }),
        "index must follow the sha, not be remembered"
    );

    // Dropped out-of-band → `None`, the only case a caller should abort on.
    run_git(p, &["stash", "drop", "stash@{1}"]);
    assert_eq!(repo.resolve_stash_index(&first, None).await.unwrap(), None);
    assert_eq!(
        repo.resolve_stash_index("0000000000000000000000000000000000000000", None)
            .await
            .unwrap(),
        None
    );
}

/// A sha is NOT unique on the stack, and the painted address is what tells
/// two entries for one commit apart.
///
/// Building the collision takes an interleaved store: `git stash store` twice
/// in a row on the same sha leaves ONE entry, because git skips the reflog
/// append when the ref value does not change. Verified — the obvious repro
/// does not reproduce it.
#[tokio::test]
async fn a_hint_disambiguates_two_entries_that_share_one_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;

    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("A"), false, &[]).await.unwrap();
    let a = repo.stash_list(true).await.unwrap()[0].sha.clone();
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("B"), false, &[]).await.unwrap();
    let b = repo.stash_list(true).await.unwrap()[0].sha.clone();

    run_git(p, &["stash", "clear"]);
    repo.stash_store(&a, "A").await.unwrap();
    repo.stash_store(&b, "B").await.unwrap();
    repo.stash_store(&a, "A2").await.unwrap();

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 3, "interleaved store should leave three: {list:?}");
    assert_eq!(list[0].sha, a);
    assert_eq!(list[2].sha, a, "one commit at two addresses");

    // Without the hint, both rows resolve to the FIRST match — so an op fired
    // from stash@{2} would act on stash@{0}.
    assert_eq!(
        repo.resolve_stash_index(&a, None).await.unwrap(),
        Some(oximux_core::StashRef { index: 0 })
    );
    // With it, each row keeps its own entry.
    assert_eq!(
        repo.resolve_stash_index(&a, Some(2)).await.unwrap(),
        Some(oximux_core::StashRef { index: 2 }),
        "the painted address should win when it still names this sha"
    );
    assert_eq!(
        repo.resolve_stash_index(&a, Some(0)).await.unwrap(),
        Some(oximux_core::StashRef { index: 0 })
    );
}

/// The hint must never override drift-healing: an address that no longer
/// names this sha is stale, and falling back to the live position is the
/// whole reason ops resolve by sha in the first place.
#[tokio::test]
async fn a_stale_hint_falls_back_to_the_live_position() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;

    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("first"), false, &[]).await.unwrap();
    let first = repo.stash_list(true).await.unwrap()[0].sha.clone();
    // Something else stashes; `first` slides to index 1 while the row on
    // screen still says stash@{0}.
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("second"), false, &[]).await.unwrap();

    assert_eq!(
        repo.resolve_stash_index(&first, Some(0)).await.unwrap(),
        Some(oximux_core::StashRef { index: 1 }),
        "a stale hint must not pin the op to the wrong stash"
    );
    // An address past the end of the stack is stale in the same way.
    assert_eq!(
        repo.resolve_stash_index(&first, Some(99)).await.unwrap(),
        Some(oximux_core::StashRef { index: 1 })
    );
}

#[tokio::test]
async fn stash_drop_returns_the_sha_git_actually_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("doomed"), false, &[]).await.unwrap();
    let entry = repo.stash_list(true).await.unwrap()[0].clone();

    let dropped = repo.stash_drop(&entry.stash_ref).await.unwrap();
    // The whole point: the caller can now prove the intended stash died,
    // rather than trusting that the index was still correct.
    assert_eq!(dropped, entry.sha);
    assert_eq!(repo.stash_list(true).await.unwrap().len(), 0);
}

#[tokio::test]
async fn stash_drop_on_shifted_stack_reports_the_wrong_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("target"), false, &[]).await.unwrap();
    let target = repo.stash_list(true).await.unwrap()[0].clone();

    // Someone else pushes: `target` is now at index 1, but our stale ref says 0.
    write(&p.join("a.txt"), "v3\n");
    run_git(p, &["stash", "push", "-m", "intruder"]);

    let dropped = repo.stash_drop(&target.stash_ref).await.unwrap();
    assert_ne!(
        dropped, target.sha,
        "a stale index drops the WRONG stash — this is why the return value exists"
    );
    // And recovery is possible, because the commit itself was never rewritten.
    repo.stash_store(&dropped, "recovered").await.unwrap();
    let shas: Vec<String> = repo
        .stash_list(true)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.sha)
        .collect();
    assert!(shas.contains(&dropped), "dropped sha recoverable: {shas:?}");
    assert!(shas.contains(&target.sha), "target survived: {shas:?}");
}

#[tokio::test]
async fn stash_store_writes_message_literally_with_no_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("original"), false, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    // Drop first: `stash store` is a verified no-op while the sha is still on
    // the stack, so it cannot relabel a live entry.
    repo.stash_drop(&oximux_core::StashRef { index: 0 })
        .await
        .unwrap();
    repo.stash_store(&sha, "solo literal msg").await.unwrap();

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].sha, sha);
    // No synthesized `On <branch>: ` prefix — the old parser rejected this
    // shape outright as "missing branch field".
    assert_eq!(list[0].branch, "", "stored entries carry no branch");
    assert_eq!(list[0].message, "solo literal msg");
}

#[tokio::test]
async fn stash_push_with_paths_stashes_only_the_named_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(
        p,
        &[("a.txt", "a\n"), ("sub/b.txt", "b\n"), ("c.txt", "c\n")],
    )
    .await;
    write(&p.join("a.txt"), "a2\n");
    write(&p.join("sub/b.txt"), "b2\n");
    write(&p.join("c.txt"), "c2\n");

    let named: Vec<&std::path::Path> = vec![
        std::path::Path::new("a.txt"),
        std::path::Path::new("sub/b.txt"),
    ];
    repo.stash_push(Some("subset"), false, &named).await.unwrap();

    // The unnamed file stays dirty.
    let leftover = std::fs::read_to_string(p.join("c.txt")).unwrap();
    assert_eq!(leftover, "c2\n", "unselected file must stay dirty");
    // The named ones were reverted.
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "a\n");

    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let mut paths: Vec<String> = repo
        .stash_files(&sha)
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
}

#[tokio::test]
async fn stash_push_with_paths_preserves_the_staged_split() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "a\n"), ("c.txt", "c\n")]).await;
    write(&p.join("a.txt"), "a2\n"); // unstaged
    write(&p.join("c.txt"), "c2\n");
    run_git(p, &["add", "c.txt"]); // staged

    let named: Vec<&std::path::Path> =
        vec![std::path::Path::new("a.txt"), std::path::Path::new("c.txt")];
    repo.stash_push(Some("split"), false, &named).await.unwrap();

    // `^1` is the base, `^2` the staged-index commit. They differ by exactly
    // the staged file, which proves the split survived the pathspec push.
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", &format!("{sha}^1"), &format!("{sha}^2")])
        .current_dir(p)
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&out.stdout);
    assert_eq!(names.trim(), "c.txt", "staged/unstaged split lost");
}

#[tokio::test]
async fn stash_push_with_glob_named_path_is_literal() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    // `g[1].txt` would glob-match `g1.txt` without `:(literal)`.
    let repo = repo_with(p, &[("g[1].txt", "g\n"), ("g1.txt", "one\n")]).await;
    write(&p.join("g[1].txt"), "g2\n");
    write(&p.join("g1.txt"), "one2\n");

    let named: Vec<&std::path::Path> = vec![std::path::Path::new("g[1].txt")];
    repo.stash_push(Some("globby"), false, &named).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(p.join("g[1].txt")).unwrap(),
        "g\n",
        "the named file was stashed"
    );
    assert_eq!(
        std::fs::read_to_string(p.join("g1.txt")).unwrap(),
        "one2\n",
        "g1.txt was never named and must stay dirty"
    );
}

#[tokio::test]
async fn stash_restore_file_is_literal_and_spares_glob_siblings() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a[1].rs", "B1\n"), ("a1.rs", "B1\n")]).await;
    write(&p.join("a[1].rs"), "S1\n");
    write(&p.join("a1.rs"), "S1\n");
    repo.stash_push(Some("both dirty"), false, &[])
        .await
        .unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    // Worktree is back at the committed state; restore ONLY the bracketed file.
    repo.stash_restore_file(&sha, std::path::Path::new("a[1].rs"))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(p.join("a[1].rs")).unwrap(),
        "S1\n",
        "named file restored from the stash"
    );
    // A bare pathspec makes git glob-match and clobber this file too —
    // verified in a sandbox, and the reason this routes through
    // `run_pathspec_op`.
    assert_eq!(
        std::fs::read_to_string(p.join("a1.rs")).unwrap(),
        "B1\n",
        "a1.rs was never named and must NOT be clobbered"
    );
}

#[tokio::test]
async fn stash_branch_consumes_the_stash() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("to branch"), false, &[])
        .await
        .unwrap();
    let entry = repo.stash_list(true).await.unwrap()[0].clone();

    repo.stash_branch("from-stash", &entry.stash_ref)
        .await
        .unwrap();

    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        0,
        "stash branch consumes the entry on success"
    );
    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "v2\n",
        "the stashed change is applied on the new branch"
    );
    assert_eq!(
        oximux_git::head_branch(p).await.unwrap().as_deref(),
        Some("from-stash")
    );
}

#[tokio::test]
async fn stash_branch_rejects_empty_name() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(None, false, &[]).await.unwrap();
    let err = repo
        .stash_branch("", &oximux_core::StashRef { index: 0 })
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }));
}

#[tokio::test]
async fn stash_list_cache_serves_stale_then_force_reads_through() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("first"), false, &[]).await.unwrap();

    // Warm the cache.
    assert_eq!(repo.stash_list(false).await.unwrap().len(), 1);

    // An EXTERNAL writer (the user's terminal) pushes. Our own ops invalidate
    // the cache; this one cannot, so the cached read is legitimately stale.
    write(&p.join("a.txt"), "v3\n");
    run_git(p, &["stash", "push", "-m", "external"]);

    assert_eq!(
        repo.stash_list(false).await.unwrap().len(),
        1,
        "within the TTL the cached list is served"
    );
    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        2,
        "force_refresh reads through to the live stack"
    );
}

#[tokio::test]
async fn our_own_ops_invalidate_the_stash_list_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("first"), false, &[]).await.unwrap();
    assert_eq!(repo.stash_list(false).await.unwrap().len(), 1); // warm

    // A push through THIS Repository must be visible to an unforced read —
    // otherwise the panel shows a stash the user just created as absent.
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("ours"), false, &[]).await.unwrap();
    assert_eq!(
        repo.stash_list(false).await.unwrap().len(),
        2,
        "stash_push must invalidate the cache"
    );

    repo.stash_drop(&oximux_core::StashRef { index: 0 })
        .await
        .unwrap();
    assert_eq!(
        repo.stash_list(false).await.unwrap().len(),
        1,
        "stash_drop must invalidate the cache"
    );
}

#[tokio::test]
async fn stash_list_cache_is_shared_across_cloned_handles() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    repo.stash_push(Some("first"), false, &[]).await.unwrap();

    let clone = repo.clone();
    assert_eq!(clone.stash_list(false).await.unwrap().len(), 1); // warm via clone

    // A mutation on one handle must clear the cache the other handle reads,
    // or two panels in the same window disagree about the stack.
    write(&p.join("a.txt"), "v3\n");
    repo.stash_push(Some("ours"), false, &[]).await.unwrap();
    assert_eq!(
        clone.stash_list(false).await.unwrap().len(),
        2,
        "cache must be shared across cloned handles"
    );
}

#[tokio::test]
async fn stash_push_with_paths_matching_nothing_dirty_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "a\n"), ("b.txt", "b\n")]).await;
    write(&p.join("a.txt"), "a2\n"); // dirty, but NOT named

    // Verified: git prints "No local changes to save" and exits 0 here, so the
    // whole-tree guard covers the path-scoped case too. Phase 7 relies on this
    // to tell the user their selection stashed nothing.
    let named: Vec<&std::path::Path> = vec![std::path::Path::new("b.txt")];
    let err = repo
        .stash_push(Some("nothing"), false, &named)
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::InvalidInput { .. }), "got {err:?}");

    assert_eq!(repo.stash_list(true).await.unwrap().len(), 0);
    // The unnamed dirty file is untouched — no partial side effect.
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "a2\n");
}

#[tokio::test]
async fn stash_list_survives_messages_containing_any_printable_delimiter() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;

    // Every byte that is NOT NUL is reachable in a stash message — verified:
    // git round-trips raw \x1f and \x1e untouched, alongside the obvious
    // `|` and `:`. The worst case is the last one: a full 40-hex object name
    // and a valid timestamp wrapped in the old separators. Under the old
    // scheme that parsed as a REAL extra record, silently shifting every index
    // below it so Drop destroyed an unselected stash.
    let forged = format!(
        "craft\x1e{}\x1f1700000000\x1f9 days ago\x1fOn main: PHANTOM",
        "d".repeat(40)
    );
    let messages = [
        "pipe | and : colon",
        "old\x1erecord separator",
        "old\x1funit separator",
        forged.as_str(),
        "plain",
    ];
    for m in messages {
        write(&p.join("a.txt"), &format!("{m}-dirty\n"));
        repo.stash_push(Some(m), false, &[]).await.unwrap();
    }

    let list = repo.stash_list(true).await.unwrap();
    assert_eq!(
        list.len(),
        messages.len(),
        "one entry per push, no phantoms: {list:#?}"
    );
    // Stack is most-recent-first, so the pushes read back reversed.
    for (i, m) in messages.iter().rev().enumerate() {
        assert_eq!(list[i].message, *m, "message {i} corrupted");
        assert_eq!(list[i].stash_ref.index, i, "index {i} shifted");
        assert_eq!(list[i].sha.len(), 40);
    }

    // The decisive check: every parsed index still addresses what git says it
    // does. A phantom record would desynchronise these and a Drop would hit
    // the wrong stash.
    for e in &list {
        assert_eq!(
            repo.resolve_stash_index(&e.sha, None).await.unwrap(),
            Some(e.stash_ref.clone()),
            "parsed index disagrees with git for {:?}",
            e.message
        );
    }
}

#[tokio::test]
async fn stash_files_surfaces_an_unreadable_untracked_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "v1\n")]).await;
    write(&p.join("a.txt"), "v2\n");
    write(&p.join("u.txt"), "new\n");
    repo.stash_push(Some("mixed"), true, &[]).await.unwrap();
    let sha = repo.stash_list(true).await.unwrap()[0].sha.clone();

    // Sanity: the untracked file is visible before we break anything.
    let before = repo.stash_files(&sha).await.unwrap();
    assert_eq!(before.len(), 2, "tracked + untracked: {before:#?}");

    // Destroy the `^3` commit object so the revision still RESOLVES but cannot
    // be read. Blanket-swallowing every `^3` error would silently drop the
    // untracked half here and tell the user their file was never stashed.
    let third = std::process::Command::new("git")
        .args(["rev-parse", &format!("{sha}^3")])
        .current_dir(p)
        .output()
        .unwrap();
    let third = String::from_utf8(third.stdout).unwrap().trim().to_string();
    assert_eq!(third.len(), 40);
    let obj = p
        .join(".git/objects")
        .join(&third[..2])
        .join(&third[2..]);
    // A packed object would need repacking to remove; this stash was just
    // created, so it is loose.
    assert!(obj.exists(), "expected a loose object at {obj:?}");
    std::fs::remove_file(&obj).unwrap();

    let err = repo.stash_files(&sha).await;
    assert!(
        err.is_err(),
        "an unreadable ^3 must surface, not silently drop the untracked half: {err:#?}"
    );
}

#[tokio::test]
async fn stash_branch_failure_can_still_leave_a_new_branch_checked_out() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "base\n")]).await;
    write(&p.join("a.txt"), "stashed\n");
    repo.stash_push(Some("s"), false, &[]).await.unwrap();

    // Dirty the same path so the apply cannot land.
    write(&p.join("a.txt"), "dirty-uncommitted\n");
    let err = repo
        .stash_branch("fresh", &oximux_core::StashRef { index: 0 })
        .await;
    assert!(err.is_err(), "apply is blocked, so the call fails");

    // ...but git had ALREADY created and switched to the branch. An Err here
    // does NOT mean "nothing happened" — callers must refresh HEAD on the
    // error path, and confirm copy must not promise atomicity.
    assert_eq!(
        oximux_git::head_branch(p).await.unwrap().as_deref(),
        Some("fresh"),
        "failed stash branch still moved HEAD"
    );
    assert_eq!(
        repo.stash_list(true).await.unwrap().len(),
        1,
        "the stash survives the failure"
    );
}

#[tokio::test]
async fn stash_branch_name_collision_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let repo = repo_with(p, &[("a.txt", "base\n")]).await;
    write(&p.join("a.txt"), "stashed\n");
    repo.stash_push(Some("s"), false, &[]).await.unwrap();
    run_git(p, &["branch", "existing"]);

    let err = repo
        .stash_branch("existing", &oximux_core::StashRef { index: 0 })
        .await;
    assert!(err.is_err());
    // The clean refusal: git rejects before touching anything.
    assert_eq!(
        oximux_git::head_branch(p).await.unwrap().as_deref(),
        Some("main")
    );
    assert_eq!(repo.stash_list(true).await.unwrap().len(), 1);
}
