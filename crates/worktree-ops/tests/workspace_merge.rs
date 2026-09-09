//! Integration tests for the merge pre-flight and `apply_merge` — real `git`
//! binary in a tempdir, same style as `workspace_rename_rollback.rs`.
//!
//! Two things are under test and they are different questions:
//!
//! 1. **The refusals**, each of which must fire before anything is touched. A
//!    merge discovered to be a bad idea *after* the auto-stash is the failure
//!    mode this module exists to design out.
//! 2. **The outcome cross-product** — {clean root, dirty root} × {fast-forward,
//!    true merge, already-up-to-date, conflict}. `MergeOutcome::AutoStashed`
//!    *wraps* another variant rather than sitting beside it, so a dirty root
//!    changes the SHAPE of every outcome, not just one of them. Testing five
//!    flat rows would miss the whole dirty column.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use oximux_core::{MergeOutcome, Workspace};
use oximux_git::Repository;
use oximux_worktree_ops::{
    MergeRefusal, MergeResult, apply_merge, merge_into_default, preflight_merge,
};

fn run_git(cwd: &Path, args: &[&str]) {
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

/// `git` that is allowed to fail (used to *start* a conflicted operation).
fn try_git(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git not on PATH")
        .status
        .success()
}

fn init_repo(cwd: &Path) {
    run_git(cwd, &["init", "-b", "main"]);
    run_git(cwd, &["config", "commit.gpgsign", "false"]);
    run_git(cwd, &["config", "user.name", "Test"]);
    run_git(cwd, &["config", "user.email", "test@example.com"]);
    std::fs::write(cwd.join("a.txt"), "v1\n").expect("write seed");
    run_git(cwd, &["add", "a.txt"]);
    run_git(cwd, &["commit", "-m", "init"]);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    _wt_root: tempfile::TempDir,
    project_root: PathBuf,
    wt_path: PathBuf,
    workspace: Workspace,
}

impl Fixture {
    /// Commit `contents` to `file` inside the worktree, so its branch has
    /// something the default branch lacks.
    fn commit_in_worktree(&self, file: &str, contents: &str, msg: &str) {
        std::fs::write(self.wt_path.join(file), contents).expect("write in worktree");
        run_git(&self.wt_path, &["add", file]);
        run_git(&self.wt_path, &["commit", "-m", msg]);
    }

    /// Commit in the project root, so the two branches diverge and the merge
    /// cannot fast-forward.
    fn commit_in_root(&self, file: &str, contents: &str, msg: &str) {
        std::fs::write(self.project_root.join(file), contents).expect("write in root");
        run_git(&self.project_root, &["add", file]);
        run_git(&self.project_root, &["commit", "-m", msg]);
    }

    /// Leave an uncommitted tracked edit in the project root, so `merge_branch`
    /// auto-stashes.
    fn dirty_the_root(&self) {
        std::fs::write(self.project_root.join("a.txt"), "dirty\n").expect("dirty root");
    }

    async fn repo(&self) -> Repository {
        Repository::open(&self.project_root).await.expect("open repo")
    }

    async fn head(&self) -> String {
        self.repo().await.head_sha().await.expect("head")
    }

    async fn stash_count(&self) -> usize {
        self.repo().await.stash_list().await.expect("stash list").len()
    }

    async fn merge(&self) -> MergeResult {
        merge_into_default(
            &self.project_root,
            &self.workspace,
            "main",
            &HashSet::new(),
        )
        .await
    }
}

/// A repo on `main` with one linked worktree on `oximux/<slug>` and a row that
/// names it. No commits on the branch yet — each test adds what it needs.
async fn fixture(slug: &str) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path().to_path_buf();
    init_repo(&project_root);

    let wt_root = tempfile::tempdir().expect("wt tempdir");
    let wt_path = wt_root.path().join(slug);
    Repository::open(&project_root)
        .await
        .expect("open repo")
        .add_worktree(&wt_path, &format!("oximux/{slug}"))
        .await
        .expect("add worktree");

    let workspace = Workspace {
        id: "ws1".into(),
        project_id: "p1".into(),
        name: slug.into(),
        slug: slug.into(),
        branch: format!("oximux/{slug}"),
        worktree_path: wt_path.to_string_lossy().into_owned(),
        status: "active".into(),
        created_at: String::new(),
        archived_at: None,
        linked_issue: None,
        tint: None,
        sort_order: 0.0,
        pinned: false,
        comment: String::new(),
        phase: String::new(),
    };

    Fixture {
        _tmp: tmp,
        _wt_root: wt_root,
        project_root,
        wt_path,
        workspace,
    }
}

