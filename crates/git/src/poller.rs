//! Background tokio task that polls `Repository::status` and surfaces changes
//! through a `watch` channel. Pauses while the window is blurred.
//!
//! v0.9 lesson codified here: the poller does **not** own an `Arc<watch::Sender>`
//! that the UI listens to directly. The app layer subscribes to the receiver and
//! routes updates through `entity.update(cx, …)` — that's what keeps focus-state
//! invariants on the GPUI side intact.
//!
//! Drop aborts the task; no zombie polling and no orphaned producer/consumer.

use crate::repository::Repository;
use oximux_core::GitState;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::AbortHandle;

/// Default tick interval. Matches the phase-02 plan; configurable per-instance
/// via `spawn_with_interval` for tests.
pub const DEFAULT_TICK: Duration = Duration::from_millis(500);

/// After this many consecutive `repo.status()` failures, the poller emits a
/// `None` on the watch channel so the UI can distinguish "git is broken /
/// repo deleted" from "still loading first sample".
const FAILURE_THRESHOLD: u32 = 3;

pub struct StatusPoller {
    state_rx: watch::Receiver<Option<GitState>>,
    focus_tx: watch::Sender<bool>,
    abort: AbortHandle,
}

impl StatusPoller {
    /// Spawn a poller at the default 500 ms cadence. Focus defaults to `true`
    /// (ticking). The first successful poll publishes a `Some(GitState)`; until
    /// then the receiver yields `None`. After `FAILURE_THRESHOLD` consecutive
    /// failures the channel is reset to `None`.
    pub fn spawn(repo: Repository) -> Self {
        Self::spawn_with_interval(repo, DEFAULT_TICK)
    }

    pub fn spawn_with_interval(repo: Repository, tick: Duration) -> Self {
        let (state_tx, state_rx) = watch::channel::<Option<GitState>>(None);
        let (focus_tx, focus_rx) = watch::channel::<bool>(true);
        let task = tokio::spawn(poll_loop(repo, tick, state_tx, focus_rx));
        Self {
            state_rx,
            focus_tx,
            abort: task.abort_handle(),
        }
    }

    /// Subscribe to status updates. Receivers see the latest value on attach.
    pub fn subscribe(&self) -> watch::Receiver<Option<GitState>> {
        self.state_rx.clone()
    }

    /// Most recently observed state (without awaiting a change).
    pub fn current(&self) -> Option<GitState> {
        self.state_rx.borrow().clone()
    }

    /// Toggle focus. While `false`, the poller suspends ticking.
    pub fn set_focused(&self, focused: bool) {
        // send_replace returns the prior value; we don't care about it.
        let _ = self.focus_tx.send_replace(focused);
    }
}

impl Drop for StatusPoller {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

async fn poll_loop(
    repo: Repository,
    tick: Duration,
    state_tx: watch::Sender<Option<GitState>>,
    mut focus_rx: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(tick);
    // Tick semantics: first `.tick()` fires immediately so callers see fresh
    // state on startup instead of waiting `tick` ms for the first sample.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut consecutive_failures: u32 = 0;

    loop {
        // Block until focused. Bail out if the sender side is dropped (poller
        // owner has gone away — we'll be aborted shortly anyway).
        if !*focus_rx.borrow() {
            match focus_rx.changed().await {
                Ok(()) => continue,
                Err(_) => return,
            }
        }

        tokio::select! {
            biased;
            // If focus drops mid-wait, loop back to the gate immediately.
            res = focus_rx.changed() => {
                if res.is_err() {
                    return;
                }
                continue;
            }
            _ = interval.tick() => {
                match repo.status().await {
                    Ok(mut next) => {
                        consecutive_failures = 0;
                        // Stable order so `send_if_modified` doesn't emit
                        // spurious wakeups if git reorders semantically
                        // identical rows between ticks.
                        next.files.sort_by(|a, b| a.path.cmp(&b.path));
                        let _ = state_tx.send_if_modified(|cur| {
                            if cur.as_ref() == Some(&next) {
                                false
                            } else {
                                *cur = Some(next);
                                true
                            }
                        });
                    }
                    Err(e) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        if consecutive_failures < FAILURE_THRESHOLD {
                            tracing::warn!(error = %e, attempt = consecutive_failures, "git status poll failed");
                        } else if consecutive_failures == FAILURE_THRESHOLD {
                            tracing::error!(error = %e, "git status poller giving up — emitting None");
                            let _ = state_tx.send_if_modified(|cur| {
                                if cur.is_some() {
                                    *cur = None;
                                    true
                                } else {
                                    false
                                }
                            });
                        }
                        // Past the threshold, stay silent until a success
                        // resets the counter — avoids log spam.
                    }
                }
            }
        }
    }
}
