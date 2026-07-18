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

use std::sync::Arc;

use futures::StreamExt;
use oximux_remote_proto::HostEvent;
use oximux_remote_session::{EventStream, FoldOutcome, SessionSubscription};

use crate::callbacks::EventSink;
use crate::client::{MobileClient, Shared};
use crate::ffi_types::{MobileError, RemoteEvent};

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
    let Ok(session) = shared.session().await else { return };
    let Ok(backlog) = session.events_since(session_id, resume_from).await else { return };
    let mut subs = shared.subs.lock().await;
    let Some(sub) = subs.get_mut(session_id) else { return };
    for ev in &backlog {
        if matches!(sub.subscription.apply(ev), Ok(FoldOutcome::Applied { .. })) {
            forward(&sub.sink, ev);
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
        let session = self.shared.session().await?;
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