fn refusal(result: MergeResult) -> MergeRefusal {
    match result {
        MergeResult::Refused(r) => r,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn failure(result: MergeResult) -> (String, bool) {
    match result {
        MergeResult::Failed {
            error,
            stranded_auto_stash,
        } => (error, stranded_auto_stash),
        other => panic!("expected a hard failure, got {other:?}"),
    }
}

fn outcome(result: MergeResult) -> MergeOutcome {
    match result {
        MergeResult::Completed(o) => o,
        other => panic!("expected a completed merge, got {other:?}"),
    }
}

/// Peel the `AutoStashed` wrapper the way a caller must: it wraps another
/// variant rather than sitting beside it.
fn peel(outcome: MergeOutcome) -> (Option<usize>, bool, MergeOutcome) {
    match outcome {
        MergeOutcome::AutoStashed {
            stash_ref,
            inner,
            pop_failed,
        } => (Some(stash_ref.index), pop_failed, *inner),
        other => (None, false, other),
    }
}

// ---------------------------------------------------------------- happy paths

/// Clean root, branch strictly ahead: a fast-forward, and it lands in the
/// PROJECT ROOT — the direction that is silent and wrong if reversed.
#[tokio::test]
async fn a_branch_ahead_of_a_clean_default_fast_forwards_into_the_project_root() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    assert!(matches!(outcome(f.merge().await), MergeOutcome::FastForward));

    // The direction assertion: the file the worktree committed is now in the
    // project root, and the worktree branch was NOT rewritten to main's tip.
    assert!(
        f.project_root.join("b.txt").exists(),
        "the branch must land in the project root"
    );
    let wt_head = Repository::open(&f.wt_path)
        .await
        .expect("open worktree")
        .head_sha()
        .await
        .expect("worktree head");
    assert_eq!(wt_head, f.head().await, "fast-forward puts both at one tip");
}

/// Clean root, both sides ahead: a true merge commit, not a fast-forward.
#[tokio::test]
async fn diverged_histories_produce_a_true_merge_on_a_clean_root() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    f.commit_in_root("c.txt", "c\n", "add c");

    assert!(matches!(outcome(f.merge().await), MergeOutcome::Merged));
    assert!(f.project_root.join("b.txt").exists());
    assert!(f.project_root.join("c.txt").exists());
}

// -------------------------------------------------- the dirty-root cross product

/// Dirty root + fast-forward: the outcome is `AutoStashed` WRAPPING
/// `FastForward`, the stash is popped, and the user's edit is back.
#[tokio::test]
async fn a_dirty_root_wraps_a_fast_forward_and_restores_the_edit() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    f.dirty_the_root();

    let (stash, pop_failed, inner) = peel(outcome(f.merge().await));
    assert!(stash.is_some(), "a dirty root must auto-stash");
    assert!(!pop_failed, "the pop must succeed when nothing conflicts");
    assert!(matches!(inner, MergeOutcome::FastForward), "{inner:?}");

    assert_eq!(
        std::fs::read_to_string(f.project_root.join("a.txt")).unwrap(),
        "dirty\n",
        "the popped stash must put the user's edit back"
    );
    assert_eq!(f.stash_count().await, 0, "a popped stash leaves the stack empty");
}

