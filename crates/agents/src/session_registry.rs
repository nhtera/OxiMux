//! Process-wide, gpui-free session registry — the event bus + command surface
//! every remote surface (and eventually the desktop view) hangs off.
//!
//! A [`SessionRegistry`] maps `session_id → `[`SessionHandle`], each holding the
//! shared `Arc<dyn AgentConnection>`, a monotonically-`seq`-indexed replayable
//! backlog, a live broadcast fan-out, an atomic idempotent-resolve gate, and a
//! coarse status snapshot. Every method is callable from a plain tokio task with
//! no `gpui::Context` in scope, so the network layer can subscribe and command a
//! session without going through the view.
//!
//! Three correctness properties are load-bearing (see the phase's red-team notes):
//!
//! 1. **Atomic idempotent resolve.** A permission is decided through exactly one
//!    gate ([`SessionHandle::resolve_permission`]); the desktop's Allow/Deny path
//!    and any remote path route through the same method, so two concurrent callers
//!    for one `request_id` can't both fire `conn.resolve_permission`.
//! 2. **Seq-indexed durable backlog.** `broadcast` can't replay (a lagging receiver
//!    gets `Lagged` and permanently loses events), so a bounded `VecDeque` backlog
//!    backs [`SessionHandle::events_since`] for reconnect gap-fill. The broadcast is
//!    only the live edge.
//! 3. **Off-thread command surface.** `send_prompt` / `steer` / `cancel` /
//!    `resolve_permission` are plain `&self` calls on the shared `Arc`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::channel::mpsc;
use tokio::sync::{broadcast, watch};

use crate::thread::{
    AgentConnection, AskQuestion, ChatImage, ModeChoice, ModelChoice, PermissionDecision,
    QuestionAnswers, ThreadEvent,
};

/// Session identity — the same `session_id` the transcript persists under.
pub type SessionId = String;
/// Monotonic per-session event index. Starts at 1; `events_since(0)` replays all
/// still-retained events.
pub type Seq = u64;

/// Coarse per-session snapshot for list views (the phone's session list, the
/// desktop rail). Cheap to read via a `watch` channel; updated as events flow.
/// Intentionally minimal for now — richer turn-state derivation lands with the
/// desktop-view wiring, which knows the exact turn-boundary events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionStatus {
    /// The `seq` of the last event ingested, so a list row can show freshness and
    /// a reconnecting client knows where to resume from.
    pub last_seq: Seq,
    /// At least one permission request is outstanding (awaiting Allow/Deny).
    pub awaiting_permission: bool,
}

/// How a session presents itself in a remote client's session list. Lives here
/// (not in [`SessionStatus`]) because it is pushed by the desktop view rather than
/// derived from the event stream, and it must not be clobbered by a status refresh
/// on every ingest. Both fields are `None` until the view first publishes them —
/// a freshly-registered session has no title until one is generated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMeta {
    /// The session's display title (from the fold's `TitleUpdated`).
    pub title: Option<String>,
    /// The effective model id driving the session.
    pub model: Option<String>,
    /// The session's working directory. Git RPCs resolve their repository from
    /// this, which scopes remote git access to the sessions a device may already
    /// reach — a session-scoped device cannot browse another project's repo.
    pub cwd: Option<PathBuf>,
}

/// A folded-transcript snapshot the desktop view publishes for remote clients.
///
/// Stored per session so a client opening it gets the full history plus a resume
/// cursor — critically, a transcript restored from disk after a host restart lives
/// only in the view's fold and never enters the event ring, so the bounded backlog
/// alone cannot supply it. Opaque to the registry: `entries_json` is the folded
/// `Vec<ThreadEntry>` as JSON, so the registry stays free of `agent-core`'s entry
/// taxonomy (the same reason the wire carries it as a string).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptSnapshot {
    /// The fold cursor the entries reflect — the registry's last-assigned `seq` at
    /// publish time (0 before any event, e.g. a freshly restored transcript). A
    /// subscriber resumes the live stream strictly after this.
    pub seq: Seq,
    /// The folded `Vec<ThreadEntry>` serialized as JSON.
    pub entries_json: String,
    /// The effective model, mirrored so a rehydrating client seeds its fold with it.
    pub model: Option<String>,
}

/// A prompt that arrived over remote control (a phone send), relayed to the bound
/// desktop view so it can show the user's own bubble. The host already forwarded
/// the prompt to the backend and ingested a synthetic copy for other subscribers;
/// the view needs this only to render the bubble locally (no backend echoes the
/// user's own message, and a desktop-typed prompt bubbles optimistically in the
/// composer — a remote one has no such local surface without this relay).
#[derive(Clone, Debug)]
pub struct RemotePrompt {
    pub text: String,
    pub images: Vec<ChatImage>,
}

