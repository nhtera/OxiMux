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
//! The edge is what makes a banner safe to post: the same alert re-derived (a
//! health re-snapshot, a reconnect, a tab reopening) is the same edge, and an
//! unchanged edge is not republished.
//!
//! This is a stream of what each row should say, not of what should be
//! announced. An alert covered by a worse one and uncovered again is current
//! again, so it is published again — the row has to show it. Announcing each
//! edge at most once is the notifier bridge's rule, kept there with the set of
//! edges it has already alerted on, because only the bridge knows what it has
//! put on screen.
//!
//! **One lock per transition.** Deriving a target's attention, recording it
//! and broadcasting it happen together under
//! [`AttentionSlots::state`]. Two threads can reach the same session at once —
//! a health follower and a person's click — and a derivation that read the
//! world before the other thread's write must not be installed after it. Doing
//! the three steps apart let a dismissed failure come back, and let a
//! retirement be broadcast before the raise it retires.
//!
//! **Targets are namespaced.** A target is the key its preferences are stored
//! under: `chat:<session>`, `cockpit:<db id>`, `team:<run>`, `plan:<run>`.
//! Deriving the namespace from the shape of a bare id would put a chat whose
//! provider happened to name it `plan:7` on the same row, and the same
//! preference record, as the run `plan:7`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use oximux_agents::attention::{
    self, Attention, AttentionEdge, EdgeOnset, ExternalAttention,
};
use oximux_agents::session_health::now_ms;
use oximux_agents::session_prefs::{SessionPrefs, SessionPrefsRepo};
use oximux_agents::session_registry::{SessionHandle, SessionId};
use tokio::sync::broadcast;

use crate::host_core::HostCore;

/// How many changes a slow subscriber may fall behind before it is told to
/// re-read instead. A rail that missed one still repaints from the snapshot.
const CHANGE_BUFFER: usize = 256;

/// How many past edges one target remembers the arrival time of. Enough for a
/// session's recent history; the oldest are dropped, and an edge whose arrival
/// is forgotten simply reads as new.
const ONSET_MEMORY: usize = 64;

/// The prefix a chat session's target carries.
const CHAT: &str = "chat:";

/// A target's attention became something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionChanged {
    /// Whose, as a namespaced target (`chat:…`, `cockpit:…`, `team:…`,
    /// `plan:…`).
    pub target: String,
    /// What it wants now, or `None` when it wants nothing.
    pub attention: Option<Attention>,
    /// The edge that is now current, if any.
    pub edge: Option<AttentionEdge>,
    /// The edge that just left — answered, viewed, dismissed or gone. A
    /// surface that alerted on it retires that alert; the push phase sends its
    /// dismissal from this same event, so a notification cannot outlive the
    /// thing it was about.
    ///
    /// Set only when the current edge is a *different* one: the same edge with
    /// fuller wording updates the row, and retiring and re-posting it would
    /// sound a second alert for a change of words.
    pub retired: Option<AttentionEdge>,
}

/// Everything the host holds for attention.
#[derive(Default)]
pub struct AttentionState {
    /// External alerts by their producer's key.
    external: HashMap<String, ExternalAttention>,
    /// The attention last published per target, so a re-derivation that says
    /// the same thing is not republished and a departure can name what left.
    last: HashMap<String, (Attention, AttentionEdge)>,
    /// When each edge was first seen, per target. This is what "has the person
    /// seen it?" is measured against: a session's own timestamps cannot say,
    /// because the moment a stall is declared is minutes after the silence it
    /// is measured from, and a process dies after its last output. Kept per
    /// edge rather than per target so an alert outranked by a worse one and
    /// then uncovered again is still judged by when it actually arrived.
    onset: HashMap<String, Vec<(String, i64)>>,
}

impl AttentionState {
    /// When `edge` was first seen on its target.
    fn raised_at(&self, edge: &AttentionEdge) -> Option<i64> {
        let seen = self.onset.get(&edge.target)?;
        seen.iter().find(|(id, _)| *id == edge.id).map(|(_, at)| *at)
    }

