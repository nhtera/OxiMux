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
    self, Attention, AttentionEdge, AttentionKind, AttentionSource, ExternalAttention, SeenEdges,
};
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
    ///
    /// Keyed by kind *and* id, because an id alone is not an alert: a finished
    /// turn and a later failure of that same turn share the turn's id, and a
    /// viewed finish must not hide the failure that followed it.
    ///
    /// The value is this process's observation counter, not a clock. Two
    /// things can happen within one millisecond — a look, then a failure — and
    /// a timestamp cannot order them, so a failure arising just after a look
    /// read as covered by it. A counter cannot tie.
    onset: HashMap<String, Vec<(EdgeKey, u64)>>,
    /// When each status-derived alert first appeared, by its producer's key.
    ///
    /// A cockpit row's state is a PTY-parsed `AgentStatus` with no moment of
    /// its own: the row carries its launch time, fixed at insert, and its end
    /// time once it has ended — an ambient row carries neither. Since an
    /// external alert's edge identity is its key *and* its `since`, taking
    /// that value from the row made the same unchanged approval a new alert on
    /// every refresh, so a viewed alert read as unread and the notifier
    /// announced it again; and a live row's launch time made a *second*
    /// approval the same alert as the first, so it raised nothing.
    ///
    /// Held here because both the producer and the row resolver ask about the
    /// same row in the same pass, and the two must agree. Whichever asks
    /// first fixes the moment; it changes when the alert does and no sooner.
    ///
    /// One entry per cockpit row seen in this run — as `onset` holds one per
    /// target — because the moment an alert ended is the floor the alert that
    /// follows it under that key has to beat.
    status_onset: HashMap<String, StatusOnset>,
    /// Where each target's last look sits in that same order, for the looks
    /// this process has seen.
    viewed_at: HashMap<String, u64>,
    /// Hands out the positions above.
    observed: u64,
}

impl AttentionState {
    /// Where `edge` sits in the observation order, if it has been seen.
    fn observed_at(&self, edge: &AttentionEdge) -> Option<u64> {
        let seen = self.onset.get(&edge.target)?;
        let key: EdgeKey = (edge.kind, edge.id.clone());
        seen.iter().find(|(k, _)| *k == key).map(|(_, at)| *at)
    }

    /// Where an alert of `kind` with `id` sits, for tests that pin the
    /// view-and-arrival ordering.
    #[cfg(test)]
    fn onset_for_test(&self, target: &str, kind: AttentionKind, id: &str) -> Option<u64> {
        self.observed_at(&AttentionEdge { target: target.to_string(), kind, id: id.to_string() })
    }

    /// Where a target's last look sits, for tests.
    #[cfg(test)]
    fn viewed_position_for_test(&self, target: &str) -> Option<u64> {
        self.viewed_at.get(target).copied()
    }

    /// The next position in the observation order.
    fn tick(&mut self) -> u64 {
        self.observed += 1;
        self.observed
    }

    /// When the alert `kind` + `reason` under `key` first appeared, stamping
    /// it if this is the first time it has been asked about.
    ///
    /// The kind and the reason together are the alert: a permission that
    /// became a failure is a different alert and gets its own moment, while
    /// the same permission asked about again keeps the one it already has.
    ///
    /// `began` is what the row itself says about when its state began, when it
    /// knows — and it is preferred over the clock for a reason that only shows
    /// across a restart. `status_onset` lives in memory, so after a restart
    /// every alert is asked about for the first time; stamping those with the
    /// clock dated them *after* the stored look, and a finish the person had
    /// read came back unread while a failure they had dismissed came back to
    /// NEEDS YOU. That is precisely what the stored look exists to prevent. A
    /// row's own end time survives the restart and is the moment the alert
    /// really arose.
    fn status_since(
        &mut self,
        key: &str,
        kind: AttentionKind,
        reason: &str,
        began: Option<i64>,
    ) -> i64 {
        let alert = (kind, reason.to_string());
        let floor = match self.status_onset.get(key) {
            Some(held) if held.alert.as_ref() == Some(&alert) => return held.at,
            // A different alert under a key that has held one: never the
            // moment that one had. Two things can happen inside a
            // millisecond, and an alert sharing both its key and its moment
            // with the one it replaces *is* that edge — so a look taken
            // between the two would swallow it.
            Some(held) => held.at + 1,
            None => i64::MIN,
        };
        let at = began.unwrap_or_else(|| chrono::Local::now().timestamp_millis()).max(floor);
        self.status_onset.insert(key.to_string(), StatusOnset { alert: Some(alert), at });
        at
    }