/// Everything the registry knows about one live session. Held behind an `Arc` so
/// the view and the network layer share one handle.
pub struct SessionHandle {
    /// Shared connection — the view borrows the same `Arc`, so commands issued
    /// remotely and locally hit one transport. Swappable ([`swap_connection`]) so a
    /// desktop respawn can re-point the session at its new backend **without**
    /// re-registering it, which would reset `seq` and strand every subscriber.
    ///
    /// [`swap_connection`]: SessionHandle::swap_connection
    conn: Mutex<Arc<dyn AgentConnection>>,
    /// Next `seq` to assign; `fetch_add` makes ingestion lock-free on the counter.
    next_seq: AtomicU64,
    /// Bounded replay store for gap-fill. Oldest entries drop past `backlog_cap`.
    backlog: Mutex<VecDeque<(Seq, ThreadEvent)>>,
    backlog_cap: usize,
    /// Live fan-out for remote subscribers. Bounded; a slow subscriber that lags
    /// recovers via [`Self::events_since`], not by growing this ring.
    live: broadcast::Sender<(Seq, ThreadEvent)>,
    /// Request-ids that have been decided. Insertion is the atomic gate: the
    /// caller whose `insert` returns `true` is the one that fires the transport.
    decided: Mutex<HashSet<String>>,
    /// Request-ids currently awaiting a decision (drives `awaiting_permission`).
    pending: Mutex<HashSet<String>>,
    status_tx: watch::Sender<SessionStatus>,
    /// Display metadata published by the desktop view (title/model).
    meta: Mutex<SessionMeta>,
    /// The latest folded-transcript snapshot the desktop view published, served to a
    /// remote client on `FetchTranscript`. `None` until the view first publishes —
    /// a client then opens with an empty base and the live stream fills it.
    transcript: Mutex<Option<TranscriptSnapshot>>,
    /// Relays a remotely-injected prompt back to the bound desktop view so it shows
    /// the user's own bubble. `None` when no view is bound (remote disabled, or a
    /// headless host); the prompt still reaches the backend and other subscribers.
    remote_prompt_tx: Mutex<Option<mpsc::UnboundedSender<RemotePrompt>>>,
    /// The registry-wide session-list generation, shared with the registry and
    /// every sibling handle. Bumped when this session's list-visible state changes
    /// (title/model or the awaiting-permission flag) so a session-list subscriber
    /// re-snapshots. Coalescing, so a burst wakes the subscriber once.
    changed: watch::Sender<u64>,
}

impl SessionHandle {
    /// Assign the next `seq`, append to the backlog (dropping the oldest past the
    /// cap), fan out to live subscribers, and refresh the status snapshot. Returns
    /// the assigned `seq`. Broadcast-send errors (no live receivers) are ignored —
    /// the backlog is the durable store.
    ///
    /// Seq-assignment, backlog-append, and broadcast all happen **under the one
    /// backlog lock**, so the `seq` order, the retained order, and the live-fan-out
    /// order are identical even if more than one producer ever tees into the same
    /// session (Phase 3 adds a remote producer alongside the desktop drain). Without
    /// that, a producer could bump the counter, stall, and let a later producer
    /// append ahead of it — reordering the backlog `events_since` promises is
    /// ascending.
    pub fn ingest(&self, event: ThreadEvent) -> Seq {
        // Track outstanding permission requests for the coarse status snapshot.
        if let ThreadEvent::PermissionRequested { request_id, .. } = &event {
            self.pending.lock().unwrap().insert(request_id.clone());
        }

        let seq = {
            let mut backlog = self.backlog.lock().unwrap();
            let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
            backlog.push_back((seq, event.clone()));
            while backlog.len() > self.backlog_cap {
                backlog.pop_front();
            }
            // Broadcast inside the lock so live order == backlog order.
            let _ = self.live.send((seq, event));
            seq
        };

        self.update_status(Some(seq));
        seq
    }

    /// Subscribe to the live edge. Pair with [`Self::events_since`] on (re)subscribe
    /// to close the gap between the last-seen `seq` and the first live event.
    pub fn subscribe(&self) -> broadcast::Receiver<(Seq, ThreadEvent)> {
        self.live.subscribe()
    }

    /// Replay retained events strictly after `after_seq` (ascending). Anything that
    /// aged out of the bounded backlog is not returned — a caller lagging past the
    /// cap must resync from a snapshot, not from here.
    pub fn events_since(&self, after_seq: Seq) -> Vec<(Seq, ThreadEvent)> {
        self.backlog
            .lock()
            .unwrap()
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .cloned()
            .collect()
    }

