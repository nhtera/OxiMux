//! The host's one attention owner.
//!
//! Every surface that shows "needs you" — the rail, the status chip, the nav
//! badge, the jump-to-next hotkey, the notifier, and later the phone — reads
//! from here. The evaluator itself is the pure
//! [`oximux_agents::attention::of`]; this module is what feeds it (a session's
//! health, what the person has already seen, and the alerts no session's
//! health can know about) and what publishes the result.
//!
//! **One owner, because two disagreed.** The dashboard, the badge, the palette
//! and two notification paths each had their own idea of what needs a person,
//! and the counts did not match. A single stream means a banner and a rail row
//! can no longer tell different stories.
//!
//! **Changes, not snapshots.** A subscriber is told when a target's attention
//! becomes something else, with the edge that arrived and the edge that left.
//! The edge is what makes a banner safe to post: the same alert re-derived
//! (a health re-snapshot, a reconnect, a tab reopening) is the same edge and
//! is published once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oximux_agents::attention::{self, Attention, AttentionEdge, ExternalAttention};
use oximux_agents::session_prefs::{SessionPrefs, SessionPrefsRepo};
use oximux_agents::session_registry::SessionId;
use tokio::sync::broadcast;

use crate::host_core::HostCore;

/// How many changes a slow subscriber may fall behind before it is told to
/// re-read instead. A rail that missed one still repaints from the snapshot.
const CHANGE_BUFFER: usize = 256;

/// A target's attention became something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionChanged {
    /// Whose: a session id, a cockpit key, or a run root.
    pub target: String,
    /// What it wants now, or `None` when it wants nothing.
    pub attention: Option<Attention>,
    /// The edge that is now current, if any.
    pub edge: Option<AttentionEdge>,
    /// The edge that just left — answered, viewed, dismissed or gone. A
    /// surface that alerted on it retires that alert; the push phase sends its
    /// dismissal from this same event, so a notification cannot outlive the
    /// thing it was about.
    pub retired: Option<AttentionEdge>,
}

/// Everything the host holds for attention: the alerts it was told about and
/// the last edge it published per target.
#[derive(Default)]
pub struct AttentionState {
    /// External alerts by their producer's key.
    external: HashMap<String, ExternalAttention>,
    /// The last attention published per target, so a re-derivation that says
    /// the same thing is not republished and a departure can name what left.
    last: HashMap<String, (Attention, AttentionEdge)>,
}

impl HostCore {
    /// Follow attention changes in this process. The current state is not
    /// replayed: a new subscriber reads [`HostCore::needs_you`] (or
    /// [`HostCore::attention_of`]) first and then follows this.
    pub fn subscribe_attention(self: &Arc<Self>) -> broadcast::Receiver<AttentionChanged> {
        let rx = self.attention().tx.subscribe();
        // Sessions already live when the first surface subscribes need their
        // followers now; the watcher loop only wakes when the registry
        // changes.
        self.follow_live_sessions();
        rx
    }

    /// Raise or update an alert no session's health can know about: a terminal
    /// agent's status (it has no health at all), CI out of fix attempts, a
    /// plan run whose ticket tripped its breaker.
    ///
    /// Idempotent by `key` and `since`: a producer re-publishing on boot from
    /// its own persisted state produces the same edge and so no second alert.
    /// Nothing here is persisted, precisely because every producer can do that.
    pub fn publish_external(&self, item: ExternalAttention) {
        let target = item.target.clone();
        {
            let mut state = self.attention().state.lock().unwrap();
            if state.external.get(&item.key) == Some(&item) {
                return;
            }
            state.external.insert(item.key.clone(), item);
        }
        self.reevaluate(&target);
    }

    /// Clear one external alert. Producers clear their own keys: nothing else
    /// knows when CI passed or a ticket was retried.
    pub fn clear_external(&self, key: &str) {
        let target = {
            let mut state = self.attention().state.lock().unwrap();
            match state.external.remove(key) {
                Some(item) => item.target,
                None => return,
            }
        };
        self.reevaluate(&target);
    }

    /// Every external alert currently raised for `target`.
    pub fn external_for(&self, target: &str) -> Vec<ExternalAttention> {
        let state = self.attention().state.lock().unwrap();
        let mut items: Vec<_> =
            state.external.values().filter(|i| i.target == target).cloned().collect();
        items.sort_by_key(|i| (i.kind.priority(), i.since));
        items
    }