    /// Remember that `edge` was seen at `now`, if it is not already known.
    fn record(&mut self, edge: &AttentionEdge, now: i64) {
        let seen = self.onset.entry(edge.target.clone()).or_default();
        if seen.iter().any(|(id, _)| *id == edge.id) {
            return;
        }
        if seen.len() >= ONSET_MEMORY {
            seen.remove(0);
        }
        seen.push((edge.id.clone(), now));
    }
}

/// [`AttentionState`] read as the evaluator's edge history.
struct Onset<'a>(&'a AttentionState);

impl EdgeOnset for Onset<'_> {
    fn raised_at(&self, edge: &AttentionEdge) -> Option<i64> {
        self.0.raised_at(edge)
    }
}

/// The target a chat session's attention is keyed by.
pub fn chat_target(session: &str) -> String {
    format!("{CHAT}{session}")
}

impl HostCore {
    /// Follow attention changes in this process.
    ///
    /// Subscribe **before** reading [`HostCore::needs_you`] or
    /// [`HostCore::attention_of`]: the current state is not replayed, so a
    /// change between the snapshot and the subscription would reach the
    /// subscriber neither way. Subscribing first means such a change arrives
    /// as an event the subscriber reconciles against its snapshot.
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
        let moved_from = {
            let mut state = self.attention().state.lock().unwrap();
            if state.external.get(&item.key) == Some(&item) {
                return;
            }
            // A producer may move its key to another target (a run's alert
            // re-attributed to a role). The target it left keeps a published
            // edge nothing else would ever retire.
            let previous = state.external.insert(item.key.clone(), item);
            previous.map(|p| p.target).filter(|t| *t != target)
        };
        self.reevaluate(&target);
        if let Some(old) = moved_from {
            self.reevaluate(&old);
        }
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
        external_for(&state, target)
    }

    /// What one target wants from a person, computed now.
    pub fn attention_of(&self, target: &str) -> Option<Attention> {
        let mut state = self.attention().state.lock().unwrap();
        self.current(target, &mut state, now_ms())
    }

    /// Every target that needs a person, worst first. This is the number the
    /// chip, the nav badge and the hotkey cycle all read, so they cannot
    /// disagree.
    pub fn needs_you(&self) -> Vec<(String, Attention)> {
        let now = now_ms();
        let mut state = self.attention().state.lock().unwrap();
        let mut out: Vec<(String, Attention)> = targets(&state, self.registry().ids())
            .into_iter()
            .filter_map(|target| {
                let a = self.current(&target, &mut state, now)?;
                a.kind.is_needs_you().then_some((target, a))
            })
            .collect();
        drop(state);
        // Worst kind first, then the one waiting longest, then by target: the
        // order the rail renders and the order the hotkey walks, and one that
        // cannot shuffle between passes.
        out.sort_by(|(ka, a), (kb, b)| {
            (a.kind.priority(), a.since, ka).cmp(&(b.kind.priority(), b.since, kb))
        });
        out
    }

    /// Every target attention could be raised for: the live sessions and
    /// whatever external producers have named.
    pub fn attention_targets(&self) -> Vec<String> {
        let state = self.attention().state.lock().unwrap();
        targets(&state, self.registry().ids())
    }

    /// The person has `target` in front of them, or dismissed one of its
    /// alerts: everything raised before now has been seen.
    ///
    /// Writing the stamp is what clears a Failed or Stalled row, so this is
    /// the one call behind both "viewed" and "Dismiss".
    pub fn mark_viewed(&self, target: &str) {
        if let Err(err) = self.attention().prefs.mark_viewed(target, chrono::Local::now()) {
            tracing::warn!(%err, target, "could not record that a session was viewed");
            return;
        }
        self.reevaluate(target);
    }

    /// Keep `target` standing out although it has been viewed.
    pub fn mark_unread(&self, target: &str) {
        if let Err(err) = self.attention().prefs.mark_unread(target, chrono::Local::now()) {
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
        let now = now_ms();
        // Derive, record and broadcast as one step. Apart, a derivation that
        // read the world before another thread's `mark_viewed` could be
        // installed after that thread had already retired the alert, and a
        // retirement could be broadcast before the raise it retires.
        let mut state = self.attention().state.lock().unwrap();
        let attention = self.current(target, &mut state, now);
        let edge = attention.as_ref().map(|a| self.edge_of(target, a));
        let previous = state.last.get(target).cloned();
        let change = match (&attention, &edge, previous) {
            // The same edge with the same line: nothing to hear about.
            (Some(a), Some(e), Some((prev_a, prev_e))) if *e == prev_e && a.reason == prev_a.reason => {
                return;
            }
            (Some(a), Some(e), previous) => {
                state.last.insert(target.to_string(), (a.clone(), e.clone()));
                AttentionChanged {
                    target: target.to_string(),
                    attention: attention.clone(),
                    edge: edge.clone(),
                    retired: previous.filter(|(_, prev_e)| prev_e != e).map(|(_, prev_e)| prev_e),
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
        };
        // Still under the lock, so two threads cannot reverse a raise and its
        // retirement on the wire. `send` on a broadcast channel does not
        // block: it writes into a ring and wakes the receivers.
        let _ = self.attention().tx.send(change);
    }

    /// What `target` wants now, recording when any alert it raises first
    /// appeared.
    ///
    /// The recording is what makes a dismissal possible: nothing in a
    /// session's own state says when an alert arose, so the first time the host
    /// sees an edge is the host's answer, and a view after that moment is a
    /// view of it. Recorded after deriving, so an alert is never born already
    /// seen.
    fn current(
        &self,
        target: &str,
        state: &mut AttentionState,
        now: i64,
    ) -> Option<Attention> {
        let external = external_for(state, target);
        let prefs = self.prefs_of(target);
        let attention = match self.session_of(target) {
            Some(handle) => {
                attention::of(target, &handle.health(), &prefs, &external, &Onset(state))
            }
            // No session of its own (a terminal agent, a run root): its alerts
            // are the ones its producer raised.
            None => attention::of_external(target, &prefs, &external, &Onset(state)),
        }?;
        state.record(&self.edge_of(target, &attention), now);
        Some(attention)
    }

    /// `target`'s alert identity, with the pending request id the session
    /// handle holds (health alone does not carry it).
    fn edge_of(&self, target: &str, attention: &Attention) -> AttentionEdge {
        match self.session_of(target) {
            Some(handle) => {
                let health = handle.health();
                let request = handle.health_inputs().prompt_request;
                attention::edge_of(target, attention, Some(&health), request.as_deref())
            }
            None => attention::edge_of(target, attention, None, None),
        }
    }

    /// The live session a target names, if it names one.
    fn session_of(&self, target: &str) -> Option<Arc<SessionHandle>> {
        self.registry().get(target.strip_prefix(CHAT)?)
    }

    /// What the person has already seen of `target`. An unreadable row reads
    /// as never viewed: an alert shown twice beats one swallowed.
    pub fn prefs_of(&self, target: &str) -> SessionPrefs {
        self.attention().prefs.get(target).unwrap_or_else(|err| {
            tracing::warn!(%err, target, "could not read session prefs");
            SessionPrefs::default()
        })
    }

    /// Follow every live session's facts and re-evaluate its attention as they
    /// change. One task per registered handle, which ends with the handle: a
    /// session's own watches are the only signal that says when a person is
    /// needed, and the 30-second stall tick is far too coarse for a banner.
    ///
    /// Only while something is listening. The snapshot calls compute on
    /// demand, so the followers exist purely to feed the stream: with no
    /// subscriber they would read the database on every health change for
    /// nobody. That also keeps a host built in a test — where nothing
    /// subscribes — from doing work on a runtime thread behind the test's back.
    pub(crate) fn spawn_attention_watchers(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut changes = self.registry().subscribe_changes();
        self.runtime().spawn(async move {
            let mut known: HashSet<String> = HashSet::new();
            loop {
                {
                    let Some(core) = weak.upgrade() else { return };
                    let live: HashSet<String> =
                        core.registry().ids().into_iter().map(|id| chat_target(&id)).collect();
                    core.follow_live_sessions();
                    // A session that left the registry wants nothing, whether
                    // or not its watches have closed: a view still holding its
                    // handle keeps them open, and its follower would never
                    // wake to retire the alert.
                    for target in known.difference(&live) {
                        core.reevaluate(target);
                    }
                    known = live;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// Start a follower for every live session that has none. A no-op while
    /// nothing subscribes to the stream.
    ///
    /// Followed by **handle**, not by session id. A session can be
    /// unregistered and registered again under the same id while a view still
    /// holds the old handle; keyed by id alone, the new handle would be taken
    /// for one already followed and its changes would never reach the stream.
    fn follow_live_sessions(self: &Arc<Self>) {
        if self.attention().tx.receiver_count() == 0 {
            return;
        }
        for id in self.registry().ids() {
            let Some(handle) = self.registry().get(&id) else { continue };
            let key = (id.clone(), handle.incarnation().to_string());
            if !self.attention().followed.lock().unwrap().insert(key.clone()) {
                continue;
            }
            let health = handle.subscribe_health();
            let inputs = handle.subscribe_health_inputs();
            // Weak, so following a session never keeps it alive.
            self.runtime().spawn(follow(Arc::downgrade(self), key, health, inputs));
        }
    }
}

/// Re-evaluate one session's attention whenever its facts change, then once
/// more when its watches close — the session ended, and that is exactly when a
/// surface has to stop showing it as needing anything.
///
/// Both watches, because either can move alone. Health alone misses two
/// successive prompts that derive the same health (the request id is what tells
/// them apart, and it lives in the inputs); the inputs alone miss a stall,
/// which the host's clock declares without the driver publishing anything.
async fn follow(
    core: std::sync::Weak<HostCore>,
    key: (SessionId, String),
    mut health: tokio::sync::watch::Receiver<oximux_agents::health::SessionHealth>,
    mut inputs: tokio::sync::watch::Receiver<oximux_agents::health::HealthInputs>,
) {
    let target = chat_target(&key.0);
    loop {
        {
            let Some(core) = core.upgrade() else { return };
            core.reevaluate(&target);
        }
        let ended = tokio::select! {
            changed = health.changed() => changed.is_err(),
            changed = inputs.changed() => changed.is_err(),
        };
        if ended {
            break;
        }
    }
    if let Some(core) = core.upgrade() {
        core.attention().followed.lock().unwrap().remove(&key);
        core.reevaluate(&target);
    }
}

/// Every external alert raised for `target`, worst first.
fn external_for(state: &AttentionState, target: &str) -> Vec<ExternalAttention> {
    let mut items: Vec<_> =
        state.external.values().filter(|i| i.target == target).cloned().collect();
    // By key as well, so a tie on kind and millisecond does not depend on the
    // order a `HashMap` happened to iterate.
    items.sort_by(|a, b| {
        (a.kind.priority(), a.since, &a.key).cmp(&(b.kind.priority(), b.since, &b.key))
    });
    items
}

/// The live sessions and the targets external producers have named.
fn targets(state: &AttentionState, sessions: Vec<SessionId>) -> Vec<String> {
    let mut out: Vec<String> = sessions
        .into_iter()
        .map(|id| chat_target(&id))
        .chain(state.external.values().map(|i| i.target.clone()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The handles [`AttentionState`] is reached through, so `host_core.rs` keeps
/// the fields private and this module keeps the behaviour.
pub(crate) struct AttentionSlots {
    pub(crate) state: Mutex<AttentionState>,
    pub(crate) tx: broadcast::Sender<AttentionChanged>,
    /// Handles a follower task is already watching, by session id and
    /// incarnation.
    pub(crate) followed: Mutex<HashSet<(SessionId, String)>>,
    pub(crate) prefs: SessionPrefsRepo,
}

impl AttentionSlots {
    pub(crate) fn new(prefs: SessionPrefsRepo) -> Self {
        Self {
            state: Mutex::new(AttentionState::default()),
            tx: broadcast::channel(CHANGE_BUFFER).0,
            followed: Mutex::new(HashSet::new()),
            prefs,
        }
    }
}

#[cfg(test)]
#[path = "attention_tests.rs"]
mod tests;