    /// The single atomic gate for deciding a permission. Returns `Ok(true)` for the
    /// caller that actually fired the transport, `Ok(false)` for any later/racing
    /// caller whose decision was already made. On transport error the gate is rolled
    /// back so a genuine retry can proceed.
    pub fn resolve_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<bool> {
        let newly_decided = self.decided.lock().unwrap().insert(request_id.to_string());
        if !newly_decided {
            return Ok(false);
        }
        if let Err(err) = self.conn().resolve_permission(request_id, decision) {
            // Undo the gate so the request isn't permanently locked as "decided"
            // by a transient transport failure.
            self.decided.lock().unwrap().remove(request_id);
            return Err(err);
        }
        self.pending.lock().unwrap().remove(request_id);
        // Resolving isn't an ingest — it must NOT advance `last_seq` (deriving it
        // from `next_seq` here can read a counter already bumped by a concurrent
        // ingest whose event isn't in the backlog yet, publishing a resume cursor
        // ahead of the durable store). Refresh only the awaiting flag.
        self.update_status(None);
        Ok(true)
    }

    /// Answer an outstanding `AskUserQuestion`, sharing the permission gate so a
    /// question and a permission can never both claim the same `request_id` and
    /// so a racing second answer is refused rather than double-sent.
    ///
    /// `questions` comes from the caller because the backend payload keys answers
    /// by question text, and that text lives in the `ChatThread` this registry
    /// deliberately does not hold.
    ///
    /// Note this does **not** carry the desktop's secret-answer redaction: that
    /// sets `redact_result` on the thread, which is out of reach here. Callers
    /// exposing this to a remote surface must keep `is_secret` questions off it.
    pub fn answer_question(
        &self,
        request_id: &str,
        questions: &[AskQuestion],
        answers: &QuestionAnswers,
    ) -> Result<bool> {
        let newly_decided = self.decided.lock().unwrap().insert(request_id.to_string());
        if !newly_decided {
            return Ok(false);
        }
        if let Err(err) = self.conn().answer_question(request_id, questions, answers) {
            // Roll the gate back so a transient transport failure doesn't lock the
            // question as answered forever.
            self.decided.lock().unwrap().remove(request_id);
            return Err(err);
        }
        self.pending.lock().unwrap().remove(request_id);
        self.update_status(None);
        Ok(true)
    }

    /// Send a user prompt, starting a new turn. `corr_id` is reserved for the
    /// optimistic-echo correlation the view/phone use to dedup the arriving stream
    /// copy against their local echo (threaded through once the surfaces echo).
    pub fn send_prompt(&self, text: &str, images: &[ChatImage]) -> Result<()> {
        self.conn().send_user_message_with_images(text, images)?;
        // No backend echoes the user's own message back, so without this the
        // remote transcript would show replies to prompts it never displayed.
        // Ingested only after the send succeeds — a failed send must not leave a
        // bubble implying the agent received something it never did.
        self.ingest(ThreadEvent::UserMessage {
            text: text.to_string(),
            images: images.to_vec(),
        });
        // Relay to the bound desktop view so it renders the user's own bubble too —
        // the synthetic ingest above only reaches remote subscribers, and no backend
        // echoes the prompt, so without this the desktop shows the reply to a prompt
        // it never displayed. Best-effort: a dropped receiver (no view) is fine.
        if let Some(tx) = self.remote_prompt_tx.lock().unwrap().as_ref() {
            let _ = tx.unbounded_send(RemotePrompt {
                text: text.to_string(),
                images: images.to_vec(),
            });
        }
        Ok(())
    }

    /// Redirect the agent mid-turn (backends that support it); no-op default
    /// otherwise. Same transport as the desktop composer's steer.
    pub fn steer(&self, text: &str) -> Result<()> {
        self.conn().steer(text)
    }

    /// Interrupt the current turn.
    pub fn cancel(&self) -> Result<()> {
        self.conn().cancel()
    }

    /// The current connection, cloned out so no lock is held across a backend call
    /// (those can block, and a swap must never wait behind one).
    fn conn(&self) -> Arc<dyn AgentConnection> {
        self.conn.lock().unwrap().clone()
    }

    /// Re-point this session at a new backend, keeping its `seq`, backlog, live
    /// subscribers, and permission gates intact.
    ///
    /// The desktop respawns a session on a model/effort/permission change or
    /// `/clear`. Re-registering for that would mint a fresh handle whose `seq`
    /// restarts at 1 — and a subscriber sitting at a higher cursor treats every
    /// such frame as an already-seen duplicate and silently drops it, with no gap
    /// to trigger a resync. Swapping keeps the stream monotonic instead.
    pub fn swap_connection(&self, conn: Arc<dyn AgentConnection>) {
        *self.conn.lock().unwrap() = conn;
    }