    /// What one target wants from a person, computed now.
    pub fn attention_of(&self, target: &str) -> Option<Attention> {
        let external = self.external_for(target);
        let prefs = self.prefs_of(target);
        match self.registry().get(target) {
            Some(handle) => attention::of(&handle.health(), &prefs, &external),
            // No session of its own (a terminal agent, a run root): its alerts
            // are the ones its producer raised.
            None => attention::of_external(&prefs, &external),
        }
    }

    /// Every target that needs a person, worst first. This is the number the
    /// chip, the nav badge and the hotkey cycle all read, so they cannot
    /// disagree.
    pub fn needs_you(&self) -> Vec<(String, Attention)> {
        let mut out: Vec<(String, Attention)> = self
            .attention_targets()
            .into_iter()
            .filter_map(|target| {
                let a = self.attention_of(&target)?;
                a.kind.is_needs_you().then_some((target, a))
            })
            .collect();
        // Worst kind first, then the one waiting longest: the order the rail
        // renders and the order the hotkey walks.
        out.sort_by(|(ka, a), (kb, b)| {
            (a.kind.priority(), a.since, ka).cmp(&(b.kind.priority(), b.since, kb))
        });
        out
    }

    /// Every target attention could be raised for: the live sessions and
    /// whatever external producers have named.
    pub fn attention_targets(&self) -> Vec<String> {
        let mut targets: Vec<String> =
            self.registry().ids().into_iter().chain(self.external_targets()).collect();
        targets.sort();
        targets.dedup();
        targets
    }

    fn external_targets(&self) -> Vec<String> {
        self.attention().state.lock().unwrap().external.values().map(|i| i.target.clone()).collect()
    }

    /// The person has `target` in front of them, or dismissed one of its
    /// alerts: everything raised before now has been seen.
    ///
    /// Writing the stamp is what clears a Failed or Stalled row, so this is
    /// the one call behind both "viewed" and "Dismiss".
    pub fn mark_viewed(&self, target: &str) {
        if let Err(err) = self.attention().prefs.mark_viewed(&self.prefs_key(target), chrono::Local::now()) {
            tracing::warn!(%err, target, "could not record that a session was viewed");
            return;
        }
        self.reevaluate(target);
    }

    /// Keep `target` standing out although it has been viewed.
    pub fn mark_unread(&self, target: &str) {
        if let Err(err) = self.attention().prefs.mark_unread(&self.prefs_key(target), chrono::Local::now()) {
            tracing::warn!(%err, target, "could not mark a session unread");
            return;
        }
        self.reevaluate(target);
    }

    /// Re-read `target`'s attention and publish it if it became something
    /// else. Called on every health change, prefs write and external update.
    ///
    /// "Something else" means a different edge or a different line to read.
    /// When the session merely did something — the activity timestamp moving —
    /// nothing is published: the events already say that, and a surface that
    /// re-alerted on it would alert constantly.
    pub fn reevaluate(&self, target: &str) {
        let now = self.attention_of(target);
        let edge = now.as_ref().map(|a| self.edge_of(target, a));
        let change = {
            let mut state = self.attention().state.lock().unwrap();
            let previous = state.last.get(target).cloned();
            match (&now, &edge, previous) {
                // The same edge with the same line: nothing to hear about.
                (Some(a), Some(e), Some((prev_a, prev_e)))
                    if *e == prev_e && a.reason == prev_a.reason =>
                {
                    return;
                }
                (Some(a), Some(e), previous) => {
                    state.last.insert(target.to_string(), (a.clone(), e.clone()));
                    AttentionChanged {
                        target: target.to_string(),
                        attention: now.clone(),
                        edge: edge.clone(),
                        // Only a *different* edge retires the one before it.
                        // The same edge whose line changed must not be
                        // retired and re-posted: that would sound a second
                        // alert for a cosmetic change.
                        retired: previous
                            .filter(|(_, prev_e)| prev_e != e)
                            .map(|(_, prev_e)| prev_e),
                    }
                }
                (_, _, None) => return,
                (_, _, Some((_, prev_e))) => {
                    state.last.remove(target);
                    AttentionChanged {
                        target: target.to_string(),
                        attention: None,
                        edge: None,
                        retired: Some(prev_e),
                    }
                }
            }
        };
        // Outside the lock: a subscriber that reacts by asking for attention
        // again must not deadlock on it.
        let _ = self.attention().tx.send(change);
    }

    /// `target`'s alert identity, with the pending request id the session
    /// handle holds (health alone does not carry it).
    fn edge_of(&self, target: &str, attention: &Attention) -> AttentionEdge {
        match self.registry().get(target) {
            Some(handle) => {
                let health = handle.health();
                let request = handle.health_inputs().prompt_request;
                attention::edge_of(target, attention, Some(&health), request.as_deref())
            }
            None => attention::edge_of(target, attention, None, None),
        }
    }