/// Dirty root + a true merge: same wrapper, different inner.
#[tokio::test]
async fn a_dirty_root_wraps_a_true_merge_too() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    f.commit_in_root("c.txt", "c\n", "add c");
    f.dirty_the_root();

    let (stash, pop_failed, inner) = peel(outcome(f.merge().await));
    assert!(stash.is_some());
    assert!(!pop_failed);
    assert!(matches!(inner, MergeOutcome::Merged), "{inner:?}");
}

/// **The trap the flat five-row table walks into.** A dirty root plus a branch
/// with nothing to give would yield `AutoStashed { inner: AlreadyUpToDate }`,
/// which a caller matching `AutoStashed` as a peer of the others reports as a
/// successful merge.
///
/// The pre-flight answers it first, so `git merge` never runs and the dirty
/// tree is never stashed at all — which is the stronger guarantee. The
/// mapping-side half of this case (peeling the wrapper) is asserted in
/// `apps/desktop/tests/workspace_merge_report.rs`.
#[tokio::test]
async fn a_branch_with_nothing_to_give_is_refused_before_a_dirty_tree_is_stashed() {
    let f = fixture("feat").await;
    // No commits on the branch: it is already an ancestor of main.
    f.dirty_the_root();

    assert_eq!(refusal(f.merge().await), MergeRefusal::NothingToMerge);
    assert_eq!(
        f.stash_count().await,
        0,
        "a no-op merge must not stash the user's work on the way to doing nothing"
    );
    assert_eq!(
        std::fs::read_to_string(f.project_root.join("a.txt")).unwrap(),
        "dirty\n",
        "the working tree must be untouched"
    );
}

// ------------------------------------------------------------------- conflicts

/// A genuine conflict: the paths come back, and the working tree is left in the
/// conflicted state for the existing Source Control surface to render.
#[tokio::test]
async fn a_conflicting_merge_names_the_paths_on_a_clean_root() {
    let f = fixture("feat").await;
    f.commit_in_worktree("a.txt", "theirs\n", "edit a in worktree");
    f.commit_in_root("a.txt", "ours\n", "edit a in root");

    let MergeOutcome::Conflicted {
        conflicts,
        auto_stash,
    } = outcome(f.merge().await)
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflicts, vec![PathBuf::from("a.txt")]);
    assert!(
        auto_stash.is_none(),
        "a clean root has nothing to stash, so nothing is stranded"
    );
    assert_eq!(
        f.repo().await.current_operation(),
        Some(oximux_core::GitOperation::Merge),
        "the conflicted merge is left in progress for the user to resolve"
    );
}

/// Dirty root + conflict: the auto-stash is `Some` and is **not** popped. This
/// is the single point of data loss in the whole phase — if the caller drops
/// this ref the user's uncommitted work is findable only via `git stash list`.
#[tokio::test]
async fn a_conflicting_merge_on_a_dirty_root_strands_the_stash_on_the_stack() {
    let f = fixture("feat").await;
    f.commit_in_worktree("a.txt", "theirs\n", "edit a in worktree");
    f.commit_in_root("b.txt", "b\n", "add b");
    // Conflict on a.txt requires main to have touched it too.
    f.commit_in_root("a.txt", "ours\n", "edit a in root");
    // ...and something ELSE uncommitted, so there is a stash to strand.
    std::fs::write(f.project_root.join("b.txt"), "dirty b\n").expect("dirty b");

    let MergeOutcome::Conflicted {
        conflicts,
        auto_stash,
    } = outcome(f.merge().await)
    else {
        panic!("expected a conflict");
    };
    assert_eq!(conflicts, vec![PathBuf::from("a.txt")]);
    let stash = auto_stash.expect("a dirty root must carry its stash out of a conflict");
    assert_eq!(
        f.stash_count().await,
        1,
        "the stash is still on the stack \u{2014} the caller owes the user a pointer to it"
    );
    assert_eq!(stash.index, 0);
}

