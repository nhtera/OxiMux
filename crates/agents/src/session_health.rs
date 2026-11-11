//! A session's health, kept beside its handle.
//!
//! Whoever drives a session (the desktop view; the headless host later)
//! publishes its [`HealthInputs`] here on every change. The handle derives the
//! [`SessionHealth`] at once, and the host's ticker re-derives it as time
//! passes (a stall is a fact about the clock, not about any event). Both use
//! the one pure [`health::compute`], so every reader sees the same answer.
//!
//! Two inputs only the host can know are folded in here rather than by the
//! publisher: the activity of the session's working children (a parent
//! waiting on a busy child is not stalled) and a sleep the host's clock
//! observed (waking a laptop is not a burst of stalls).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::watch;

use crate::health::{self, HealthConfig, HealthInputs, HealthStatus, Process, SessionHealth};
use crate::session_registry::SessionHandle;

/// Wall clock, Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One consistent read of a session's state.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub health: SessionHealth,
    pub inputs: HealthInputs,
    /// The connection generation the inputs were published by.
    pub generation: i64,
}

/// What the host knows about a session's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildActivity {
    /// Children that are working.
    pub working: u32,
    /// The latest sign of life among them.
    pub last_activity_at: i64,
}

/// One session's health state. Held by its [`SessionHandle`].
pub struct HealthCell {
    inputs: watch::Sender<HealthInputs>,
    health: watch::Sender<SessionHealth>,
    children: Mutex<ChildActivity>,
    /// The stall clock restarts no earlier than this (the host's last wake,
    /// or the last answered prompt).
    forgiven_until: AtomicI64,
    cfg: Arc<RwLock<HealthConfig>>,
    /// Held across read-inputs → write-health → set-working, so a slower
    /// derivation (the host's tick) never overwrites one made from newer
    /// inputs.
    derive: Mutex<()>,
    /// The driver has published at least once. Until then the tick leaves the
    /// session alone: a spawn marks a child working before its view exists,
    /// and default inputs must not undo that.
    published: std::sync::atomic::AtomicBool,
    /// Which process the published facts came from (the handle's connection
    /// generation when they were published).
    ///
    /// Read with the facts rather than from the handle, because the two must
    /// agree: a respawn bumps the generation without clearing a prompt the old
    /// process had published, so a generation read a moment later would brand
    /// that prompt with the successor's number — and the successor's own first
    /// prompt, numbered the same by its provider, would then be the same alert.
    published_generation: AtomicI64,
}

