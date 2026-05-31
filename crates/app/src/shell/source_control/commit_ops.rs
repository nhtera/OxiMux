//! Async execution machinery for the commit area's primary action AND
//! every dropdown-driven remote verb (Push / Pull / Sync / Fetch /
//! Commit & Push / Commit & Sync).
//!
//! Pulled out of `commit_area.rs` so that file stays under the 500-LOC
//! warn cap. All entry points still live as `pub fn` on `CommitArea`;
//! this module just hosts their bodies and the helper enum.
//!
//! Single-flight contract: every entry point swaps `in_flight` atomically
//! before scheduling work and resets it on completion. The status surface
//! adapts so the user sees which op is in flight.

use std::sync::atomic::Ordering;

use gpui::{Context, Window};
use tokio::sync::oneshot;

use crate::shell::source_control::commit_area::{CommitArea, CommitStatus};

/// Standalone remote ops the user can trigger from the commit dropdown.
/// Separate from `PrimaryActionKind` (which represents the resolved primary
/// button verb factoring in stage/commit gates); these are unconditional
/// dropdown items.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RemoteVerb {
    Push,
    Pull,
    Sync,
    Fetch,
    /// `git push -u origin <branch>` to publish a branch that has no
    /// upstream yet. Reuses the Push in-flight status — there's no
    /// dedicated `CommitStatus::Publishing` because the user-visible
    /// distinction is only meaningful in the dropdown menu.
    Publish,
}

impl RemoteVerb {
    pub fn label(self) -> &'static str {
        match self {
            RemoteVerb::Push => "push",
            RemoteVerb::Pull => "pull",
            RemoteVerb::Sync => "sync",
            RemoteVerb::Fetch => "fetch",
            RemoteVerb::Publish => "publish",
        }
    }

    pub fn in_flight_status(self) -> CommitStatus {
        match self {
            RemoteVerb::Push => CommitStatus::Pushing,
            RemoteVerb::Pull => CommitStatus::Pulling,
            RemoteVerb::Sync => CommitStatus::Syncing,
            RemoteVerb::Fetch => CommitStatus::Fetching,
            // Publishing a branch is functionally a push; reusing
            // `Pushing` keeps the status row consistent without a new
            // enum variant.
            RemoteVerb::Publish => CommitStatus::Pushing,
        }
    }
}

/// Run the commit pipeline with optional follow-up push/sync.
/// `followup_push` ⊕ `followup_sync` are mutually exclusive (asserted).
///
/// `window` is plumbed through so the completion task can root its
/// `cx.spawn_in(window, …)` — that's what gives the success-arm
/// access to a `&mut Window` inside `update_in`, which the textarea
/// auto-clear (`InputState::set_value`) requires.
pub fn run_commit(
    area: &mut CommitArea,
    followup_push: bool,
    followup_sync: bool,
    window: &mut Window,
    cx: &mut Context<CommitArea>,
) {
    debug_assert!(!(followup_push && followup_sync));
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    let message = area.message_state.read(cx).value().to_string();
    let trimmed = message.trim();
    if trimmed.is_empty() {
        area.in_flight.store(false, Ordering::SeqCst);
        area.status = CommitStatus::Failed("commit".to_string(), "Message is empty".to_string());
        return;
    }
    let message = trimmed.to_string();
    area.status = CommitStatus::Committing;
    let repo = area.repo.clone();
    let (tx, rx) = oneshot::channel::<Result<&'static str, (&'static str, String)>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                // Commit first. Bail out before push/sync if it fails.
                let _sha = match repo.commit(&message).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(Err(("commit", e.to_string())));
                        return;
                    }
                };
                if followup_push {
                    let _ = match repo.push().await {
                        Ok(_) => tx.send(Ok("push")),
                        Err(e) => tx.send(Err(("push", e.to_string()))),
                    };
                } else if followup_sync {
                    let _ = match repo.sync().await {
                        Ok(_) => tx.send(Ok("sync")),
                        Err(e) => tx.send(Err(("sync", e.to_string()))),
                    };
                } else {
                    let _ = tx.send(Ok("commit"));
                }
            });
        }
        Err(_) => {
            tracing::warn!(
                target: "oximux_app::source_control",
                "no tokio runtime entered; commit skipped"
            );
            area.status =
                CommitStatus::Failed("commit".to_string(), "no tokio runtime".to_string());
            area.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    }
    spawn_commit_completion(area, rx, window, cx);
}

/// History-rewriting / history-extending verbs fired from the commit
/// graph row's right-click context menu. Distinct from `RemoteVerb` —
/// these always carry a target commit SHA, never touch the network,
/// and surface their in-flight state through the dedicated
/// `CommitStatus::CherryPicking` / `CommitStatus::Reverting` variants
/// so the status row reads "Cherry-picking…" rather than the generic
/// "Committing…" .
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitVerb {
    CherryPick(String),
    Revert(String),
}