/// `pop_failed: true` — the merge **succeeded**, only the pop did not. Forced
/// by dirtying the very file the merge rewrites, so the stashed edit no longer
/// applies to the merged result.
///
/// The distinction matters because both halves are true at once and the caller
/// has to say both: the branch landed, AND the user's uncommitted edit is still
/// on the stash stack.
#[tokio::test]
async fn a_stash_that_conflicts_with_the_merge_result_reports_a_failed_pop_not_a_failed_merge() {
    let f = fixture("feat").await;
    f.commit_in_worktree("a.txt", "theirs\n", "rewrite a in worktree");
    // Uncommitted edit to the SAME file the merge is about to rewrite.
    f.dirty_the_root();

    let (stash, pop_failed, inner) = peel(outcome(f.merge().await));
    assert!(stash.is_some());
    assert!(pop_failed, "the stash cannot apply to the merged result");
    assert!(
        matches!(inner, MergeOutcome::FastForward),
        "the merge itself succeeded: {inner:?}"
    );
    assert_eq!(
        f.stash_count().await,
        1,
        "a failed pop leaves the stash on the stack \u{2014} that is the pointer the \
         caller must not drop"
    );

    // ...and it leaves the project root CONFLICTED. This is the half that is
    // easy to miss: the merge succeeded, so a caller mapping on the inner
    // variant alone reports plain success while the user's working tree has
    // conflict markers in it and no banner says so.
    let conflicts = f
        .repo()
        .await
        .list_conflicting_paths()
        .await
        .expect("list conflicts");
    assert_eq!(
        conflicts,
        vec![PathBuf::from("a.txt")],
        "a failed stash pop leaves unmerged paths behind"
    );
    let text = std::fs::read_to_string(f.project_root.join("a.txt")).expect("read a.txt");
    assert!(
        text.contains("<<<<<<<"),
        "the markers are in the user's file: {text}"
    );
}

// ------------------------------------------------------------------- refusals

#[tokio::test]
async fn a_root_on_another_branch_is_refused_by_name() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    run_git(&f.project_root, &["switch", "-c", "release"]);

    let r = refusal(f.merge().await);
    assert_eq!(
        r,
        MergeRefusal::RootNotOnDefault {
            on: Some("release".into()),
            default: "main".into(),
        }
    );
    assert!(r.message(&f.workspace.branch).contains("release"));
}

/// The gate `crates/git/src/operation.rs` exists for. Without it a paused
/// rebase gets auto-stashed out from under the user.
#[tokio::test]
async fn a_paused_rebase_in_the_project_root_is_refused_by_name() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    // Build a conflicting side branch in the root and start a rebase that
    // pauses on it.
    f.commit_in_root("a.txt", "ours\n", "edit a in root");
    run_git(&f.project_root, &["switch", "-c", "side", "HEAD~1"]);
    f.commit_in_root("a.txt", "theirs\n", "edit a on side");
    assert!(
        !try_git(&f.project_root, &["rebase", "main"]),
        "the rebase must pause on a conflict for this test to mean anything"
    );
    // A paused rebase leaves HEAD detached, so `RootNotOnDefault` would also
    // be a true statement here. Asserting `OperationInProgress` instead is what
    // pins the ORDER: the operation gate has to be consulted first, or the user
    // is told to switch branches when the real answer is "finish your rebase".
    let r = refusal(f.merge().await);
    assert_eq!(
        r,
        MergeRefusal::OperationInProgress {
            operation: oximux_core::GitOperation::Rebase,
        },
        "the operation gate must fire ahead of the branch check"
    );
    assert!(r.message(&f.workspace.branch).contains("rebase"));
}

#[tokio::test]
async fn a_live_agent_in_the_project_root_refuses_the_merge_by_path() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    // An agent one level DOWN, which an equality check would miss.
    let holder = f.project_root.join("src");
    std::fs::create_dir_all(&holder).expect("mkdir");
    let holders: HashSet<PathBuf> = [holder.clone()].into_iter().collect();

    let result = merge_into_default(&f.project_root, &f.workspace, "main", &holders).await;
    let MergeRefusal::RootHeld { holder: named } = refusal(result) else {
        panic!("expected RootHeld");
    };
    assert_eq!(
        std::fs::canonicalize(&named).unwrap(),
        std::fs::canonicalize(&holder).unwrap()
    );
}

