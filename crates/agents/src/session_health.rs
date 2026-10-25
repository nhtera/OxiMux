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

use crate::health::{self, HealthConfig, HealthInputs, HealthStatus, SessionHealth};
use crate::session_registry::SessionHandle;

/// Wall clock, Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    /// The stall clock restarts no earlier than this (the host's last wake).
    forgiven_until: AtomicI64,
    cfg: Arc<RwLock<HealthConfig>>,
    /// Held across read-inputs → write-health, so a slower derivation (the
    /// host's tick) never overwrites one made from newer inputs.
    derive: Mutex<()>,
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
            cfg,
            derive: Mutex::new(()),
        }
    }

    /// Re-derive at `now`. Returns the new health when what a person would
    /// read changed (status, liveness, stall, reason), and whether the session
    /// counts as working.
    fn recompute(&self, now: i64) -> (Option<SessionHealth>, bool) {
        let _derive = self.derive.lock().unwrap();
        let mut inputs = self.inputs.borrow().clone();
        let children = *self.children.lock().unwrap();
        inputs.children_active = children.working;
        inputs.last_activity_at = inputs
            .last_activity_at
            .max(self.forgiven_until.load(Ordering::Relaxed))
            .max(if children.working > 0 { children.last_activity_at } else { 0 });
        let cfg = *self.cfg.read().unwrap();
        // A continuation whose closing frame never came would keep an idle
        // session working forever; past the stall window with no host turn,
        // the count is stale.
        if health::continuations_stale(&inputs, now, &cfg) {
            inputs.continuations_open = 0;
        }
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
        (changed, working)
    }
}

impl SessionHandle {
    /// Publish the driver's facts. Re-derives the health and the coarse
    /// `working` flag the lineage parks idle children by (a continuation or an
    /// unanswered send counts, so a child still reacting to its background
    /// tasks keeps its slot).
    pub fn publish_health(&self, inputs: HealthInputs) {
        let modified = self.health.inputs.send_if_modified(|current| {
            if *current == inputs {
                return false;
            }
            *current = inputs;
            true
        });
        if modified {
            let (changed, working) = self.health.recompute(now_ms());
            self.log_change(changed);
            self.set_working(working);
        }
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

    /// The host's periodic look: fold in the children's activity and re-derive
    /// at `now` (a quiet session crosses the stall threshold here).
    pub fn refresh_health(&self, now: i64, children: ChildActivity) -> Option<SessionHealth> {
        *self.health.children.lock().unwrap() = children;
        let (changed, working) = self.health.recompute(now);
        self.log_change(changed.clone());
        self.set_working(working);
        changed
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
