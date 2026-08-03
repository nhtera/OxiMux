//! Integration tests for `StatusPoller`. Uses real git CLI in tempdirs and
//! short tick intervals to keep wall-clock cost low.

use oximux_git::{GitCmd, PollState, Repository, StatusPoller};
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use tokio::time::timeout;

/// Test helper — extract the `GitState` out of a `Ready` channel value or
/// panic. Keeps the assertion sites readable.
fn ready(state: &PollState) -> &oximux_core::GitState {
    match state {
        PollState::Ready(s) => s,
        other => panic!("expected Ready, got {other:?}"),
    }
}

const FAST_TICK: Duration = Duration::from_millis(40);
// Generous wait so CI scheduling jitter doesn't flake. A ceiling, not a sleep
// — green runs never wait it out — so it is sized for the worst platform:
// the failure-threshold test needs THREE serial git children after `.git`
// vanishes, and a Windows spawn behind an AV scan costs 100-300 ms apiece,
// which overran the earlier 750 ms budget.
const RECV_BUDGET: Duration = Duration::from_secs(5);

async fn git(repo: &std::path::Path, args: &[&str]) {
    GitCmd::new(repo)
        .args(args.iter().copied())
        .run()
        .await
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
}

async fn make_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-b", "main"]).await;
    let repo = Repository::open(tmp.path()).await.expect("open");
    (tmp, repo)
}

#[tokio::test]
async fn emits_initial_status_on_startup() {
    let (_tmp, repo) = make_repo().await;
    let poller = StatusPoller::spawn_with_interval(repo, FAST_TICK);
    let mut rx = poller.subscribe();
    // First emit may already be visible (initial value is None, then poll fires);
    // wait for `changed` to ensure we see the first poll's Some.
    timeout(RECV_BUDGET, rx.changed())
        .await
        .expect("first poll within budget")
        .expect("sender alive");
    let snap = rx.borrow_and_update().clone();
    let s = ready(&snap);
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert!(s.files.is_empty());
}

#[tokio::test]
async fn detects_filesystem_change() {
    let (tmp, repo) = make_repo().await;
    let poller = StatusPoller::spawn_with_interval(repo, FAST_TICK);
    let mut rx = poller.subscribe();

    // Wait for the initial Some.
    timeout(RECV_BUDGET, rx.changed())
        .await
        .expect("initial poll")
        .unwrap();
    rx.borrow_and_update();

    // Create an untracked file; the next tick should reflect it.
    fs::write(tmp.path().join("hello.txt"), b"hi\n").unwrap();
    timeout(RECV_BUDGET, rx.changed())
        .await
        .expect("change detected within budget")
        .unwrap();
    let snap = rx.borrow_and_update().clone();
    let s = ready(&snap);
    assert_eq!(s.files.len(), 1);
    assert_eq!(s.files[0].path.to_str(), Some("hello.txt"));
}

#[tokio::test]
async fn pauses_when_unfocused_and_resumes_on_focus() {
    let (tmp, repo) = make_repo().await;
    let poller = StatusPoller::spawn_with_interval(repo, FAST_TICK);
    let mut rx = poller.subscribe();

    // First emit.
    timeout(RECV_BUDGET, rx.changed())
        .await
        .expect("initial")
        .unwrap();
    rx.borrow_and_update();

    poller.set_focused(false);
    // Give the loop time to observe the focus drop and park on the gate.
    // 150 ms is conservative for loaded CI without slowing the test much.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Mutate filesystem; no emit should arrive while unfocused.
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();
    let elapsed = timeout(Duration::from_millis(200), rx.changed()).await;
    assert!(
        elapsed.is_err(),
        "expected no emit while unfocused, got: {elapsed:?}"
    );

    // Resume focus → the next tick should see the file and emit.
    poller.set_focused(true);
    timeout(RECV_BUDGET, rx.changed())
        .await
        .expect("resumed emit within budget")
        .unwrap();
    let snap = rx.borrow_and_update().clone();
    let s = ready(&snap);
    assert_eq!(s.files.len(), 1);
}

#[tokio::test]
async fn deduplicates_identical_status() {
    let (_tmp, repo) = make_repo().await;
    let poller = StatusPoller::spawn_with_interval(repo, FAST_TICK);
    let mut rx = poller.subscribe();

    // First emit.
    timeout(RECV_BUDGET, rx.changed()).await.unwrap().unwrap();
    rx.borrow_and_update();

    // With no filesystem change, several ticks should pass without another
    // `changed()` firing.
    let result = timeout(Duration::from_millis(250), rx.changed()).await;
    assert!(
        result.is_err(),
        "expected no duplicate emit, got: {result:?}"
    );
}

#[tokio::test]
async fn emits_failed_after_threshold_of_consecutive_errors() {
    // Open a real repo, wait for the first Ready sample, then delete `.git`
    // out from under the poller. After FAILURE_THRESHOLD=3 ticks, the channel
    // must transition to `PollState::Failed` (the M4 contract — UI should see
    // "git is broken" instead of silently sticking on the last good sample).
    let (tmp, repo) = make_repo().await;
    let poller = StatusPoller::spawn_with_interval(repo, FAST_TICK);
    let mut rx = poller.subscribe();

    timeout(RECV_BUDGET, rx.changed())
        .await
        .expect("initial poll")
        .unwrap();
    rx.borrow_and_update();

    fs::remove_dir_all(tmp.path().join(".git")).expect("rm .git");

    // 3 failures × FAST_TICK + scheduling slack. Cap the wait at ~750 ms.
    let deadline = std::time::Instant::now() + RECV_BUDGET;
    loop {
        if matches!(*rx.borrow_and_update(), PollState::Failed(_)) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "expected PollState::Failed within budget, last={:?}",
                rx.borrow().clone()
            );
        }
        let remaining = deadline - std::time::Instant::now();
        let _ = timeout(remaining, rx.changed()).await;
    }
}

#[tokio::test]
async fn drop_aborts_background_task() {
    let (_tmp, repo) = make_repo().await;
    let poller = StatusPoller::spawn_with_interval(repo, FAST_TICK);
    let rx = poller.subscribe();
    drop(poller);
    // After drop, the sender is gone — `changed()` resolves with Err.
    let mut rx = rx;
    let res = timeout(RECV_BUDGET, rx.changed()).await;
    assert!(
        matches!(res, Ok(Err(_))),
        "expected sender-closed error after drop, got: {res:?}"
    );
}