    /// The models this session's backend offers, for a remote picker.
    ///
    /// Read straight off the live connection rather than from any cache: for a
    /// bound session `models()` is authoritative, and the desktop's own
    /// `CatalogCache` is a `gpui::Global` in the `app` crate that this crate is
    /// deliberately unable to reach.
    ///
    /// An empty list is a legitimate answer — a dynamic-catalog backend reports
    /// nothing until its handshake completes — and means "no choice to offer",
    /// not a failure.
    pub fn models(&self) -> Vec<ModelChoice> {
        self.conn().models()
    }

    /// The permission modes this session's backend offers. Empty is legitimate,
    /// as for [`Self::models`].
    pub fn permission_modes(&self) -> Vec<ModeChoice> {
        self.conn().permission_modes()
    }

    /// Switch the backend's model in place.
    ///
    /// Mirrors what the desktop tries first. **Some backends fix the model at
    /// spawn** and answer with an error here; the desktop recovers by respawning
    /// the child, which is a view-level operation this crate cannot perform (it
    /// owns no process spawning, by design). So the error propagates to the
    /// caller instead of being swallowed — a remote picker that silently did
    /// nothing would be worse than one that says it could not.
    pub fn set_model(&self, model: &str) -> anyhow::Result<()> {
        self.conn().set_model(model)
    }

    /// Switch the backend's permission mode in place. Same fix-at-spawn caveat
    /// as [`Self::set_model`].
    pub fn set_permission_mode(&self, mode: &str) -> anyhow::Result<()> {
        self.conn().set_mode(mode)
    }

    /// A `watch` receiver over the coarse status snapshot for list views.
    pub fn status(&self) -> watch::Receiver<SessionStatus> {
        self.status_tx.subscribe()
    }

    /// The current status snapshot, cloned — for a one-shot read (e.g. building a
    /// remote session-list row) without holding a `watch` receiver.
    pub fn status_snapshot(&self) -> SessionStatus {
        self.status_tx.borrow().clone()
    }

    /// The session's current display metadata (title/model).
    pub fn meta_snapshot(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }

    /// The transcript snapshot the desktop view last published, if any.
    pub fn transcript_snapshot(&self) -> Option<TranscriptSnapshot> {
        self.transcript.lock().unwrap().clone()
    }

    /// Register the sink that relays remotely-injected prompts to the desktop view.
    /// Replaces any prior sink (a rebind re-points it); the old receiver then ends.
    pub fn set_remote_prompt_sink(&self, tx: mpsc::UnboundedSender<RemotePrompt>) {
        *self.remote_prompt_tx.lock().unwrap() = Some(tx);
    }

    /// Publish the folded transcript for remote clients. Pairs the entries with the
    /// registry's current last-assigned `seq` (0 before any event) so a subscriber
    /// resumes the live stream from exactly the point the snapshot already covers —
    /// no gap, no duplicate. Cheap enough for the desktop view to call whenever a
    /// settled turn lands; it is not on the per-token delta path.
    pub fn publish_transcript(&self, entries_json: String, model: Option<String>) {
        let seq = self.next_seq.load(Ordering::SeqCst).saturating_sub(1);
        *self.transcript.lock().unwrap() = Some(TranscriptSnapshot { seq, entries_json, model });
    }

    /// Publish display metadata from the desktop view. Returns whether anything
    /// changed, so a caller on a hot path (the per-event tee) can skip follow-up
    /// work when the title and model are unchanged — which is the common case.
    pub fn set_meta(&self, meta: SessionMeta) -> bool {
        let mut current = self.meta.lock().unwrap();
        if *current == meta {
            return false;
        }
        *current = meta;
        drop(current);
        // A title/model change is visible in the session-list row.
        self.bump_changed();
        true
    }

