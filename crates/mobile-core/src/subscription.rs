//! Live subscription: fold a session's `HostEvent` stream and push the folded
//! transcript to the foreign [`ThreadSink`]. The demux pump feeds one
//! connection-wide event stream; [`run_dispatcher`] routes each frame to the
//! matching registered session and heals seq gaps via `events_since`.
//!
//! **Pushes are batched, not per-event.** A snapshot costs one serialization of
//! the whole transcript, so pushing per streaming text delta would re-serialize
//! the entire thread per token. The dispatcher instead drains every frame already
//! queued, folds them all, and pushes once — so a burst coalesces into a single
//! push while a quiet stream still pushes immediately. That needs no timer and
//! adds no latency: batching only happens when frames are genuinely waiting.
//!
//! Known limitations (deferred robustness, acceptable for this slice):
//! - **Decode-`Err` wedge:** if a frame's `ThreadEvent` fails to decode (only
//!   reachable under phone/desktop version skew — both build from one workspace
//!   today), the fold cursor can't advance past it, so the session's stream
//!   silently stalls and re-fetches the same frame. A robust fix needs the fold
//!   to skip-advance + surface an unrecoverable signal.
//! - **Subscribe race:** a live frame arriving between the backlog RPC returning
//!   and the `Sub` being registered is dropped; the seq-gap logic recovers all
//!   but a literal end-of-turn last event. A seed-buffer would close the window.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};

use futures::StreamExt;
use futures::channel::oneshot;
use oximux_agent_core::thread::ThreadEntry;
use oximux_remote_session::{EventStream, FoldOutcome, RemoteSession, SessionSubscription};

use crate::callbacks::ThreadSink;
use crate::client::{MobileClient, Shared};
use crate::ffi_types::MobileError;
use crate::runtime::rt;
use crate::snapshot::ThreadSnapshot;

/// The first-connection outcome channel `connect_with` awaits: `Ok` once the
/// session is live and wired up, `Err` if the host is unreachable before it ever
/// connects. Held behind a mutex so the one-shot fires exactly once.
pub(crate) type FirstResult = Arc<StdMutex<Option<oneshot::Sender<Result<(), MobileError>>>>>;

/// A registered subscription: the fold cursor + the foreign sink to push into.
pub(crate) struct Sub {
    pub subscription: SessionSubscription,
    pub sink: Arc<dyn ThreadSink>,
}

/// One pending push: the foreign sink and the snapshot to hand it.
type Push = (Arc<dyn ThreadSink>, ThreadSnapshot);

/// Build a subscription's snapshot, ready to push *after* the subs lock is
/// released. A thread that will not serialize yields `None` and is skipped
/// rather than tearing down the stream — see [`ThreadSnapshot::build`].
fn snapshot_of(sub: &Sub) -> Option<Push> {
    let snapshot = ThreadSnapshot::build(
        sub.subscription.session_id(),
        sub.subscription.last_seq(),
        sub.subscription.thread(),
    )?;
    Some((sub.sink.clone(), snapshot))
}

/// Hand each snapshot to its sink.
///
/// Deliberately called with **no lock held**: `on_thread` crosses into the
/// foreign runtime (JS), so its duration is not ours to bound — a slow render
/// would otherwise hold the subs lock and stall every concurrent RPC handler
/// that needs it.
fn deliver(pushes: Vec<Push>) {
    for (sink, snapshot) in pushes {
        sink.on_thread(snapshot);
    }
}

/// The connection-wide event loop: fold each frame into its session's cursor,
/// then push the sessions it touched; on a seq gap, backfill from the host and
/// push the recovered state. Ends when the pump closes the stream (link lost /
/// disconnect).
pub(crate) async fn run_dispatcher(shared: Arc<Shared>, mut events: EventStream) {
    while let Some(frame) = events.next().await {
        // Fold this frame plus every frame already queued behind it, so a burst
        // of deltas costs one snapshot rather than one per delta. `try_next`
        // never waits, so a quiet stream takes this loop exactly once.
        let mut batch = vec![frame];
        while let Ok(more) = events.try_recv() {
            batch.push(more);
        }

        let mut gaps = Vec::new();
        let pushes = {
            let mut subs = shared.subs.lock().await;
            // A set, not a list: a batch usually carries several frames for the
            // same session, and each session is pushed once at the end.
            let mut touched: HashSet<String> = HashSet::new();
            // Sessions that have gapped in this batch. Once a session gaps, its
            // remaining frames are skipped — they would each gap in turn, and the
            // backfill replays the whole span in order anyway. Tracked per
            // session rather than by breaking out of the batch, so one session's
            // gap does not discard another session's frames: those would fold
            // fine, and dropping them would stall that session until its next
            // frame happened to trigger a gap of its own.
            let mut gapped: HashSet<&str> = HashSet::new();
            for frame in &batch {
                if gapped.contains(frame.session_id.as_str()) {
                    continue;
                }
                let Some(sub) = subs.get_mut(&frame.session_id) else { continue };
                match sub.subscription.apply(frame) {
                    Ok(FoldOutcome::Applied { .. }) => {
                        touched.insert(frame.session_id.clone());
                    }
                    Ok(FoldOutcome::Gap { resume_from }) => {
                        gaps.push((frame.session_id.clone(), resume_from));
                        gapped.insert(frame.session_id.as_str());
                    }
                    // A decode failure — and ONLY that. A duplicate folds as
                    // `Applied` (at the unchanged cursor), so this arm is not the
                    // dedup path it might look like. Skipping leaves the cursor
                    // put, which is the wedge documented at the top of this file.
                    Err(_) => {}
                }
            }
            touched.iter().filter_map(|id| subs.get(id)).filter_map(snapshot_of).collect()
        };
        deliver(pushes);

        // Gaps: fetch the missed span WITHOUT holding the subs lock across the RPC.
        for (session_id, resume_from) in gaps {
            backfill_gap(&shared, &session_id, resume_from).await;
        }
    }
}

