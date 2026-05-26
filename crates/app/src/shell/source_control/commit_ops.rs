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

use gpui::Context;
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
}

impl RemoteVerb {
    pub fn label(self) -> &'static str {
        match self {
            RemoteVerb::Push => "push",
            RemoteVerb::Pull => "pull",
            RemoteVerb::Sync => "sync",
            RemoteVerb::Fetch => "fetch",
        }
    }

    pub fn in_flight_status(self) -> CommitStatus {
        match self {
            RemoteVerb::Push => CommitStatus::Pushing,
            RemoteVerb::Pull => CommitStatus::Pulling,
            RemoteVerb::Sync => CommitStatus::Syncing,
            RemoteVerb::Fetch => CommitStatus::Fetching,
        }
    }
}

/// Run the commit pipeline with optional follow-up push/sync.
/// `followup_push` ⊕ `followup_sync` are mutually exclusive (asserted).
pub fn run_commit(
    area: &mut CommitArea,
    followup_push: bool,
    followup_sync: bool,
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
        area.status =
            CommitStatus::Failed("commit".to_string(), "Message is empty".to_string());
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
    spawn_completion(area, rx, cx);
}

/// Run a standalone remote op (push/pull/sync/fetch). Updates `status`
/// to the matching in-flight variant and back to Idle/Failed on completion.
pub fn run_remote(
    area: &mut CommitArea,
    verb: RemoteVerb,
    cx: &mut Context<CommitArea>,
) {
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
                };
                let _ = match r {
                    Ok(_) => tx.send(Ok(label)),
                    Err(e) => tx.send(Err((label, e.to_string()))),
                };
            });
        }
        Err(_) => {
            area.status =
                CommitStatus::Failed(label.to_string(), "no tokio runtime".to_string());
            area.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    }
    spawn_completion(area, rx, cx);
}

/// Shared completion path: await the oneshot, update status, clear
/// in-flight. Both `run_commit` and `run_remote` route through this so
/// they only differ in the work they schedule, not the bookkeeping.
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
            area.apply_result(result);
            cx.notify();
        });
    });
    area._commit_task = Some(task);
}