impl CommitVerb {
    fn label(&self) -> &'static str {
        match self {
            CommitVerb::CherryPick(_) => "cherry-pick",
            CommitVerb::Revert(_) => "revert",
        }
    }

    fn in_flight_status(&self) -> CommitStatus {
        match self {
            CommitVerb::CherryPick(_) => CommitStatus::CherryPicking,
            CommitVerb::Revert(_) => CommitStatus::Reverting,
        }
    }
}

/// Run a per-commit history op (cherry-pick / revert). Mirrors `run_remote`
/// but takes a `CommitVerb` carrying the target SHA. Single-flight on the
/// `in_flight` flag — a second click on a Cherry-pick / Revert row while
/// the first is still running is a no-op rather than a queue.
///
/// Conflict failure leaves the worktree in CHERRY_PICK_HEAD / REVERT_HEAD
/// state; `Repository::current_operation()` (polled once per tick) picks
/// the new state up on the next refresh, so the operation banner appears
/// to guide the user through the recovery sequence.
pub fn run_commit_verb(area: &mut CommitArea, verb: CommitVerb, cx: &mut Context<CommitArea>) {
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    area.status = verb.in_flight_status();
    let repo = area.repo.clone();
    let label = verb.label();
    let (tx, rx) = oneshot::channel::<Result<&'static str, (&'static str, String)>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let verb_for_task = verb.clone();
            handle.spawn(async move {
                let r = match verb_for_task {
                    CommitVerb::CherryPick(sha) => repo.cherry_pick(&sha).await,
                    CommitVerb::Revert(sha) => repo.revert_commit(&sha).await,
                };
                let _ = match r {
                    Ok(_) => tx.send(Ok(label)),
                    Err(e) => tx.send(Err((label, e.to_string()))),
                };
            });
        }
        Err(_) => {
            area.status = CommitStatus::Failed(label.to_string(), "no tokio runtime".to_string());
            area.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    }
    spawn_completion(area, rx, cx);
}

/// Run a standalone remote op (push/pull/sync/fetch). Updates `status`
/// to the matching in-flight variant and back to Idle/Failed on completion.
pub fn run_remote(area: &mut CommitArea, verb: RemoteVerb, cx: &mut Context<CommitArea>) {
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    area.status = verb.in_flight_status();
    let repo = area.repo.clone();
    let label = verb.label();
    let (tx, rx) = oneshot::channel::<Result<&'static str, (&'static str, String)>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let r = match verb {
                    RemoteVerb::Push => repo.push().await,
                    RemoteVerb::Pull => repo.pull().await,
                    RemoteVerb::Sync => repo.sync().await,
                    RemoteVerb::Fetch => repo.fetch().await,
                    // Hardcoded `origin` matches the default-remote
                    // assumption used elsewhere in the SCM surface. If a
                    // future feature lets the user pick a remote, the
                    // verb gains a payload field.
                    RemoteVerb::Publish => repo.publish_branch("origin").await,
                };
                let _ = match r {
                    Ok(_) => tx.send(Ok(label)),
                    Err(e) => tx.send(Err((label, e.to_string()))),
                };
            });
        }
        Err(_) => {
            area.status = CommitStatus::Failed(label.to_string(), "no tokio runtime".to_string());
            area.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    }
    spawn_completion(area, rx, cx);
}

/// Completion path for remote-only verbs (push, pull, sync, fetch,
/// publish). No textarea clear, no Window plumbing required —
/// `apply_result` receives `window: None` and only flips the status
/// row.
fn spawn_completion(
    area: &mut CommitArea,
    rx: oneshot::Receiver<Result<&'static str, (&'static str, String)>>,
    cx: &mut Context<CommitArea>,
) {
    let task = cx.spawn(async move |this, cx| {
        let Ok(result) = rx.await else {
            return;
        };
        let _ = this.update(cx, |area, cx| {
            area.in_flight.store(false, Ordering::SeqCst);
            area.apply_result(result, None, cx);
            cx.notify();
        });
    });
    area._commit_task = Some(task);
}

/// Completion path for the commit verb (and commit-with-followup
/// variants). Roots the spawn in `cx.spawn_in(window, …)` so the
/// `update_in` block on success can hand a live `&mut Window` to
/// `apply_result`, which clears the textarea via `set_value`.
///
/// `cx.spawn_in` + `update_in` is the same pattern as the branch
/// picker's async open flow (`picker_wiring::open_switch_picker`);
/// verified to fire correctly from a click-rooted async chain. The
/// `update_in silently fails in mouse callbacks` GPUI gotcha applies
/// to `cx.spawn` (no `_in`), not to this `cx.spawn_in` rooted variant.
fn spawn_commit_completion(
    area: &mut CommitArea,
    rx: oneshot::Receiver<Result<&'static str, (&'static str, String)>>,
    window: &mut Window,
    cx: &mut Context<CommitArea>,
) {
    let task = cx.spawn_in(window, async move |this, cx| {
        let Ok(result) = rx.await else {
            return;
        };
        let _ = this.update_in(cx, |area, window, cx| {
            area.in_flight.store(false, Ordering::SeqCst);
            area.apply_result(result, Some(window), cx);
            cx.notify();
        });
    });
    area._commit_task = Some(task);
}