/// Re-fetch `events_since(resume_from)`, fold the recovered frames, and push the
/// healed transcript once.
async fn backfill_gap(shared: &Arc<Shared>, session_id: &str, resume_from: u64) {
    let Ok(session) = shared.session() else { return };
    let Ok(backlog) = session.events_since(session_id, resume_from).await else { return };
    let pending = {
        let mut subs = shared.subs.lock().await;
        let Some(sub) = subs.get_mut(session_id) else { return };
        let mut applied = false;
        for ev in &backlog {
            if matches!(sub.subscription.apply(ev), Ok(FoldOutcome::Applied { .. })) {
                applied = true;
            }
        }
        applied.then(|| snapshot_of(sub)).flatten()
    };
    deliver(pending.into_iter().collect());
}

/// Wire a freshly-(re)connected session into the shared state: restore any
/// carried-over subscriptions, publish it as the live session, drain its event
/// stream through the dispatcher, and signal the first-connection outcome to the
/// waiting `connect_with`. Runs on the core runtime (spawned by the driver's
/// `on_connected`) so the driver keeps polling the pump that services the
/// re-subscribe RPCs below.
pub(crate) async fn activate(
    shared: Arc<Shared>,
    session: Arc<RemoteSession>,
    first: FirstResult,
    epoch: u64,
) {
    let events = session.take_events().expect("events taken once per session");
    // Taken here, beside the event stream, because both are per-session: a new
    // session after a reconnect brings a fresh pair. Draining it is not optional
    // — it is an unbounded channel the pump keeps filling, so leaving it untaken
    // would grow without limit for any client the host pushes terminal frames to.
    let terminal_pushes = session.take_terminals().expect("terminals taken once per session");
    // Also per-session: a fresh (re)connect brings a new push stream, and the host
    // rebuilds its subscriber set on accept, so this must be taken + re-subscribed
    // every activation, like the terminal stream above.
    let session_pushes = session.take_sessions().expect("sessions taken once per session");
    resubscribe_all(&shared, &session).await;
    // Subscribe to the live session list on this connection; the RPC returns the
    // initial snapshot, delivered to the sink inside.
    crate::sessions::resubscribe_sessions(&shared, &session).await;
    {
        // Publish the session — but only if a newer (re)connect or a `disconnect`
        // hasn't superseded us while we were wiring up. Checked under the same lock
        // that guards the write so the decision and the store are atomic.
        let mut live = shared.session.lock().unwrap();
        if shared.epoch.load(Ordering::Acquire) != epoch {
            return; // superseded — drop this session instead of resurrecting it
        }
        *live = Some(session);
    }
    rt().spawn(run_dispatcher(shared.clone(), events));
    rt().spawn(crate::terminals::run_terminal_pump(shared.clone(), terminal_pushes));
    rt().spawn(crate::sessions::run_sessions_pump(shared.clone(), session_pushes));
    // After the session is published and the pump is running: the app answers this
    // by re-attaching, which needs a live session to issue the RPC against and a
    // running pump for the resulting frames to arrive through.
    crate::terminals::resync_attached(&shared);
    if let Some(tx) = first.lock().unwrap().take() {
        let _ = tx.send(Ok(()));
    }
}

