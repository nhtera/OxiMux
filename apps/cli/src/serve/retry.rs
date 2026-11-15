//! Automatic re-send of a turn a provider limit closed, headless.
//!
//! The policy — what counts as retryable, how long to wait, how many times —
//! is `oximux_agents::retry` and is pure; the durable attempt count is the
//! host's own `turn_retries` table, which the desktop writes too. What is
//! here is the part a host has to supply: a timer, and the answer to "where
//! do I send it?".
//!
//! **Why this is not inside the pump.** A pump ends when its agent's stream
//! closes, and the turn that failed is often the one that killed the agent —
//! so a timer owned by the pump would die with exactly the session it exists
//! to rescue. It also has to survive a host restart: a five-hour window
//! outlives the process that hit it, which is the whole reason the wait is
//! worth having. So the driver sits beside the catalog, reopens a dormant
//! session before it sends, and is rebuilt at boot from the rows the last
//! host left.
//!
//! The pump still owns what only it can see — the failure, the turn ending
//! well, the lines it writes — and tells this driver about each.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use oximux_agents::health_track::HealthTrack;
use oximux_agents::retry::driver::{Next, class_text, decide};
use oximux_agents::retry::store::{Resolution, RetryStore};
use oximux_agents::retry::{self, RetryClass, classify_failure, reason_for};
use oximux_agents::send_gate::SendSource;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::ChatThread;
use oximux_remote_host::HostConfig;
use oximux_storage::SettingsRepo;
use uuid::Uuid;

use super::blob;
use super::catalog::HeadlessCatalog;

/// How far past its wake time a restored retry may be and still fire.
///
/// Sized for "the host was restarted a few minutes ago", not for "it was off
/// overnight". A retry whose reset passed while nothing was running for
/// longer than this is dropped: the turn is still there and a person can
/// still send it again, but nothing re-sends on their behalf for a limit that
/// expired without them. The desktop holds the same line for the same reason.
const RESTORE_GRACE_MS: i64 = 5 * 60 * 1000;

/// A retry this host has committed to, as everything but the timer sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmedRetry {
    /// Unix ms it fires at.
    pub wake_at_ms: i64,
    /// Short human reason ("Usage limit reached", "Provider overloaded").
    pub reason: String,
}