/// A worktree elsewhere on disk is not "inside the project root", so an agent
/// working in ANOTHER worktree must not block landing this one.
#[tokio::test]
async fn an_agent_in_a_different_worktree_does_not_block_the_merge() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    let holders: HashSet<PathBuf> = [f.wt_path.clone()].into_iter().collect();

    let result = merge_into_default(&f.project_root, &f.workspace, "main", &holders).await;
    assert!(matches!(outcome(result), MergeOutcome::FastForward));
}

// ------------------------------------------------- the hard-failure stash path

/// `merge_branch`'s recovery contract pops the auto-stash before returning
/// `Err`, so an ordinary hard failure leaves nothing behind and the checkout is
/// exactly where it started.
#[tokio::test]
async fn a_hard_merge_failure_pops_its_stash_and_strands_nothing() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");
    f.dirty_the_root();

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes");
    // Delete the branch out from under the plan: `git merge` now hard-fails
    // with "not something we can merge" AFTER the auto-stash was pushed.
    run_git(&f.project_root, &["worktree", "remove", "--force", &f.wt_path.to_string_lossy()]);
    run_git(&f.project_root, &["branch", "-D", "oximux/feat"]);

    let (error, stranded) = failure(apply_merge(&plan, &HashSet::new()).await);
    assert!(!error.is_empty());
    assert!(!stranded, "the contract pops the stash on a hard failure: {error}");
    assert_eq!(f.stash_count().await, 0);
    assert_eq!(
        std::fs::read_to_string(f.project_root.join("a.txt")).unwrap(),
        "dirty\n",
        "\u{201c}merge failed = back to where I was\u{201d}"
    );
}

/// `stranded_auto_stash` must mean "THIS merge left one", not "there is one".
///
/// **What this test proves, precisely:** an auto-stash already on the stack —
/// left by an earlier merge, and already carrying its own recovery notice —
/// does not make this merge report a stranded stash. That is what rules out the
/// naive implementation (scan the stack for the auto-stash message), which
/// would raise a duplicate notice on every subsequent failure.
///
/// **What it does NOT prove:** the positive case. Making the pop fail after a
/// hard merge failure needs the working tree dirtied between `merge_branch`'s
/// stash push and its pop, inside a single call — a race a test cannot win
/// without a hook that does not exist. The positive branch is one comparison
/// (`after > before`) over the same two reads this test exercises, and the
/// notice it produces is asserted desktop-side in
/// `apps/desktop/tests/workspace_merge_report.rs`.
#[tokio::test]
async fn an_auto_stash_already_on_the_stack_is_not_reported_as_stranded_by_this_merge() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes");
    run_git(&f.project_root, &["worktree", "remove", "--force", &f.wt_path.to_string_lossy()]);
    run_git(&f.project_root, &["branch", "-D", "oximux/feat"]);
    // A pre-existing auto-stash from an EARLIER merge, already on the stack.
    // A naive "is there an entry with the auto-stash message?" check would call
    // this one stranded by THIS merge and raise a duplicate notice.
    f.dirty_the_root();
    f.repo()
        .await
        .stash_push(Some(oximux_git::AUTO_STASH_MESSAGE), false)
        .await
        .expect("seed an older auto-stash");
    assert_eq!(f.stash_count().await, 1);

    let (_, stranded) = failure(apply_merge(&plan, &HashSet::new()).await);
    assert!(
        !stranded,
        "a stash that was already there is not one this merge stranded"
    );
}

// ------------------------------------------------------- the unsynchronized gap