    /// Nudge the registry-wide session-list generation so a subscriber re-snapshots.
    fn bump_changed(&self) {
        self.changed.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Refresh the coarse status snapshot. `ingested_seq` is `Some` only from the
    /// ingest path (which just appended that seq to the backlog); `last_seq` is
    /// advanced monotonically via `max` so a late status update from a slower
    /// producer can't regress it below an already-retained event. Command paths
    /// (resolve) pass `None` — they refresh only `awaiting_permission`, never the
    /// resume cursor.
    fn update_status(&self, ingested_seq: Option<Seq>) {
        let awaiting = !self.pending.lock().unwrap().is_empty();
        let mut awaiting_flipped = false;
        self.status_tx.send_modify(|status| {
            if let Some(seq) = ingested_seq {
                status.last_seq = status.last_seq.max(seq);
            }
            awaiting_flipped = status.awaiting_permission != awaiting;
            status.awaiting_permission = awaiting;
        });
        // Only the awaiting flag shows in a session-list row; a pure `last_seq` tick
        // (every event/token) would re-push an identical list, so bump only on a
        // flip — the coalescing generation keeps even that cheap.
        if awaiting_flipped {
            self.bump_changed();
        }
    }
}

/// Default bounded-backlog depth per session. Enough to gap-fill a phone that
/// backgrounds for a moment; a client that lags past it resyncs from a snapshot.
pub const DEFAULT_BACKLOG_CAP: usize = 1024;
/// Default live-broadcast ring depth. Only the remote path rides this; the desktop
/// view gets its own dedicated non-lossy channel (added with the view wiring), so a
/// small ring here can never starve the desktop UI.
pub const DEFAULT_BROADCAST_CAP: usize = 256;

/// The process-wide map of live sessions. Cloneable handles are h-shared out;
/// the registry itself is held behind an `Arc` (and, in the app, a `gpui::Global`).
pub struct SessionRegistry {
    sessions: Mutex<HashMap<SessionId, Arc<SessionHandle>>>,
    /// Coalescing generation, bumped whenever the session set or any session's
    /// list-visible state changes. A session-list subscriber awaits this and
    /// re-snapshots, so the phone is pushed the list rather than polling it.
    changed: watch::Sender<u64>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        let (changed, _) = watch::channel(0);
        Self { sessions: Mutex::new(HashMap::new()), changed }
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A receiver that ticks whenever the live session list changes — a session
    /// opened, closed, renamed, remodeled, or its permission flag flipped. The host
    /// awaits this and re-snapshots the list for each subscriber; the counter
    /// coalesces so a burst of changes wakes it once.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Register a session and return its handle (also retained in the map). Call on
    /// connect; pair with [`Self::unregister`] on close.
    pub fn register(&self, id: SessionId, conn: Arc<dyn AgentConnection>) -> Arc<SessionHandle> {
        self.register_with_caps(id, conn, DEFAULT_BACKLOG_CAP, DEFAULT_BROADCAST_CAP)
    }

    /// Register with explicit ring sizes (tests use small caps to exercise eviction
    /// and lag).
    pub fn register_with_caps(
        &self,
        id: SessionId,
        conn: Arc<dyn AgentConnection>,
        backlog_cap: usize,
        broadcast_cap: usize,
    ) -> Arc<SessionHandle> {
        // Re-registering a live id is a respawn, not a new session: swap the
        // backend in place so `seq`, the backlog, and live subscribers survive.
        // Minting a fresh handle here would reset `seq` to 1 and strand every
        // subscriber (their cursor is already higher, so the frames read as
        // duplicates and are dropped without any gap to resync from).
        if let Some(existing) = self.sessions.lock().unwrap().get(&id) {
            existing.swap_connection(conn);
            return existing.clone();
        }
        let (live, _) = broadcast::channel(broadcast_cap.max(1));
        let (status_tx, _) = watch::channel(SessionStatus::default());
        let handle = Arc::new(SessionHandle {
            conn: Mutex::new(conn),
            next_seq: AtomicU64::new(1),
            backlog: Mutex::new(VecDeque::new()),
            backlog_cap: backlog_cap.max(1),
            live,
            decided: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashSet::new()),
            status_tx,
            meta: Mutex::new(SessionMeta::default()),
            transcript: Mutex::new(None),
            remote_prompt_tx: Mutex::new(None),
            changed: self.changed.clone(),
        });
        self.sessions.lock().unwrap().insert(id, handle.clone());
        // A newly registered session changes the list.
        self.changed.send_modify(|g| *g = g.wrapping_add(1));
        handle
    }

    /// Remove a session from the map, returning its handle if present. Any live
    /// subscribers see the broadcast close when the last handle is dropped.
    pub fn unregister(&self, id: &str) -> Option<Arc<SessionHandle>> {
        let removed = self.sessions.lock().unwrap().remove(id);
        if removed.is_some() {
            // A closed session changes the list.
            self.changed.send_modify(|g| *g = g.wrapping_add(1));
        }
        removed
    }

    /// Look up a live session handle.
    pub fn get(&self, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Number of live sessions.
    pub fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.lock().unwrap().is_empty()
    }