/// Re-establish every registered subscription against a new session after a
/// reconnect: resume each from its fold cursor (`last_seq`) so only events missed
/// during the drop re-seed the sink — a reconnect never re-floods the app with the
/// whole transcript. The shared per-session cursor also makes this idempotent, so
/// two overlapping activations (a reconnect flap) forward each frame at most once.
/// A no-op on the first connect (no subs yet); a session whose re-subscribe RPC
/// fails is skipped — the dispatcher's gap logic recovers it once live frames flow.
async fn resubscribe_all(shared: &Arc<Shared>, session: &Arc<RemoteSession>) {
    let ids: Vec<String> = shared.subs.lock().await.keys().cloned().collect();
    for id in ids {
        let after = match shared.subs.lock().await.get(&id) {
            Some(sub) => sub.subscription.last_seq(),
            None => continue,
        };
        let Ok(backlog) = session.subscribe(&id, after).await else { continue };
        let pending = {
            let mut subs = shared.subs.lock().await;
            let Some(sub) = subs.get_mut(&id) else { continue };
            let mut advanced = false;
            for ev in &backlog {
                let before = sub.subscription.last_seq();
                // Count only frames that actually advanced the cursor (skip any
                // the cursor already covers), so a racing activation can't
                // double-apply.
                if matches!(sub.subscription.apply(ev), Ok(FoldOutcome::Applied { .. }))
                    && sub.subscription.last_seq() > before
                {
                    advanced = true;
                }
            }
            // Push once the whole missed span is folded, so a reconnect repaints
            // the healed transcript in one step rather than frame by frame.
            advanced.then(|| snapshot_of(sub)).flatten()
        };
        deliver(pending.into_iter().collect());
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// Subscribe to a session's transcript. Folds the whole backlog and pushes it
    /// as one snapshot — the cold-open bootstrap — after which live frames flow
    /// through the dispatcher. Re-subscribing replaces the prior sink.
    pub async fn subscribe(
        &self,
        session_id: String,
        sink: Arc<dyn ThreadSink>,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        // Fetch the authoritative transcript snapshot first, so the session opens
        // with its full history — including one restored from disk after a host
        // restart, which lives only in the desktop view's fold and never entered
        // the live event ring. Rehydrate from it and resume the live stream from its
        // cursor. An older host without `FetchTranscript` (or an empty base) falls
        // back to a fresh, backlog-only fold — exactly the prior cold-open behavior.
        let (mut subscription, after) = match session.fetch_transcript(&session_id).await {
            Ok(t) => {
                let entries: Vec<ThreadEntry> =
                    serde_json::from_str(&t.entries_json).unwrap_or_default();
                let sub = SessionSubscription::rehydrated(session_id.clone(), entries, t.model, t.seq);
                (sub, t.seq)
            }
            Err(_) => (SessionSubscription::new(session_id.clone()), 0),
        };
        let backlog = session
            .subscribe(&session_id, after)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        for ev in &backlog {
            let _ = subscription.apply(ev);
        }
        let sub = Sub { subscription, sink };
        // Push unconditionally, even for an empty backlog: the app is waiting for
        // a first snapshot to replace its loading state, and a session with no
        // history yet would otherwise sit on a spinner until the first event.
        deliver(snapshot_of(&sub).into_iter().collect());
        self.shared.subs.lock().await.insert(session_id, sub);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;

    use oximux_remote_proto::testing::duplex_pair;
    use oximux_remote_proto::transport::Transport;
    use oximux_remote_session::ClientSigner;
    use tokio::sync::Mutex as TokioMutex;

    use super::*;

    fn shared_at_epoch(epoch: u64) -> Arc<Shared> {
        Arc::new(Shared {
            session: StdMutex::new(None),
            subs: TokioMutex::new(HashMap::new()),
            epoch: AtomicU64::new(epoch),
            terminal_sink: StdMutex::new(None),
            attached: StdMutex::new(std::collections::HashSet::new()),
            sessions_sink: StdMutex::new(None),
        })
    }

    fn a_session() -> Arc<RemoteSession> {
        // The peer is dropped: with no subscriptions, `activate` issues no RPC, so
        // the transport is never driven — we only exercise the publish decision.
        let (client, _peer) = duplex_pair();
        Arc::new(RemoteSession::new(Arc::new(client) as Arc<dyn Transport>, ClientSigner::generate()))
    }

    /// The guard: an `activate` whose epoch has been superseded (a reconnect flap,
    /// or a `disconnect` racing an in-flight activation) must NOT publish its
    /// session — otherwise a dead connection would be resurrected under a UI that
    /// still reads "connected".
    #[tokio::test]
    async fn a_superseded_activate_declines_to_publish() {
        let shared = shared_at_epoch(5); // current epoch = 5
        let first: FirstResult = Arc::new(StdMutex::new(Some(oneshot::channel().0)));

        activate(shared.clone(), a_session(), first, 3).await; // stale epoch 3 != 5

        assert!(shared.session.lock().unwrap().is_none(), "stale activate must not publish");
    }

    /// The current activation publishes and signals the first-connection outcome.
    #[tokio::test]
    async fn a_current_activate_publishes_and_signals() {
        let shared = shared_at_epoch(7);
        let (tx, rx) = oneshot::channel();
        let first: FirstResult = Arc::new(StdMutex::new(Some(tx)));

        activate(shared.clone(), a_session(), first, 7).await; // epoch matches

        assert!(shared.session.lock().unwrap().is_some(), "current activate publishes");
        assert!(matches!(rx.await, Ok(Ok(()))), "first-connection outcome signaled Ok");
    }
}
