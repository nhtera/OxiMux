//! Live subscription: fold a session's `HostEvent` stream and push each applied
//! event to the foreign [`EventSink`]. The demux pump feeds one connection-wide
//! event stream; [`run_dispatcher`] routes each frame to the matching registered
//! session and heals seq gaps via `events_since`.
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

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};

use futures::StreamExt;
use futures::channel::oneshot;
use oximux_remote_proto::HostEvent;
use oximux_remote_session::{EventStream, FoldOutcome, RemoteSession, SessionSubscription};

use crate::callbacks::EventSink;
use crate::client::{MobileClient, Shared};
use crate::ffi_types::{MobileError, RemoteEvent};
use crate::runtime::rt;

/// The first-connection outcome channel `connect_with` awaits: `Ok` once the
/// session is live and wired up, `Err` if the host is unreachable before it ever
/// connects. Held behind a mutex so the one-shot fires exactly once.
pub(crate) type FirstResult = Arc<StdMutex<Option<oneshot::Sender<Result<(), MobileError>>>>>;

/// A registered subscription: the fold cursor + the foreign sink to push into.
pub(crate) struct Sub {
    pub subscription: SessionSubscription,
    pub sink: Arc<dyn EventSink>,
}

/// Project + push one wire frame to the sink (best-effort; a malformed frame is
/// skipped rather than tearing down the stream).
fn forward(sink: &Arc<dyn EventSink>, frame: &HostEvent) {
    if let Ok(ev) = RemoteEvent::from_host_event(frame) {
        sink.on_event(ev);
    }
}

/// The connection-wide event loop: fold each frame into its session's cursor and
/// forward it; on a seq gap, backfill from the host and forward the recovered
/// span. Ends when the pump closes the stream (link lost / disconnect).
pub(crate) async fn run_dispatcher(shared: Arc<Shared>, mut events: EventStream) {
    while let Some(frame) = events.next().await {
        let outcome = {
            let mut subs = shared.subs.lock().await;
            match subs.get_mut(&frame.session_id) {
                None => continue, // no subscriber for this session
                Some(sub) => match sub.subscription.apply(&frame) {
                    Ok(FoldOutcome::Applied { .. }) => {
                        forward(&sub.sink, &frame);
                        continue;
                    }
                    Ok(FoldOutcome::Gap { resume_from }) => Some(resume_from),
                    _ => continue, // dedup / already-seen
                },
            }
        };
        // Gap: fetch the missed span WITHOUT holding the subs lock across the RPC.
        if let Some(resume_from) = outcome {
            backfill_gap(&shared, &frame.session_id, resume_from).await;
        }
    }
}

/// Re-fetch `events_since(resume_from)` and fold+forward the recovered frames.
async fn backfill_gap(shared: &Arc<Shared>, session_id: &str, resume_from: u64) {
    let Ok(session) = shared.session() else { return };
    let Ok(backlog) = session.events_since(session_id, resume_from).await else { return };
    let mut subs = shared.subs.lock().await;
    let Some(sub) = subs.get_mut(session_id) else { return };
    for ev in &backlog {
        if matches!(sub.subscription.apply(ev), Ok(FoldOutcome::Applied { .. })) {
            forward(&sub.sink, ev);
        }
    }
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
    resubscribe_all(&shared, &session).await;
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
        let mut subs = shared.subs.lock().await;
        let Some(sub) = subs.get_mut(&id) else { continue };
        for ev in &backlog {
            let before = sub.subscription.last_seq();
            // Forward only frames that actually advanced the cursor (skip any the
            // cursor already covers), so a racing activation can't double-deliver.
            if matches!(sub.subscription.apply(ev), Ok(FoldOutcome::Applied { .. }))
                && sub.subscription.last_seq() > before
            {
                forward(&sub.sink, ev);
            }
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// Subscribe to a session's live events. Seeds the backlog (folded + pushed to
    /// `sink`), then live frames flow through the dispatcher. Re-subscribing
    /// replaces the prior sink for that session.
    pub async fn subscribe(
        &self,
        session_id: String,
        sink: Arc<dyn EventSink>,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        let backlog = session
            .subscribe(&session_id, 0)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        let mut sub = Sub { subscription: SessionSubscription::new(session_id.clone()), sink };
        for ev in &backlog {
            if matches!(sub.subscription.apply(ev), Ok(FoldOutcome::Applied { .. })) {
                forward(&sub.sink, ev);
            }
        }
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