    /// Forget `key`'s alert, so the next one raised under it is a new alert
    /// rather than the one that just ended.
    ///
    /// The moment stays behind as the floor for that next one: the key is
    /// still the same row, and its next alert has to be a different edge from
    /// the one that just ended.
    fn forget_status(&mut self, key: &str) {
        if let Some(held) = self.status_onset.get_mut(key) {
            held.alert = None;
        }
    }

    /// Remember when each of `edges` was observed, then forget history down to
    /// the cap.
    ///
    /// `edges` is every alert the target could raise *right now*, and none of
    /// them is ever forgotten. That is the whole point of capping here rather
    /// than per edge: a live edge dropped to make room would be re-recorded on
    /// the next pass at a position *after* the look that was meant to cover
    /// it, so it would read as unseen again and no number of looks could
    /// dismiss it. Reachable whenever one target has more live alerts than
    /// [`ONSET_MEMORY`] — the previous code evicted the oldest entry on every
    /// insertion, live or not, so the row stayed in "needs you" for good.
    ///
    /// So the cap bounds *history*, and the live set bounds itself: the list
    /// may sit above [`ONSET_MEMORY`] while that many alerts are genuinely
    /// outstanding, and falls back to it as they clear.
    fn record_all(&mut self, target: &str, edges: &[AttentionEdge]) {
        for edge in edges {
            let at = self.tick();
            self.record(edge, at);
        }
        let live: HashSet<EdgeKey> = edges.iter().map(|e| (e.kind, e.id.clone())).collect();
        let Some(seen) = self.onset.get_mut(target) else { return };
        let mut excess = seen.len().saturating_sub(ONSET_MEMORY);
        if excess == 0 {
            return;
        }
        // In order, so the oldest no-longer-live edges go first.
        seen.retain(|(key, _)| {
            if excess > 0 && !live.contains(key) {
                excess -= 1;
                return false;
            }
            true
        });
    }

    /// Remember that `edge` was observed at `at`, if it is not already known.
    /// An edge already known keeps the position it had: that is when it
    /// arrived, and re-deriving it is not a new arrival.
    fn record(&mut self, edge: &AttentionEdge, at: u64) {
        let seen = self.onset.entry(edge.target.clone()).or_default();
        let key: EdgeKey = (edge.kind, edge.id.clone());
        if seen.iter().any(|(k, _)| *k == key) {
            return;
        }
        seen.push((key, at));
    }

    /// Whether `edge` was observed no later than its target's last look.
    ///
    /// Within one run this is an exact order, which is the point: a look and an
    /// alert can fall in the same millisecond and a clock cannot separate them.
    ///
    /// Across a restart the order is gone — only the stored look survives — so
    /// the alert's own `since` is compared against it instead. That is weaker,
    /// and knowingly so: a stall or a death whose `since` predates the last
    /// look reads as covered, where within a run it would not. The alternative
    /// is a restart replaying every old failure into the badge, which is the
    /// behaviour the stored look exists to prevent.
    fn covered_by_a_look(&self, edge: &AttentionEdge, since: i64, prefs: &SessionPrefs) -> bool {
        match (self.observed_at(edge), self.viewed_at.get(&edge.target)) {
            (Some(observed), Some(looked)) => observed <= *looked,
            // No look seen in this run: the stored one is all there is.
            (_, None) => prefs.last_viewed_at.is_some_and(|viewed| viewed >= since),
            // Never observed, and there *was* a look this run: it came after.
            (None, Some(_)) => false,
        }
    }
}

/// What identifies an edge within one target: an id alone is not an alert,
/// since a finished turn and a later failure of it share the turn's id.
type EdgeKey = (AttentionKind, String);

/// What one cockpit key is holding: the alert it raises, and the moment that
/// alert was stamped with — kept after the alert ends, as the floor the next
/// one must beat. See [`AttentionState::status_onset`].
struct StatusOnset {
    alert: Option<(AttentionKind, String)>,
    at: i64,
}

/// A caller's own reading of a cockpit row: the key its producer owns, and the
/// alert the row's parsed status raises, if any.
///
/// The key travels even when there is no alert. A published item under it has
/// to go in that case: the status is the newer word on the row, and a quiet
/// status means the request that was published has ended.
struct StatusView {
    key: String,
    item: Option<ExternalAttention>,
}

