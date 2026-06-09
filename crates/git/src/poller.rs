//! Background tokio task that polls `Repository::status` and surfaces changes
//! through a `watch` channel. Pauses while the window is blurred.
//!
//! v0.9 lesson codified here: the poller does **not** own an `Arc<watch::Sender>`
//! that the UI listens to directly. The app layer subscribes to the receiver and
//! routes updates through `entity.update(cx, …)` — that's what keeps focus-state
//! invariants on the GPUI side intact.
//!
//! Drop aborts the task; no zombie polling and no orphaned producer/consumer.

use crate::error::GitError;
use crate::repository::Repository;
use oximux_core::GitState;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::AbortHandle;

/// Default tick interval. Matches the phase-02 plan; configurable per-instance
/// via `spawn_with_interval` for tests.
pub const DEFAULT_TICK: Duration = Duration::from_millis(500);

/// After this many consecutive `repo.status()` failures, the poller emits
/// `PollState::Failed(last_error)` on the watch channel so the UI can
/// distinguish "git is broken / repo deleted" from "still loading first sample".
const FAILURE_THRESHOLD: u32 = 3;

/// Tri-state poller output. `Loading` is the initial channel value before
/// the first poll completes; `Ready` carries the last successful sample;
/// `Failed` carries the last error after `FAILURE_THRESHOLD` consecutive
/// failures (transient single-poll failures are logged but not surfaced).
#[derive(Debug, Clone)]
pub enum PollState {
    /// Initial value; no poll has completed yet.
    Loading,
    /// Most recent poll succeeded.
    Ready(GitState),
    /// `FAILURE_THRESHOLD` consecutive failures; last error attached.
    Failed(GitError),
}

pub struct StatusPoller {
    state_rx: watch::Receiver<PollState>,
    focus_tx: watch::Sender<bool>,
    /// Bumped by `kick()` to force the poll loop out of `interval.tick()`
    /// early — used by the focus-regain path so the user sees a fresh
    /// status without waiting for the next 500 ms tick.
    kick_tx: watch::Sender<u64>,
    abort: AbortHandle,
}

impl StatusPoller {
    /// Spawn a poller at the default 500 ms cadence. Focus defaults to `true`
    /// (ticking). Initial channel value is `PollState::Loading`; the first
    /// successful poll publishes `PollState::Ready(state)`. After
    /// `FAILURE_THRESHOLD` consecutive failures the channel transitions to
    /// `PollState::Failed(error)` and stays there until a poll succeeds.
    pub fn spawn(repo: Repository) -> Self {
        Self::spawn_with_interval(repo, DEFAULT_TICK)
    }

    /// Spawn with the watch channel pre-seeded to `initial` instead of
    /// `Loading`. Used at boot/project-switch to paint the last-known
    /// `GitState` immediately (stale-while-revalidate): subscribers see the
    /// prior snapshot on attach, and the first poll — which still fires
    /// right away — overwrites it ~one `status()` later. A cache miss passes
    /// `PollState::Loading` and behaves exactly like `spawn`.
    pub fn spawn_seeded(repo: Repository, initial: PollState) -> Self {
        Self::spawn_with_interval_seeded(repo, DEFAULT_TICK, initial)
    }

    pub fn spawn_with_interval(repo: Repository, tick: Duration) -> Self {
        Self::spawn_with_interval_seeded(repo, tick, PollState::Loading)
    }

    pub fn spawn_with_interval_seeded(
        repo: Repository,
        tick: Duration,
        initial: PollState,
    ) -> Self {
        let (state_tx, state_rx) = watch::channel::<PollState>(initial);
        let (focus_tx, focus_rx) = watch::channel::<bool>(true);
        let (kick_tx, kick_rx) = watch::channel::<u64>(0);
        let task = tokio::spawn(poll_loop(repo, tick, state_tx, focus_rx, kick_rx));
        Self {
            state_rx,
            focus_tx,
            kick_tx,
            abort: task.abort_handle(),
        }
    }

    /// Subscribe to status updates. Receivers see the latest value on attach.
    pub fn subscribe(&self) -> watch::Receiver<PollState> {
        self.state_rx.clone()
    }

    /// Most recently observed state (without awaiting a change).
    pub fn current(&self) -> PollState {
        self.state_rx.borrow().clone()
    }

    /// Toggle focus. While `false`, the poller suspends ticking.
    pub fn set_focused(&self, focused: bool) {
        // send_replace returns the prior value; we don't care about it.
        let _ = self.focus_tx.send_replace(focused);
    }

    /// Force the next status fetch to happen immediately, skipping the
    /// remaining interval. Used on window focus regain so the user doesn't
    /// stare at a stale `GitState` while the timer winds down.
    pub fn kick(&self) {
        let next = self.kick_tx.borrow().wrapping_add(1);
        let _ = self.kick_tx.send_replace(next);
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
    state_tx: watch::Sender<PollState>,
    mut focus_rx: watch::Receiver<bool>,
    mut kick_rx: watch::Receiver<u64>,
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

        // Block until something fires: focus drop, kick, or tick.
        tokio::select! {
            biased;
            res = focus_rx.changed() => {
                if res.is_err() {
                    return;
                }
                continue;
            }
            res = kick_rx.changed() => {
                // kick sender drop is non-fatal — we just stop reacting to kicks.
                let _ = res;
                interval.reset();
            }
            _ = interval.tick() => {}
        }
        run_poll_once(&repo, &state_tx, &mut consecutive_failures).await;
    }
}

/// Run one `repo.status()` round and publish the result via `state_tx`.
/// Extracted out of the select loop so kick + tick share the same body.
async fn run_poll_once(
    repo: &Repository,
    state_tx: &watch::Sender<PollState>,
    consecutive_failures: &mut u32,
) {
    match repo.status().await {
        Ok(mut next) => {
            *consecutive_failures = 0;
            // Stable order so `send_if_modified` doesn't emit spurious
            // wakeups if git reorders semantically identical rows between
            // ticks.
            next.files.sort_by(|a, b| a.path.cmp(&b.path));
            let _ = state_tx.send_if_modified(|cur| {
                if let PollState::Ready(prev) = cur
                    && prev == &next
                {
                    return false;
                }
                *cur = PollState::Ready(next);
                true
            });
        }
        Err(e) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            if *consecutive_failures < FAILURE_THRESHOLD {
                tracing::warn!(error = %e, attempt = *consecutive_failures, "git status poll failed");
            } else if *consecutive_failures == FAILURE_THRESHOLD {
                tracing::error!(error = %e, "git status poller giving up — emitting Failed");
                let err_for_channel = e.clone();
                let _ = state_tx.send_if_modified(|cur| {
                    *cur = PollState::Failed(err_for_channel);
                    true
                });
            }
            // Past the threshold, stay silent until a success resets the
            // counter — avoids log spam.
        }
    }
}