/// The pre-flight is advice, not a lock. A commit landing in the root between
/// the two phases would put the branch on a base the user never saw, so
/// `apply_merge` re-reads HEAD and aborts.
#[tokio::test]
async fn a_head_that_moved_between_preflight_and_merge_aborts() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes");
    let was = f.head().await;

    // Somebody else commits in the root while the user was deciding.
    f.commit_in_root("c.txt", "c\n", "add c");

    let r = refusal(apply_merge(&plan, &HashSet::new()).await);
    let MergeRefusal::HeadMoved { was: reported, now } = &r else {
        panic!("expected HeadMoved, got {r:?}");
    };
    assert_eq!(reported, &was);
    assert_ne!(now, &was);
    assert!(!f.project_root.join("b.txt").exists(), "nothing was merged");
}

/// A branch switch does not have to move HEAD's SHA, so the HEAD compare cannot
/// stand in for the branch check. `git switch -c` opens a new branch at the
/// current commit: the SHA is identical, and without its own re-check the merge
/// would advance a branch the user never named.
#[tokio::test]
async fn a_branch_switched_at_the_same_commit_after_the_preflight_aborts() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes");
    let was = f.head().await;

    // A new branch at the SAME commit — HEAD's SHA does not move.
    run_git(&f.project_root, &["switch", "-c", "release"]);
    assert_eq!(f.head().await, was, "precondition: the SHA is unchanged");

    let r = refusal(apply_merge(&plan, &HashSet::new()).await);
    let MergeRefusal::RootNotOnDefault { on, default } = &r else {
        panic!("expected RootNotOnDefault, got {r:?}");
    };
    assert_eq!(on.as_deref(), Some("release"));
    assert_eq!(default, "main");
    assert!(
        !f.project_root.join("b.txt").exists(),
        "nothing was merged onto release"
    );
}

/// The same hole, reached the other way: a detaching checkout also leaves the
/// SHA alone, and a merge commit landing on a detached HEAD becomes unreachable
/// at the next checkout.
#[tokio::test]
async fn a_head_detached_at_the_same_commit_after_the_preflight_aborts() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes");
    let was = f.head().await;

    run_git(&f.project_root, &["checkout", "--detach", "HEAD"]);
    assert_eq!(f.head().await, was, "precondition: the SHA is unchanged");

    let r = refusal(apply_merge(&plan, &HashSet::new()).await);
    assert!(
        matches!(r, MergeRefusal::RootNotOnDefault { .. }),
        "expected RootNotOnDefault, got {r:?}"
    );
    assert!(!f.project_root.join("b.txt").exists(), "nothing was merged");
}

/// An agent that starts during the pre-flight's git round-trips is invisible to
/// the snapshot the pre-flight used. The second read is the only thing that can
/// catch it.
#[tokio::test]
async fn an_agent_that_starts_after_the_preflight_still_stops_the_merge() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes with no holders");

    let holders_now: HashSet<PathBuf> = [f.project_root.clone()].into_iter().collect();
    assert!(matches!(
        refusal(apply_merge(&plan, &holders_now).await),
        MergeRefusal::RootHeld { .. }
    ));
    assert!(!f.project_root.join("b.txt").exists(), "nothing was merged");
}

/// A rebase started after the pre-flight leaves HEAD where the plan recorded
/// it in the fast-forward case, so the HEAD compare alone would not catch it.
#[tokio::test]
async fn an_operation_started_after_the_preflight_still_stops_the_merge() {
    let f = fixture("feat").await;
    f.commit_in_worktree("b.txt", "b\n", "add b");

    let plan = preflight_merge(&f.project_root, &f.workspace, "main", &HashSet::new())
        .await
        .expect("pre-flight passes");

    // Fabricate the sentinel a paused merge leaves behind, without moving HEAD.
    let head = f.head().await;
    std::fs::write(f.project_root.join(".git/MERGE_HEAD"), format!("{head}\n"))
        .expect("write sentinel");

    assert_eq!(
        refusal(apply_merge(&plan, &HashSet::new()).await),
        MergeRefusal::OperationInProgress {
            operation: oximux_core::GitOperation::Merge,
        },
    );
    assert!(!f.project_root.join("b.txt").exists(), "nothing was merged");
}