/// The alert a cockpit row's parsed status raises, stamped with the moment
/// that alert first appeared.
///
/// Both the producer and the row resolver come through here, so the item that
/// is published and the item a row shows are the same alert rather than two
/// readings of one. See [`AttentionState::status_onset`] for why the moment
/// cannot come from the row.
fn status_item(
    state: &mut AttentionState,
    key: &str,
    target: &str,
    status: &oximux_core::AgentStatus,
    completed_turn: bool,
    began: Option<i64>,
) -> Option<ExternalAttention> {
    let item = attention::external_of_agent_status(
        key.to_string(),
        target.to_string(),
        status,
        completed_turn,
        |kind, reason| state.status_since(key, kind, reason, began),
    );
    if item.is_none() {
        // Nothing to raise: whatever this key held has ended, and the next
        // alert under it is a new one.
        state.forget_status(key);
    }
    item
}

/// Every session's preferences as of one moment — see
/// [`HostCore::prefs_snapshot`].
pub struct PrefsSnapshot(HashMap<String, SessionPrefs>);

impl PrefsSnapshot {
    /// What the person had seen of `target` as of this reading. A target with
    /// no row has been neither viewed nor marked, which is the default.
    fn of(&self, target: &str) -> SessionPrefs {
        self.0.get(target).cloned().unwrap_or_default()
    }
}

/// [`AttentionState`] read as the evaluator's record of what has been seen.
struct Seen<'a>(&'a AttentionState, &'a SessionPrefs);

impl SeenEdges for Seen<'_> {
    fn is_seen(&self, edge: &AttentionEdge, since: i64) -> bool {
        self.0.covered_by_a_look(edge, since, self.1)
    }
}