/// One session's live retry, as this host holds it.
///
/// The turn key is not kept here: the timer carries its own copy, and the
/// durable record is the store's. A third would be one more thing to keep in
/// step with the other two.
struct Armed {
    retry: ArmedRetry,
    /// Cancels the timer on drop, so resolving needs no second step.
    timer: tokio::task::JoinHandle<()>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

/// The host's retry driver. One per host; every session's retry lives here.
pub struct ServeRetry {
    registry: Arc<SessionRegistry>,
    store: RetryStore,
    settings: SettingsRepo,
    config: tokio::sync::watch::Receiver<HostConfig>,
    instance: String,
    /// Set once the catalog exists — it holds this driver, so this side is
    /// weak. A retry that fires with no catalog can still reach a live
    /// session; it just cannot reopen a dormant one.
    catalog: Mutex<Weak<HeadlessCatalog>>,
    armed: Mutex<HashMap<String, Armed>>,
    /// Bumped whenever any session's armed retry changes.
    ///
    /// A pump's holds are partly this driver's state, and nothing else tells
    /// it when that state moves: the gate's own waker fires when an item is
    /// *pushed* and only while the gate is open, so a retry that fires —
    /// which lifts the hold and queues a line in the same breath — would
    /// push into a gate still holding for it and wake nobody. The line would
    /// then wait for an unrelated event that a session waiting out a limit
    /// does not have.
    changed: tokio::sync::watch::Sender<u64>,
    /// The line a fired retry wrote, per session, until the pump reports it.
    ///
    /// A resend is a send like any other by the time it reaches the pump, and
    /// the pump's rule is that a send supersedes the retry of an earlier
    /// turn. Without this the retry would supersede *itself*, taking the
    /// attempt count and the turn key with it, and the next failure would
    /// start counting from zero — four attempts without end.
    in_flight: Mutex<HashMap<String, Uuid>>,
}

impl ServeRetry {
    pub fn new(
        registry: Arc<SessionRegistry>,
        store: RetryStore,
        settings: SettingsRepo,
        config: tokio::sync::watch::Receiver<HostConfig>,
        instance: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            store,
            settings,
            config,
            instance,
            catalog: Mutex::new(Weak::new()),
            armed: Mutex::new(HashMap::new()),
            changed: tokio::sync::watch::channel(0).0,
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    /// Hand over the catalog, once it exists, so a retry can reopen a session
    /// whose agent is gone.
    pub fn set_catalog(&self, catalog: &Arc<HeadlessCatalog>) {
        *self.catalog.lock().unwrap() = Arc::downgrade(catalog);
    }

    /// This session's armed retry, for the health facts and the gate's holds.
    /// A map read: the pump asks on every publish.
    pub fn armed(&self, session_id: &str) -> Option<ArmedRetry> {
        self.armed.lock().unwrap().get(session_id).map(|a| a.retry.clone())
    }

    /// Follow "something armed or unarmed", for a pump to republish on. One
    /// counter for the host rather than one per session: a republish a pump
    /// did not need is a publish of facts that have not changed, and the
    /// alternative is a channel per session to keep in step with the pumps.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn note_changed(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// A turn ended in error. Arm a retry if the failure is one waiting can
    /// fix and this session has attempts left.
    ///
    /// Three conditions are checked here rather than in the policy, because
    /// they are properties of this host and not of the failure: the setting
    /// may be off, the process may be gone (a dead agent is reopened at fire
    /// time, but one whose turn failed *because* it died is not a limit), and
    /// a turn already running has superseded this one.
    pub fn on_turn_failed(self: &Arc<Self>, session_id: &str, fold: &ChatThread, track: &HealthTrack) {
        let class = classify_failure(fold.last_rate_limit.as_ref(), fold.last_turn_failure.clone());
        if !class.is_retryable() {
            // A turn that had a retry and now fails for good: that retry is
            // over, and the failure is the session's own answer.
            self.resolve(session_id, Resolution::Cancelled);
            return;
        }
        if !self.config.borrow().retry_enabled {
            return;
        }
        // The key the attempts count against, held across this turn's
        // retries: recomputing it from a transcript the last resend grew
        // would reset the count and retry for ever.
        let live = self.store.live(session_id).ok().flatten();
        let turn_key = match &live {
            Some(live) => live.turn_key.clone(),
            None => retry::turn_key(track.sent_id().as_deref(), &fold.entries),
        };
        let attempts = self.store.attempts(session_id, &turn_key).unwrap_or(0);
        let now_ms = oximux_agents::session_health::now_ms();
        let policy = self.config.borrow().retry();
        match decide(class, attempts, now_ms, &policy, self.seed(session_id, now_ms)) {
            Next::Arm { wake_at_ms, .. } => self.arm(session_id, turn_key, class, wake_at_ms),
            // Out of attempts, or a wait longer than this host may hold a
            // turn. Either way nothing more is automatic, and the failure the
            // pump already published stands.
            Next::Exhausted => self.resolve(session_id, Resolution::Exhausted),
            Next::Never => {}
        }
    }

    /// A turn ended well: the retry that got it there succeeded, and the next
    /// failure counts from zero.
    pub fn on_turn_succeeded(&self, session_id: &str) {
        self.resolve(session_id, Resolution::Succeeded);
    }

    /// The pump wrote a line. Anything but this driver's own resend
    /// supersedes an armed retry of an earlier turn: whoever sent it has
    /// taken over, and firing the old turn on top of their message would be
    /// a surprise.
    ///
    /// Called for every line, not only the ones that could have superseded
    /// something: while a retry is armed the gate holds every other sender
    /// anyway, so the common case is a `resolve` that matches no row — one
    /// no-op statement against a table with one row per session.
    pub fn on_line_written(&self, session_id: &str, uuid: Uuid) {
        if self.in_flight.lock().unwrap().get(session_id) == Some(&uuid) {
            self.in_flight.lock().unwrap().remove(session_id);
            return;
        }
        self.resolve(session_id, Resolution::Superseded);
    }

    /// Re-arm every retry the last host left live. Called once at boot,
    /// after the catalog is set.
    ///
    /// A row whose wake time is further past than [`RESTORE_GRACE_MS`] is
    /// resolved rather than fired: the window reopened while nothing was
    /// running, and re-sending a turn whose failure nobody has seen since is
    /// a surprise, not a resumption.
    pub fn restore(self: &Arc<Self>) {
        let now_ms = oximux_agents::session_health::now_ms();
        let rows = match self.store.live_all() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "armed retries could not be read; none restored");
                return;
            }
        };
        let (mut armed, mut dropped) = (0, 0);
        for (session_id, live) in rows {
            let Some(wake_at_ms) = live.next_wake_at_ms else { continue };
            // The setting is read now, not as it was when the retry was
            // armed: a user who switched retries off while this host was down
            // is obeyed.
            let stale = now_ms - wake_at_ms > RESTORE_GRACE_MS;
            let over_ceiling = self
                .config
                .borrow()
                .retry()
                .max_automatic_wait
                .is_some_and(|limit| wake_at_ms - now_ms > limit.as_millis() as i64);
            if !self.config.borrow().retry_enabled || stale || over_ceiling {
                let _ = self.store.resolve(&session_id, Resolution::Cancelled, chrono::Local::now());
                dropped += 1;
                continue;
            }
            self.hold_until(&session_id, live.turn_key.clone(), live.reason.clone(), wake_at_ms);
            armed += 1;
        }
        if armed > 0 || dropped > 0 {
            tracing::info!(armed, dropped, "retries carried over from the previous host");
        }
    }

    /// Record the decision and start the timer.
    fn arm(self: &Arc<Self>, session_id: &str, turn_key: String, class: RetryClass, wake_at_ms: i64) {
        let reason = reason_for(class);
        if let Err(err) = self.store.arm(
            session_id,
            &turn_key,
            class_text(class),
            &reason,
            wake_at_ms,
            &self.instance,
            chrono::Local::now(),
        ) {
            // Without the record there is no durable attempt count, so firing
            // anyway could retry past the cap across a restart. The failure
            // stands instead.
            tracing::warn!(%err, session = session_id, "a retry was not armed: its record could not be written");
            return;
        }
        self.hold_until(session_id, turn_key, reason, wake_at_ms);
    }

    /// Hold the turn until `wake_at_ms`, then fire. Shared by the live arming
    /// path and by [`Self::restore`], so a retry rebuilt at boot waits and
    /// fires by exactly the same code as one armed a moment ago.
    fn hold_until(self: &Arc<Self>, session_id: &str, turn_key: String, reason: String, wake_at_ms: i64) {
        let driver = self.clone();
        let session = session_id.to_string();
        let key = turn_key;
        let timer = tokio::spawn(async move {
            let wait = wake_at_ms - oximux_agents::session_health::now_ms();
            if wait > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
            }
            driver.fire(&session, &key).await;
        });
        // Replacing the entry drops the previous timer, which aborts it:
        // one retry per session, and the newest decision is the live one.
        self.armed
            .lock()
            .unwrap()
            .insert(session_id.to_string(), Armed { retry: ArmedRetry { wake_at_ms, reason }, timer });
        self.note_changed();
    }

    /// The wait is over: spend the attempt and send the turn again.
    async fn fire(self: Arc<Self>, session_id: &str, turn_key: &str) {
        // Spent at fire time, not at arm time, so a retry that is cancelled
        // or superseded before it fires costs nothing.
        if let Err(err) = self.store.fired(session_id, turn_key) {
            tracing::warn!(%err, session = session_id, "a fired retry was not counted");
        }
        // The hold is over whether or not the send lands: left in place, the
        // gate would keep holding this session's queue behind a retry that
        // has already happened. Announced before the send below, so the pump
        // has republished its holds by the time the line is pushed.
        self.armed.lock().unwrap().remove(session_id);
        self.note_changed();

        let handle = match self.registry.get(session_id) {
            Some(handle) => Some(handle),
            // Dormant: its agent exited while the turn was held. Reopened
            // once — a reopen that fails is the end of this retry, never a
            // second attempt at the same thing.
            None => {
                // Taken out of the lock before the await, not matched on in
                // place: a `MutexGuard` alive across an await makes this
                // future `!Send`, and `tokio::spawn` refuses it.
                let catalog = self.catalog.lock().unwrap().upgrade();
                match catalog {
                    Some(catalog) => {
                        match oximux_remote_host::catalog::SessionCatalog::open(&*catalog, session_id).await {
                            Ok(()) => self.registry.get(session_id),
                            Err(err) => {
                                tracing::warn!(session = session_id, %err, "a retry could not reopen its session");
                                None
                            }
                        }
                    }
                    None => None,
                }
            }
        };
        let Some(handle) = handle else {
            self.resolve(session_id, Resolution::Exhausted);
            return;
        };

        let Some(blob) = blob::load(&self.settings, session_id) else {
            tracing::warn!(session = session_id, "a retry found no transcript to re-send");
            self.resolve(session_id, Resolution::Exhausted);
            return;
        };
        let (text, images) = match resend(&blob.entries) {
            Some(prompt) => prompt,
            None => {
                tracing::warn!(session = session_id, "a retry found nothing to re-send");
                self.resolve(session_id, Resolution::Exhausted);
                return;
            }
        };

        // Recorded before the write: the pump sees the line as soon as it is
        // flushed, and a line it cannot recognise as this retry's own would
        // supersede the retry that sent it.
        let line = Uuid::new_v4();
        self.in_flight.lock().unwrap().insert(session_id.to_string(), line);
        // `Remote` and not a sender of its own: this is the person's turn
        // going out again, from no one at a screen — which is what a phone's
        // send already means to the gate.
        if let Err(err) = handle.send_prompt_tagged(&text, &images, SendSource::Remote, Some(line)) {
            tracing::warn!(session = session_id, %err, "a retry could not be sent");
            self.in_flight.lock().unwrap().remove(session_id);
            self.resolve(session_id, Resolution::Exhausted);
        }
    }

    /// This session's retry is over. Drops the timer with it.
    fn resolve(&self, session_id: &str, how: Resolution) {
        self.armed.lock().unwrap().remove(session_id);
        self.note_changed();
        if let Err(err) = self.store.resolve(session_id, how, chrono::Local::now()) {
            tracing::warn!(%err, session = session_id, "a retry's record was not resolved");
        }
    }

    /// The jitter seed. Per *session*, not per call: every thread on one
    /// account sees the same reset, and a shared seed would wake them all in
    /// the same second — the storm the jitter exists to prevent. The clock
    /// breaks ties between two sessions that happen to hash alike.
    fn seed(&self, session_id: &str, now_ms: i64) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(session_id, &mut hasher);
        std::hash::Hash::hash(&now_ms, &mut hasher);
        std::hash::Hasher::finish(&hasher)
    }
}

/// What to send again: the user's own prompt, or a request to carry on when
/// the failed turn had already produced something.
///
/// The rule is `oximux_agents::retry`'s, so this host and the desktop cannot
/// disagree about it: re-sending a prompt the agent had already started on
/// would make it do the work twice.
fn resend(entries: &[oximux_agents::thread::ThreadEntry]) -> Option<(String, Vec<oximux_agents::thread::ChatImage>)> {
    if retry::turn_had_output(entries) {
        return Some((retry::CONTINUE_PROMPT.to_string(), Vec::new()));
    }
    entries.iter().rev().find_map(|entry| match entry {
        oximux_agents::thread::ThreadEntry::User { text, images, .. } => {
            Some((text.clone(), images.clone()))
        }
        _ => None,
    })
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