    /// A snapshot of every live session's id + coarse status, for a remote
    /// session-list bootstrap. Ascending by nothing in particular — the caller
    /// orders as it likes.
    pub fn statuses(&self) -> Vec<(SessionId, SessionStatus)> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, h)| (id.clone(), h.status_snapshot()))
            .collect()
    }

    // ---- id-keyed convenience pass-throughs (return None/empty for unknown id) ----

    /// Tee a backend event into a session: assign `seq`, store, fan out. Returns the
    /// assigned `seq`, or `None` if the session isn't registered.
    pub fn ingest(&self, id: &str, event: ThreadEvent) -> Option<Seq> {
        self.get(id).map(|h| h.ingest(event))
    }

    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<(Seq, ThreadEvent)>> {
        self.get(id).map(|h| h.subscribe())
    }

    pub fn events_since(&self, id: &str, after_seq: Seq) -> Vec<(Seq, ThreadEvent)> {
        self.get(id).map(|h| h.events_since(after_seq)).unwrap_or_default()
    }

    /// Route a permission decision through the session's atomic gate. `Ok(false)`
    /// means already-decided (or unknown session).
    pub fn resolve_permission(
        &self,
        id: &str,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<bool> {
        match self.get(id) {
            Some(h) => h.resolve_permission(request_id, decision),
            None => Ok(false),
        }
    }

    pub fn send_prompt(&self, id: &str, text: &str, images: &[ChatImage]) -> Result<()> {
        match self.get(id) {
            Some(h) => h.send_prompt(text, images),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A minimal `Send + Sync` connection that records how many times each command
    /// fired, so tests can assert "called exactly once".
    #[derive(Default)]
    struct RecordingConn {
        resolves: AtomicUsize,
        prompts: Mutex<Vec<String>>,
        fail_resolve: bool,
    }

    impl AgentConnection for RecordingConn {
        fn send_user_message(&self, text: &str) -> Result<()> {
            self.prompts.lock().unwrap().push(text.to_string());
            Ok(())
        }
        fn resolve_permission(&self, _request_id: &str, _decision: PermissionDecision) -> Result<()> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            if self.fail_resolve {
                anyhow::bail!("transport down");
            }
            Ok(())
        }
        fn shutdown(&self) {}
    }

    fn allow() -> PermissionDecision {
        PermissionDecision::Allow { updated_input: serde_json::Value::Null }
    }

    /// A remote subscriber learns the user's own prompt ONLY from the synthetic
    /// event `send_prompt` ingests — no backend echoes it back — so without this
    /// the phone would render replies to prompts it never displayed.
    #[test]
    fn send_prompt_ingests_the_user_message_for_subscribers() {
        let reg = SessionRegistry::new();
        let handle = reg.register("s1".into(), Arc::new(RecordingConn::default()));
        let mut rx = reg.subscribe("s1").unwrap();

        handle.send_prompt("hello", &[]).unwrap();

        let (_, ev) = rx.try_recv().expect("the prompt reached subscribers");
        assert!(
            matches!(&ev, ThreadEvent::UserMessage { text, .. } if text == "hello"),
            "saw {ev:?}",
        );
    }

    /// A remotely-injected prompt is relayed to the bound view sink so the desktop
    /// renders the user's own bubble (the synthetic ingest only reaches the phone).
    #[test]
    fn send_prompt_relays_to_the_bound_view_sink() {
        let reg = SessionRegistry::new();
        let handle = reg.register("s1".into(), Arc::new(RecordingConn::default()));
        let (tx, mut rx) = mpsc::unbounded();
        handle.set_remote_prompt_sink(tx);

        handle.send_prompt("hi from phone", &[]).unwrap();

        let prompt = rx.try_recv().expect("relay sent a prompt");
        assert_eq!(prompt.text, "hi from phone");
    }

    /// A send that never reached the agent must not leave a bubble implying it
    /// did — the gate is the transport result, not the attempt.
    #[test]
    fn a_failed_send_ingests_nothing() {
        struct DeadConn;
        impl AgentConnection for DeadConn {
            fn send_user_message(&self, _text: &str) -> Result<()> {
                anyhow::bail!("transport down")
            }
            fn resolve_permission(&self, _id: &str, _d: PermissionDecision) -> Result<()> {
                Ok(())
            }
            fn shutdown(&self) {}
        }

        let reg = SessionRegistry::new();
        let handle = reg.register("s1".into(), Arc::new(DeadConn));
        let mut rx = reg.subscribe("s1").unwrap();

        assert!(handle.send_prompt("hello", &[]).is_err());
        assert!(rx.try_recv().is_err(), "a failed send publishes no bubble");
    }

    /// A non-gpui subscriber observes events and drives commands with no
    /// `gpui::Context` anywhere in scope.
    #[test]
    fn headless_subscriber_observes_events_and_commands() {
        let reg = SessionRegistry::new();
        let conn = Arc::new(RecordingConn::default());
        reg.register("s1".into(), conn.clone());

        let mut rx = reg.subscribe("s1").unwrap();
        let seq = reg.ingest("s1", ThreadEvent::Notice("hi".into())).unwrap();
        assert_eq!(seq, 1);

        let (got_seq, got_ev) = rx.try_recv().unwrap();
        assert_eq!(got_seq, 1);
        assert_eq!(got_ev, ThreadEvent::Notice("hi".into()));

        reg.send_prompt("s1", "do the thing", &[]).unwrap();
        assert_eq!(conn.prompts.lock().unwrap().as_slice(), ["do the thing"]);

        let won = reg.resolve_permission("s1", "req-1", allow()).unwrap();
        assert!(won);
        assert_eq!(conn.resolves.load(Ordering::SeqCst), 1);
    }

    /// Two concurrent callers race `resolve_permission` for one request_id: exactly
    /// one wins, and the transport fires exactly once.
    #[test]
    fn concurrent_resolve_is_atomic_and_idempotent() {
        for _ in 0..200 {
            let reg = Arc::new(SessionRegistry::new());
            let conn = Arc::new(RecordingConn::default());
            reg.register("s1".into(), conn.clone());

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let reg = reg.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        reg.resolve_permission("s1", "req-1", allow()).unwrap()
                    })
                })
                .collect();

            let wins: usize =
                handles.into_iter().map(|h| h.join().unwrap()).filter(|won| *won).count();
            assert_eq!(wins, 1, "exactly one caller wins the gate");
            assert_eq!(conn.resolves.load(Ordering::SeqCst), 1, "transport fired once");
        }
    }

    /// A failed transport rolls the gate back so a later retry can still resolve.
    #[test]
    fn failed_resolve_rolls_back_the_gate() {
        let reg = SessionRegistry::new();
        let conn = Arc::new(RecordingConn { fail_resolve: true, ..Default::default() });
        reg.register("s1".into(), conn.clone());

        assert!(reg.resolve_permission("s1", "req-1", allow()).is_err());
        // Not permanently locked: the gate was rolled back, so it can be retried.
        assert!(reg.resolve_permission("s1", "req-1", allow()).is_err());
        assert_eq!(conn.resolves.load(Ordering::SeqCst), 2, "retry re-hit the transport");
    }

    /// `events_since` replays the retained backlog to a lagged/reconnecting caller.
    #[test]
    fn events_since_replays_the_backlog() {
        let reg = SessionRegistry::new();
        reg.register("s1".into(), Arc::new(RecordingConn::default()));

        for i in 0..5 {
            reg.ingest("s1", ThreadEvent::Notice(format!("e{i}"))).unwrap();
        }

        let replay = reg.events_since("s1", 2);
        let seqs: Vec<Seq> = replay.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![3, 4, 5], "only events after seq 2, in order");
        assert_eq!(reg.events_since("s1", 0).len(), 5, "from 0 replays everything");
        assert!(reg.events_since("s1", 5).is_empty(), "nothing after the last seq");
    }

    /// A bounded backlog evicts the oldest; `events_since` can't return aged-out
    /// events, but never gaps or reorders what it retains.
    #[test]
    fn bounded_backlog_evicts_oldest() {
        let reg = SessionRegistry::new();
        reg.register_with_caps("s1".into(), Arc::new(RecordingConn::default()), 3, 8);

        for i in 0..6 {
            reg.ingest("s1", ThreadEvent::Notice(format!("e{i}"))).unwrap();
        }

        let all = reg.events_since("s1", 0);
        let seqs: Vec<Seq> = all.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![4, 5, 6], "only the last 3 retained, still ordered");
    }

    /// Status snapshot tracks last_seq and toggles awaiting_permission across a
    /// request/resolve cycle.
    #[test]
    fn status_tracks_last_seq_and_awaiting_permission() {
        let reg = SessionRegistry::new();
        reg.register("s1".into(), Arc::new(RecordingConn::default()));
        let status = reg.get("s1").unwrap().status();

        reg.ingest("s1", ThreadEvent::Notice("hi".into()));
        assert_eq!(status.borrow().last_seq, 1);
        assert!(!status.borrow().awaiting_permission);

        reg.ingest(
            "s1",
            ThreadEvent::PermissionRequested {
                request_id: "req-1".into(),
                tool_use_id: None,
                tool_name: "bash".into(),
                input: serde_json::Value::Null,
                description: String::new(),
                suggestions: vec![],
                kind: crate::thread::PermissionKind::Tool,
            },
        );
        assert!(status.borrow().awaiting_permission, "request outstanding");

        reg.resolve_permission("s1", "req-1", allow()).unwrap();
        assert!(!status.borrow().awaiting_permission, "cleared after resolve");
    }

    /// Resolving a permission must NOT advance `last_seq` — it isn't ingesting, so
    /// it has no authority over the resume cursor. (Regression guard: the old code
    /// derived last_seq from `next_seq`, which could publish a cursor ahead of the
    /// durable backlog.)
    #[test]
    fn resolve_does_not_advance_last_seq() {
        let reg = SessionRegistry::new();
        reg.register("s1".into(), Arc::new(RecordingConn::default()));
        let status = reg.get("s1").unwrap().status();

        reg.ingest(
            "s1",
            ThreadEvent::PermissionRequested {
                request_id: "req-1".into(),
                tool_use_id: None,
                tool_name: "bash".into(),
                input: serde_json::Value::Null,
                description: String::new(),
                suggestions: vec![],
                kind: crate::thread::PermissionKind::Tool,
            },
        );
        let cursor_after_ingest = status.borrow().last_seq;

        reg.resolve_permission("s1", "req-1", allow()).unwrap();
        assert_eq!(
            status.borrow().last_seq,
            cursor_after_ingest,
            "resolve leaves the resume cursor put — never ahead of the backlog"
        );
    }

    /// Many producers tee into one session concurrently: seqs come out 1..=N with
    /// no gaps, dupes, or reordering, because assignment + append happen under the
    /// one backlog lock. (Regression guard for the old fetch_add-before-lock path,
    /// which could interleave a bumped counter with a later producer's append.)
    #[test]
    fn concurrent_ingest_keeps_backlog_ascending_and_gapless() {
        let reg = Arc::new(SessionRegistry::new());
        let n_threads = 8usize;
        let per = 200usize;
        let total = (n_threads * per) as u64;
        reg.register_with_caps("s1".into(), Arc::new(RecordingConn::default()), total as usize, 8);

        let barrier = Arc::new(std::sync::Barrier::new(n_threads));
        let handles: Vec<_> = (0..n_threads)
            .map(|t| {
                let reg = reg.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..per {
                        reg.ingest("s1", ThreadEvent::Notice(format!("t{t}-{i}")));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let seqs: Vec<Seq> = reg.events_since("s1", 0).iter().map(|(s, _)| *s).collect();
        let expected: Vec<Seq> = (1..=total).collect();
        assert_eq!(seqs, expected, "seqs are 1..=N, ascending, gapless, no dupes");
    }

    /// A respawn re-registers the same id. `seq` must keep climbing and live
    /// subscribers must survive — otherwise a phone sitting at a higher cursor
    /// silently discards everything after the respawn as a duplicate, with no gap
    /// to trigger a resync.
    #[test]
    fn re_registering_a_live_id_swaps_the_backend_and_keeps_the_stream() {
        let reg = SessionRegistry::new();
        let first = reg.register("s1".into(), Arc::new(RecordingConn::default()));
        first.ingest(ThreadEvent::AssistantText("before".into()));
        first.ingest(ThreadEvent::AssistantText("respawn imminent".into()));
        assert_eq!(first.status_snapshot().last_seq, 2);

        let mut live = reg.subscribe("s1").expect("subscribed before the respawn");

        // The respawn: same id, brand-new backend.
        let replacement = Arc::new(RecordingConn::default());
        let after = reg.register("s1".into(), replacement.clone());

        assert_eq!(after.status_snapshot().last_seq, 2, "seq is not reset by a respawn");
        after.ingest(ThreadEvent::AssistantText("after".into()));
        assert_eq!(after.status_snapshot().last_seq, 3, "seq keeps climbing across the respawn");

        // The subscriber was created BEFORE the respawn and still receives after
        // it. (`subscribe` is the live edge only — it never replays, so the first
        // frame here is the post-respawn one, at a seq above the pre-respawn
        // cursor. That ordering is the whole point: a client would have discarded
        // it as a duplicate had the respawn reset seq to 1.)
        let (seq, ev) = live.try_recv().expect("subscription survived the respawn");
        assert_eq!(seq, 3, "the post-respawn event continues the sequence");
        assert_eq!(ev, ThreadEvent::AssistantText("after".into()));

        // The backlog spans the respawn, so a reconnecting client can resume.
        assert_eq!(reg.events_since("s1", 0).len(), 3, "backlog is continuous");

        // Commands now reach the NEW backend.
        after.send_prompt("go", &[]).expect("send");
        assert_eq!(replacement.prompts.lock().unwrap().as_slice(), ["go"], "swapped backend used");
    }

    /// Meta starts empty, round-trips, and reports whether it actually changed —
    /// the per-event tee republishes on every batch and relies on that to no-op.
    #[test]
    fn session_meta_round_trips_and_reports_change() {
        let reg = SessionRegistry::new();
        let handle = reg.register("s1".into(), Arc::new(RecordingConn::default()));
        assert_eq!(handle.meta_snapshot(), SessionMeta::default(), "untitled until published");

        let meta =
            SessionMeta { title: Some("Fix auth".into()), model: Some("opus".into()), cwd: None };
        assert!(handle.set_meta(meta.clone()), "first publish is a change");
        assert_eq!(handle.meta_snapshot(), meta);

        assert!(!handle.set_meta(meta), "republishing the same meta is not a change");
        assert!(
            handle.set_meta(SessionMeta {
                title: Some("Fix auth".into()),
                model: None,
                cwd: None
            }),
            "dropping the model is a change",
        );
    }
}