/// The target a chat session's attention is keyed by. The same spelling as
/// its preference key, because they are the same thing: the key a rekey moves
/// has to be the key the host reads.
pub use oximux_agents::session_prefs::chat_key as chat_target;

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
            // The alert's moment goes with the alert. Kept, it would make the
            // next alert under this key the same one — raised before the look
            // that has since been taken, and so born already seen.
            state.forget_status(key);
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
        self.current_with(target, None, None, &mut state).0
    }

    /// What one target wants, with the edge that identifies it.
    ///
    /// The edge is what a notifier dedupes on, so a surface that announces
    /// things needs both halves from one read: taken apart, a health update
    /// in between could pair an alert with an identity from another moment.
    pub fn attention_with_edge(&self, target: &str) -> (Option<Attention>, Option<AttentionEdge>) {
        let mut state = self.attention().state.lock().unwrap();
        self.current(target, &mut state)
    }

    /// As [`HostCore::attention_of`], against a reading of the preferences the
    /// caller already has in hand.
    ///
    /// A surface asking about many rows takes one reading (see
    /// [`HostCore::prefs_snapshot`]) and passes it to each call, instead of
    /// paying a query per row.
    pub fn attention_of_in(&self, target: &str, prefs: &PrefsSnapshot) -> Option<Attention> {
        let mut state = self.attention().state.lock().unwrap();
        self.current_with(target, None, Some(prefs), &mut state).0
    }

    /// The attention of a cockpit (terminal) agent, from the status the caller
    /// is already holding.
    ///
    /// Terminal agents have no [`SessionHealth`], so their state arrives as a
    /// parsed `AgentStatus`. It is mapped by
    /// [`attention::external_of_agent_status`] — the one mapping, shared with
    /// the producer, and stamped by the one owner — and then ranked against
    /// whatever else was published against the same row, so a CI alert or a
    /// plan breaker on that row still competes with it.
    ///
    /// The caller's status is the newer word on its own key: it replaces an
    /// item the producer published under it, and a status that raises nothing
    /// clears it.
    pub fn attention_of_agent_status(
        &self,
        target: &str,
        status: &oximux_core::AgentStatus,
        completed_turn: bool,
        began: Option<i64>,
    ) -> Option<Attention> {
        self.agent_status_attention(target, status, completed_turn, began, None)
    }

    /// As [`HostCore::attention_of_agent_status`], against a reading of the
    /// preferences the caller already has in hand — the dashboard's per-pass
    /// reading, see [`HostCore::prefs_snapshot`].
    pub fn attention_of_agent_status_in(
        &self,
        target: &str,
        status: &oximux_core::AgentStatus,
        completed_turn: bool,
        began: Option<i64>,
        prefs: &PrefsSnapshot,
    ) -> Option<Attention> {
        self.agent_status_attention(target, status, completed_turn, began, Some(prefs))
    }

    fn agent_status_attention(
        &self,
        target: &str,
        status: &oximux_core::AgentStatus,
        completed_turn: bool,
        began: Option<i64>,
        prefs: Option<&PrefsSnapshot>,
    ) -> Option<Attention> {
        let mut state = self.attention().state.lock().unwrap();
        // Key and target are the same string for a cockpit item: the producer
        // owns one alert per row, so the row's key *is* its identity.
        let view = StatusView {
            key: target.to_string(),
            item: status_item(&mut state, target, target, status, completed_turn, began),
        };
        self.current_with(target, Some(view), prefs, &mut state).0
    }

    /// The alert a cockpit row's parsed status raises right now, for the
    /// producer that publishes it.
    ///
    /// The producer does not mint this itself: the alert's moment is the
    /// host's ([`AttentionState::status_onset`]), so what is published and
    /// what a row shows are the same alert rather than two readings of one.
    ///
    /// `began` is what the row says about when its state began — its end time,
    /// else its launch time — and is used only when an alert is first stamped.
    /// A caller that has no such moment passes `None` and gets the clock,
    /// which is right for a row that has none (an ambient terminal) and
    /// weaker for one that does: see [`AttentionState::status_since`].
    pub fn cockpit_item(
        &self,
        key: &str,
        target: &str,
        status: &oximux_core::AgentStatus,
        completed_turn: bool,
        began: Option<i64>,
    ) -> Option<ExternalAttention> {
        let mut state = self.attention().state.lock().unwrap();
        status_item(&mut state, key, target, status, completed_turn, began)
    }

    /// Every session's preferences in one query, for a pass that will ask
    /// about many rows.
    ///
    /// The dashboard resolves attention for every row it builds, and a point
    /// query per row put one SQLite read per row on the render path —
    /// including the renders an agent's own output causes. One reading, taken
    /// once and passed in, costs one query however many rows the pass holds.
    ///
    /// Deliberately short-lived: it is the table at one moment, so a pass
    /// holding it reads a consistent world and nothing has to invalidate it.
    pub fn prefs_snapshot(&self) -> PrefsSnapshot {
        match self.attention().prefs.all() {
            Ok(rows) => PrefsSnapshot(rows.into_iter().collect()),
            Err(err) => {
                tracing::warn!(%err, "could not read session prefs");
                // Every row then reads as never viewed, the same reading a
                // failed single read takes: an alert shown twice beats one
                // swallowed.
                PrefsSnapshot(HashMap::new())
            }
        }
    }

    /// Every target that needs a person, worst first. This is the number the
    /// chip, the nav badge and the hotkey cycle all read, so they cannot
    /// disagree.
    pub fn needs_you(&self) -> Vec<(String, Attention)> {
        // One reading of the preferences for the whole walk: every target is
        // asked about, so a query each would scale with the session count.
        let prefs = self.prefs_snapshot();
        let mut state = self.attention().state.lock().unwrap();
        let mut out: Vec<(String, Attention)> = targets(&state, self.registry().ids())
            .into_iter()
            .filter_map(|target| {
                let a = self.current_with(&target, None, Some(&prefs), &mut state).0?;
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
        // The view and what it covers are **one moment**, so which of the two
        // writes happens first cannot matter. Everything the target is asking
        // for is recorded as having arrived at `at`, and the view is stamped
        // `at`, so a view covers what was already there — including an alert
        // the host has never evaluated, which a session that failed with no
        // surface listening would otherwise keep for good.
        let at = chrono::Local::now();
        {
            let mut state = self.attention().state.lock().unwrap();
            // Everything raised up to here is observed first, and the look is
            // placed after it. An alert that arrives later gets a higher
            // position, so it cannot read as covered by this look however
            // close in time the two are.
            let _ = self.current(target, &mut state);
            // The stored look and the in-memory one are the same look, so they
            // are written together: a database failure that left the in-memory
            // one behind would hide an alert now and let it return after a
            // restart, since nothing on disk would say it had been seen.
            // Written while the lock is held, so no evaluation can fall
            // between the two.
            if let Err(err) = self.attention().prefs.mark_viewed(target, at) {
                tracing::warn!(%err, target, "could not record that a session was viewed");
                return;
            }
            let looked = state.tick();
            state.viewed_at.insert(target.to_string(), looked);
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
        // Derive, record and broadcast as one step. Apart, a derivation that
        // read the world before another thread's `mark_viewed` could be
        // installed after that thread had already retired the alert, and a
        // retirement could be broadcast before the raise it retires.
        let mut state = self.attention().state.lock().unwrap();
        // One read of the session for the attention *and* its edge: taken
        // apart, a health update in between could pair an alert with an
        // identity from another moment — a prompt edge minted for a prompt
        // that had just been answered.
        let (attention, edge) = self.current(target, &mut state);
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
    ) -> (Option<Attention>, Option<AttentionEdge>) {
        self.current_with(target, None, None, state)
    }

    /// As [`HostCore::current`], with one external item supplied by the caller
    /// rather than read from the registry.
    ///
    /// A cockpit row's state is a PTY-parsed `AgentStatus` the caller already
    /// holds. Evaluating only what the producer has published would read
    /// "Working" for a blocked agent until the first publication landed, and
    /// evaluating it anywhere else would be a second evaluator. So it comes in
    /// here, through the same ranking, the same prefs, the same looks and the
    /// same onset recording as everything else.
    fn current_with(
        &self,
        target: &str,
        from_status: Option<StatusView>,
        prefs: Option<&PrefsSnapshot>,
        state: &mut AttentionState,
    ) -> (Option<Attention>, Option<AttentionEdge>) {
        let mut external = external_for(state, target);
        if let Some(status) = from_status {
            // The producer publishes under this same key. If its publication
            // has already landed, the caller's reading is the fresher of the
            // two, not a second alert — and a status that raises nothing is
            // the fresher word too: the request it published has ended, so
            // the published item goes with it rather than outliving the
            // status it came from. Other producers' items on this row stay.
            external.retain(|published| published.key != status.key);
            if let Some(item) = status.item {
                external.push(item);
                // `external_for` returns them worst-first and the evaluator
                // relies on that order for its tie-break.
                external.sort_by(|a, b| {
                    (a.kind.priority(), a.since, &a.key).cmp(&(b.kind.priority(), b.since, &b.key))
                });
            }
        }
        let prefs = match prefs {
            Some(snapshot) => snapshot.of(target),
            None => self.prefs_of(target),
        };
        // **One** read of the session: one handle lookup, and the health, the
        // prompt id and the process that published them taken together. Two
        // lookups let a respawn land in between and brand the old process's
        // prompt with the successor's number — at which point the successor's
        // own first prompt, numbered the same by its provider, is the same
        // alert and is never announced.
        // The incarnation comes from the same lookup as the snapshot. It is
        // fixed for the life of a handle, so unlike the generation it cannot
        // be made stale by reading it here.
        let session = self
            .session_of(target)
            .map(|handle| (handle.incarnation().to_string(), handle.health_snapshot()));
        let request = session.as_ref().and_then(|(born, s)| prompt_edge(s, born));
        let health = session.as_ref().map(|(_, s)| &s.health);
        // Evaluated against the onsets as they stood *before* this call, so an
        // alert is never born already seen: recording first would compare a
        // brand-new alert against this very moment, and a view taken in the
        // same millisecond would swallow it.
        let attention = match health {
            Some(health) => attention::of(target, health, &prefs, &external, &Seen(state, &prefs)),
            // No session of its own (a terminal agent, a run root): its alerts
            // are the ones its producer raised.
            None => attention::of_external(target, &prefs, &external, &Seen(state, &prefs)),
        };
        // Then every alert this target could raise is recorded, not only the
        // one that won. An alert sitting beneath a worse one has arrived as
        // surely as the worse one has, and a view covers what had arrived when
        // it was taken; recording only the winner left the one underneath
        // reading as new after the view, and stuck in "needs you".
        let raised: Vec<AttentionEdge> = self
            .candidates(target, health, &external)
            .iter()
            .map(|candidate| attention::edge_of(target, candidate, health, request.as_deref()))
            .collect();
        state.record_all(target, &raised);
        let edge = attention
            .as_ref()
            .map(|a| attention::edge_of(target, a, health, request.as_deref()));
        (attention, edge)
    }

    /// Every alert `target` could raise right now, ignoring what the person
    /// has seen — the session's own state and each external item.
    fn candidates(
        &self,
        target: &str,
        health: Option<&oximux_agents::health::SessionHealth>,
        external: &[ExternalAttention],
    ) -> Vec<Attention> {
        let unseen = SessionPrefs::default();
        let mut out: Vec<Attention> = external
            .iter()
            .map(|item| Attention {
                kind: item.kind,
                since: item.since,
                reason: Some(item.reason.clone()),
                source: AttentionSource::External(item.key.clone()),
            })
            .collect();
        if let Some(health) = health
            // With default prefs nothing reads as seen, so this is the
            // session's own alert as it stands.
            && let Some(own) = attention::of(target, health, &unseen, &[], &attention::NothingSeen)
        {
            out.push(own);
        }
        out
    }

    /// The live session a target names, if it names one.
    fn session_of(&self, target: &str) -> Option<Arc<SessionHandle>> {
        self.registry().get(oximux_agents::session_prefs::chat_id(target)?)
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
                    for target in core.chats_to_retire(&known, &live) {
                        core.reevaluate(&target);
                    }
                    known = live;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    /// Whether anything is listening to the attention stream.
    ///
    /// The followers exist only to feed it, so with no subscriber their work
    /// is unobservable and they skip it.
    pub(crate) fn feeding_subscribers(&self) -> bool {
        self.attention().tx.receiver_count() > 0
    }

    /// The chat targets to re-evaluate because they are no longer registered.
    ///
    /// Two sources, because the watcher's own view is incomplete. A session can
    /// register, publish an alert through its follower and unregister between
    /// two of the watcher's passes: it was never in `known`, so a difference
    /// against `known` alone would never reconcile it, and a view still
    /// holding its handle keeps its watches open, so its follower never wakes
    /// to retire the alert either. Whatever was published is in `last`, so
    /// that is the second place to look.
    ///
    /// Chats only: an external producer's target has no handle in the
    /// registry, so `live` says nothing about whether it is gone.
    pub(crate) fn chats_to_retire(
        &self,
        known: &HashSet<String>,
        live: &HashSet<String>,
    ) -> Vec<String> {
        let mut out: Vec<String> = known.difference(live).cloned().collect();
        {
            // Dropped before the caller re-evaluates: `reevaluate` takes this
            // same lock, and it is not reentrant.
            let state = self.attention().state.lock().unwrap();
            for target in state.last.keys() {
                if oximux_agents::session_prefs::chat_id(target).is_some()
                    && !live.contains(target)
                    && !known.contains(target)
                {
                    out.push(target.clone());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Start a follower for every live session that has none. A no-op while
    /// nothing subscribes to the stream.
    ///
    /// Followed by **handle**, not by session id. A session can be
    /// unregistered and registered again under the same id while a view still
    /// holds the old handle; keyed by id alone, the new handle would be taken
    /// for one already followed and its changes would never reach the stream.
    fn follow_live_sessions(self: &Arc<Self>) {
        if !self.feeding_subscribers() {
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
            // Nothing is listening. The guard in `follow_live_sessions` only
            // stops followers being *started*; one already running would go on
            // taking the attention lock and reading the preferences database
            // on every health change for nobody. The snapshot readers derive
            // on demand, so skipping this is unobservable.
            if core.feeding_subscribers() {
                core.reevaluate(&target);
            }
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

/// The pending prompt's edge identity: the provider's request id qualified by
/// the handle that published it *and* the process within that handle.
///
/// A provider that numbers its requests from 1 again after a respawn gives the
/// successor's prompt the same id as the one before it. Unqualified, the two
/// would be one edge, and a consumer that alerts once per edge would never
/// announce the new prompt. The raw id still decides the prompt itself — this
/// qualification is for telling alerts apart.
///
/// **The generation alone is not enough.** `register_with_caps` starts a
/// replacement handle's generation at zero again, so a session unregistered
/// and registered under the same id gives its first prompt the same
/// `0:<request>` as the departed handle's first prompt. If the replacement
/// arrives before the watcher has reconciled the removal, that is the *same*
/// edge, and [`HostCore::reevaluate`]'s "nothing to hear about" arm drops the
/// new prompt — a person is never shown a request that is waiting on them.
/// The incarnation is unique per handle, so it separates the two.
///
/// Taken from the snapshot rather than from the handle, so the number is the
/// one the facts were published under and not whatever is current a moment
/// later.
fn prompt_edge(
    session: &oximux_agents::session_health::Snapshot,
    incarnation: &str,
) -> Option<String> {
    let request = session.inputs.prompt_request.as_deref()?;
    Some(format!("{incarnation}:{}:{request}", session.generation))
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
