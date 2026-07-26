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
    /// `git push --force-with-lease`. Surfaced when the branch is ahead of
    /// upstream and a plain push would be rejected (non-fast-forward). The
    /// `--force-with-lease` guard means a teammate's surprise upstream work
    /// aborts the push instead of being overwritten.
    ForcePush,
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
            RemoteVerb::ForcePush => "force push",
            RemoteVerb::Pull => "pull",
            RemoteVerb::Sync => "sync",
            RemoteVerb::Fetch => "fetch",
            RemoteVerb::Publish => "publish",
        }
    }

    pub fn in_flight_status(self) -> CommitStatus {
        match self {
            RemoteVerb::Push => CommitStatus::Pushing,
            // A force-push is still a push from the status row's POV.
            RemoteVerb::ForcePush => CommitStatus::Pushing,
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

/// Optional remote step run after a successful commit. Mutually
/// exclusive by construction (an enum, not a set of bools), so the
/// "commit then push AND sync" impossible state can't be expressed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommitFollowup {
    /// Commit only — no network step.
    None,
    /// Commit then `git push`.
    Push,
    /// Commit then `git pull --ff-only && git push`.
    Sync,
    /// Commit then `git push --force-with-lease`. Used when the local
    /// branch has rewritten history (amend/rebase) that a plain push
    /// would reject.
    ForcePush,
}

/// Run the commit pipeline with an optional follow-up remote step.
///
/// `window` is plumbed through so the completion task can root its
/// `cx.spawn_in(window, …)` — that's what gives the success-arm
/// access to a `&mut Window` inside `update_in`, which the textarea
/// auto-clear (`InputState::set_value`) requires.
pub fn run_commit(
    area: &mut CommitArea,
    followup: CommitFollowup,
    window: &mut Window,
    cx: &mut Context<CommitArea>,
) {
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
                match followup {
                    CommitFollowup::Push => {
                        let _ = match repo.push().await {
                            Ok(_) => tx.send(Ok("push")),
                            Err(e) => tx.send(Err(("push", e.to_string()))),
                        };
                    }
                    CommitFollowup::Sync => {
                        let _ = match repo.sync().await {
                            Ok(_) => tx.send(Ok("sync")),
                            Err(e) => tx.send(Err(("sync", e.to_string()))),
                        };
                    }
                    CommitFollowup::ForcePush => {
                        let _ = match repo.force_push().await {
                            Ok(_) => tx.send(Ok("force push")),
                            Err(e) => tx.send(Err(("force push", e.to_string()))),
                        };
                    }
                    CommitFollowup::None => {
                        let _ = tx.send(Ok("commit"));
                    }
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
    /// `git rebase <base>`. Carries the base ref (e.g. `origin/main`)
    /// rather than a commit SHA, but shares the same conflict→operation-banner
    /// lifecycle as cherry-pick / revert, so it rides the same machinery.
    Rebase(String),
}

impl CommitVerb {
    fn label(&self) -> &'static str {
        match self {
            CommitVerb::CherryPick(_) => "cherry-pick",
            CommitVerb::Revert(_) => "revert",
            CommitVerb::Rebase(_) => "rebase",
        }
    }

    fn in_flight_status(&self) -> CommitStatus {
        match self {
            CommitVerb::CherryPick(_) => CommitStatus::CherryPicking,
            CommitVerb::Revert(_) => CommitStatus::Reverting,
            CommitVerb::Rebase(_) => CommitStatus::Rebasing,
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
                    CommitVerb::Rebase(base) => repo.rebase_onto(&base).await,
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

/// Operation-banner recovery verbs: abort or continue the in-progress
/// merge / rebase / cherry-pick / revert / bisect. Carries the
/// `GitOperation` so the git layer dispatches the right `--abort` /
/// `--continue` command. Distinct from `CommitVerb` (which extends
/// history) — these resolve a *paused* operation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OperationRecovery {
    Abort(oximux_core::GitOperation),
    Continue(oximux_core::GitOperation),
}

impl OperationRecovery {
    fn label(self) -> &'static str {
        match self {
            OperationRecovery::Abort(_) => "abort",
            OperationRecovery::Continue(_) => "continue",
        }
    }

    fn in_flight_status(self) -> CommitStatus {
        match self {
            OperationRecovery::Abort(_) => CommitStatus::Aborting,
            OperationRecovery::Continue(_) => CommitStatus::Continuing,
        }
    }
}

/// Run an operation-banner recovery (abort / continue). Single-flight on
/// the shared `in_flight` flag, same completion path as the remote verbs.
/// On success the next poll tick re-reads `current_operation()`; an aborted
/// or completed op clears the banner. A failed continue (conflicts still
/// unstaged) surfaces `Failed("continue", …)` in the status row.
pub fn run_op_recovery(
    area: &mut CommitArea,
    recovery: OperationRecovery,
    cx: &mut Context<CommitArea>,
) {
    if area.in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    area.status = recovery.in_flight_status();
    let repo = area.repo.clone();
    let label = recovery.label();
    let (tx, rx) = oneshot::channel::<Result<&'static str, (&'static str, String)>>();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let r = match recovery {
                    OperationRecovery::Abort(op) => repo.abort_operation(op).await,
                    OperationRecovery::Continue(op) => repo.continue_operation(op).await,
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
                    RemoteVerb::ForcePush => repo.force_push().await,
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