impl HealthCell {
    pub(crate) fn new(cfg: Arc<RwLock<HealthConfig>>) -> Self {
        let now = now_ms();
        let first = health::compute(&HealthInputs::default(), now, &cfg.read().unwrap());
        Self {
            inputs: watch::channel(HealthInputs::default()).0,
            health: watch::channel(first).0,
            children: Mutex::new(ChildActivity::default()),
            forgiven_until: AtomicI64::new(0),
            published_generation: AtomicI64::new(0),
            cfg,
            derive: Mutex::new(()),
            published: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Re-derive at `now` and hand `set_working` whether the session counts as
    /// working, under the same lock. Returns the new health when what a person
    /// would read changed (status, liveness, stall, reason).
    fn recompute(
        &self,
        now: i64,
        decided: impl Fn(&str) -> bool,
        set_working: impl FnOnce(bool),
    ) -> Option<SessionHealth> {
        let derive = self.derive.lock().unwrap();
        self.recompute_under(derive, now, decided, set_working)
    }

    /// [`Self::recompute`] for a caller that already holds the lock, so a
    /// publish can write the facts and derive from them without anyone reading
    /// in between.
    fn recompute_under(
        &self,
        derive: std::sync::MutexGuard<'_, ()>,
        now: i64,
        decided: impl Fn(&str) -> bool,
        set_working: impl FnOnce(bool),
    ) -> Option<SessionHealth> {
        let _derive = derive;
        let mut inputs = self.inputs.borrow().clone();
        // Answered here (a phone, the CLI) before the driver's card heard.
        if inputs.prompt_request.as_deref().is_some_and(&decided) {
            inputs.prompt = None;
        }
        // Waiting on a person is not silence: a prompt answered (here or in
        // the driver) restarts the stall clock, before anything reads it.
        let waiting = |s: HealthStatus| matches!(s, HealthStatus::NeedsPermission | HealthStatus::NeedsAnswer);
        if inputs.prompt.is_none() && matches!(inputs.process, Process::Live) && waiting(self.health.borrow().status) {
            self.forgiven_until.fetch_max(now, Ordering::Relaxed);
        }
        let children = *self.children.lock().unwrap();
        inputs.children_active = children.working;
        inputs.last_activity_at = inputs
            .last_activity_at
            .max(self.forgiven_until.load(Ordering::Relaxed))
            .max(if children.working > 0 { children.last_activity_at } else { 0 });
        let cfg = *self.cfg.read().unwrap();
        let next = health::compute(&inputs, now, &cfg);
        let working = matches!(
            next.status,
            HealthStatus::Working | HealthStatus::NeedsPermission | HealthStatus::NeedsAnswer
        );
        let mut changed = None;
        self.health.send_if_modified(|h| {
            if *h == next {
                return false;
            }
            let readable = (h.status, h.liveness, h.stalled_since.is_some(), &h.reason)
                != (next.status, next.liveness, next.stalled_since.is_some(), &next.reason);
            if readable {
                changed = Some(next.clone());
            }
            *h = next.clone();
            true
        });
        set_working(working);
        changed
    }
}

impl HealthCell {
    /// Whether the driver has published at least once.
    pub(crate) fn published(&self) -> bool {
        self.published.load(Ordering::Relaxed)
    }

    /// The derived health, the inputs it was derived from, and the process
    /// those inputs were published by — under the lock every derivation holds.
    fn coherent(&self) -> Snapshot {
        let _derive = self.derive.lock().unwrap();
        Snapshot {
            health: self.health.borrow().clone(),
            inputs: self.inputs.borrow().clone(),
            generation: self.published_generation.load(Ordering::Relaxed),
        }
    }
}

impl SessionHandle {
    /// Whether `request_id` was decided through this handle.
    fn is_decided(&self, request_id: &str) -> bool {
        self.decided_contains(request_id)
    }

    /// Publish the driver's facts. Re-derives the health and the coarse
    /// `working` flag the lineage parks idle children by (a continuation or an
    /// unanswered send counts, so a child still reacting to its background
    /// tasks keeps its slot).
    pub fn publish_health(&self, inputs: HealthInputs) {
        let first = !self.health.published();
        // The facts and the health derived from them move in one step, under
        // the lock every derivation holds. A reader that saw the new facts
        // beside the old health would pair a prompt id with a health that no
        // longer has that prompt, and raise an alert for something already
        // answered.
        let derive = self.health.derive.lock().unwrap();
        let generation = self.conn_generation() as i64;
        let modified = self.health.inputs.send_if_modified(|current| {
            if *current == inputs {
                return false;
            }
            *current = inputs;
            true
        });
        // Stamped only when the facts actually changed. A republish of the
        // *same* facts — a rebind re-announcing what the chat already knows —
        // must not re-attribute them: they are still the old process's, and
        // moving the stamp would give its pending prompt the successor's
        // number, at which point the successor's own first prompt is the very
        // same alert and nothing announces it.
        if modified {
            self.health.published_generation.store(generation, Ordering::Relaxed);
        }
        let changed = if modified || first {
            self.health.recompute_under(
                derive,
                now_ms(),
                |id| self.is_decided(id),
                |working| {
                    self.set_working(working);
                },
            )
        } else {
            drop(derive);
            None
        };
        self.log_change(changed);
        if first {
            // Set only now, so nobody reads the default as observed; then
            // tell followers, even when the health reads as it did before.
            self.health.published.store(true, Ordering::Relaxed);
            self.health.health.send_modify(|_| {});
        }
    }

    /// Whether the session's driver has published its facts: until then its
    /// health is a default, not something anyone observed.
    pub fn health_published(&self) -> bool {
        self.health.published()
    }

    /// Re-derive from the last published facts (a prompt was decided here).
    pub(crate) fn rederive_health(&self, now: i64) -> Option<SessionHealth> {
        let changed = self.health.recompute(
            now,
            |id| self.is_decided(id),
            |working| {
                self.set_working(working);
            },
        );
        self.log_change(changed.clone());
        changed
    }

    /// The thresholds this session derives with (the host config).
    pub fn health_config(&self) -> HealthConfig {
        *self.health.cfg.read().unwrap()
    }

    /// The facts last published.
    pub fn health_inputs(&self) -> HealthInputs {
        self.health.inputs.borrow().clone()
    }

    /// The session's health now.
    pub fn health(&self) -> SessionHealth {
        self.health.health.borrow().clone()
    }

    /// Follow the session's health.
    pub fn subscribe_health(&self) -> watch::Receiver<SessionHealth> {
        self.health.health.subscribe()
    }

    /// The driver's facts and the health derived from them, read as one.
    ///
    /// `publish_health` writes the facts and then derives the health from
    /// them, so the two watches move one after the other. A reader that caught
    /// the gap would pair a prompt id with a health that no longer has that
    /// prompt — or the reverse — and mint an alert for something already
    /// answered. Taken under the same lock the derivation holds, so there is
    /// no gap to catch.
    pub fn health_snapshot(&self) -> Snapshot {
        self.health.coherent()
    }

    /// Follow the driver's raw facts.
    ///
    /// Needed beside [`Self::subscribe_health`] because two things a person
    /// has to act on can derive the same health: one permission request
    /// answered and the next arriving in the same millisecond leave the status
    /// and the activity stamp alike, so the health watch stays quiet while the
    /// request id — which is what tells the two prompts apart — has changed.
    pub fn subscribe_health_inputs(&self) -> watch::Receiver<HealthInputs> {
        self.health.inputs.subscribe()
    }

    /// The host's periodic look: fold in the children's activity and re-derive
    /// at `now` (a quiet session crosses the stall threshold here).
    pub fn refresh_health(&self, now: i64, children: ChildActivity) -> Option<SessionHealth> {
        *self.health.children.lock().unwrap() = children;
        if !self.health.published.load(Ordering::Relaxed) {
            return None;
        }
        self.rederive_health(now)
    }

    /// The host's clock slept: silence up to `now` is forgiven.
    pub fn forgive_until(&self, now: i64) {
        self.health.forgiven_until.fetch_max(now, Ordering::Relaxed);
    }

    fn log_change(&self, changed: Option<SessionHealth>) {
        let Some(h) = changed else { return };
        tracing::info!(
            incarnation = %self.incarnation(),
            status = ?h.status,
            liveness = h.liveness.wire_name(),
            stalled = h.stalled_since.is_some(),
            reason = h.reason.as_deref().unwrap_or(""),
            "health"
        );
    }
}

/// The working children of `parent` as `(working, latest activity)`, from
/// their own health.
pub fn child_activity<'a>(children: impl IntoIterator<Item = &'a SessionHandle>) -> ChildActivity {
    let mut out = ChildActivity::default();
    for child in children {
        let h = child.health();
        if h.status == HealthStatus::Working {
            out.working += 1;
            out.last_activity_at = out.last_activity_at.max(h.last_activity_at);
        }
    }
    out
}

#[cfg(test)]
#[path = "session_health_tests.rs"]
mod tests;