    /// What the person has already seen of `target`. An unreadable row reads
    /// as never viewed: an alert shown twice beats one swallowed.
    pub fn prefs_of(&self, target: &str) -> SessionPrefs {
        self.attention().prefs.get(&self.prefs_key(target)).unwrap_or_else(|err| {
            tracing::warn!(%err, target, "could not read session prefs");
            SessionPrefs::default()
        })
    }

    /// A target's prefs key. A run root and a terminal agent already carry
    /// their own prefix; a session id is a chat.
    pub fn prefs_key(&self, target: &str) -> String {
        if target.starts_with("team:") || target.starts_with("plan:") || target.starts_with("cockpit:") {
            return target.to_string();
        }
        oximux_agents::session_prefs::chat_key(target)
    }

    /// Follow every live session's health and re-evaluate its attention as it
    /// changes. One task per session, which ends with the session: a session's
    /// health watch is the only signal that says when a person is needed, and
    /// the 30-second stall tick is far too coarse for a banner.
    ///
    /// Only while something is listening. The snapshot calls compute on
    /// demand, so the followers exist purely to feed the stream: with no
    /// subscriber they would read the database on every health change for
    /// nobody. That also keeps a host built in a test — where nothing
    /// subscribes — from doing work on a runtime thread behind the test's back
    /// ([[gpui-test-scheduler-forbids-foreign-thread-wakes]]).
    pub(crate) fn spawn_attention_watchers(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut changes = self.registry().subscribe_changes();
        self.runtime().spawn(async move {
            let mut known: std::collections::HashSet<SessionId> = Default::default();
            loop {
                {
                    let Some(core) = weak.upgrade() else { return };
                    let live = core.follow_live_sessions();
                    // A session that left the registry wants nothing, whether
                    // or not its health watch has closed: a view still holding
                    // its handle keeps that watch open, and its follower would
                    // never wake to retire the alert.
                    for id in known.difference(&live) {
                        core.reevaluate(id);
                    }
                    known = live;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// Start a follower for every live session that has none, and answer with
    /// the live set. A no-op while nothing subscribes to the stream.
    fn follow_live_sessions(self: &Arc<Self>) -> std::collections::HashSet<SessionId> {
        let live: std::collections::HashSet<SessionId> =
            self.registry().ids().into_iter().collect();
        if self.attention().tx.receiver_count() == 0 {
            return live;
        }
        for id in &live {
            if !self.attention().followed.lock().unwrap().insert(id.clone()) {
                continue;
            }
            let Some(handle) = self.registry().get(id) else {
                self.attention().followed.lock().unwrap().remove(id);
                continue;
            };
            let rx = handle.subscribe_health();
            // Weak, so following a session never keeps it alive.
            self.runtime().spawn(follow(Arc::downgrade(self), id.clone(), rx));
        }
        live
    }
}

/// Re-evaluate one session's attention on every health change, then once more
/// when its watch closes — the session ended, and that is exactly when a
/// surface has to stop showing it as needing anything.
async fn follow(
    core: std::sync::Weak<HostCore>,
    id: SessionId,
    mut rx: tokio::sync::watch::Receiver<oximux_agents::health::SessionHealth>,
) {
    loop {
        {
            let Some(core) = core.upgrade() else { return };
            core.reevaluate(&id);
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(core) = core.upgrade() {
        core.attention().followed.lock().unwrap().remove(&id);
        // The session is gone: whatever it wanted, it no longer does.
        core.reevaluate(&id);
    }
}

/// The handles [`AttentionState`] is reached through, so `host_core.rs` keeps
/// the fields private and this module keeps the behaviour.
pub(crate) struct AttentionSlots {
    pub(crate) state: Mutex<AttentionState>,
    pub(crate) tx: broadcast::Sender<AttentionChanged>,
    /// Sessions a follower task is already watching.
    pub(crate) followed: Mutex<std::collections::HashSet<SessionId>>,
    pub(crate) prefs: SessionPrefsRepo,
}

impl AttentionSlots {
    pub(crate) fn new(prefs: SessionPrefsRepo) -> Self {
        Self {
            state: Mutex::new(AttentionState::default()),
            tx: broadcast::channel(CHANGE_BUFFER).0,
            followed: Mutex::new(std::collections::HashSet::new()),
            prefs,
        }
    }
}

#[cfg(test)]
#[path = "attention_tests.rs"]
mod tests;
