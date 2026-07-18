//! Live event forwarding for `Subscribe`.
//!
//! A subscription turns a session's registry broadcast into a `'static` stream of
//! tagged [`LiveFrame`]s that the serve loop merges (`SelectAll`) and pushes to the
//! client as [`Response::Event`] frames. Two invariants ride here:
//!
//! - **Per-frame authorization recheck.** Revocation and per-device scope must bite
//!   the live stream too, not only request RPCs — so a device revoked mid-stream
//!   stops receiving events even though its broadcast receiver is still open.
//! - **Backlog dedup cursor.** Subscribing snapshots the backlog *after* taking the
//!   broadcast receiver, so an event landing in that gap is caught by the live
//!   stream; a per-session cursor then drops any live `seq` already covered by the
//!   backlog batch, so the client never sees a `seq` twice.

use std::collections::HashMap;

use futures::stream::{self, BoxStream, StreamExt};
use oximux_agent_core::thread::ThreadEvent;
use oximux_agents::session_registry::{Seq, SessionHandle, SessionId};
use oximux_remote_proto::messages::SessionStatusWire;
use oximux_remote_proto::proto::{Response, RpcError};
use oximux_remote_proto::{HostEvent, Transport};
use tokio::sync::broadcast;

use super::Dispatcher;
use crate::auth::AppPubkey;

/// One live event as it leaves the merged subscription stream, tagged with the
/// session it belongs to (the merge erases which stream produced it).
pub(super) struct LiveFrame {
    pub session_id: SessionId,
    pub seq: Seq,
    pub event: ThreadEvent,
}

/// Turn a session's broadcast receiver into a `'static` stream of [`LiveFrame`]s.
/// A lagged receiver (a slow client outran the ring) **skips** the dropped span
/// rather than ending — the client resynchronizes via `EventsSince`. The stream
/// ends when the session's last handle drops (broadcast closed).
fn live_stream(
    session_id: SessionId,
    rx: broadcast::Receiver<(Seq, ThreadEvent)>,
) -> BoxStream<'static, LiveFrame> {
    stream::unfold((session_id, rx), |(session_id, mut rx)| async move {
        loop {
            match rx.recv().await {
                Ok((seq, event)) => {
                    let frame = LiveFrame { session_id: session_id.clone(), seq, event };
                    return Some((frame, (session_id, rx)));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .boxed()
}

/// Encode the retained backlog after `after_seq` into wire frames, returning the
/// frames and the highest `seq` seen (the initial dedup cursor). `Err(())` on an
/// encode failure. Each frame carries the session's current coarse status.
fn backlog_frames(
    handle: &SessionHandle,
    session_id: &str,
    after_seq: u64,
) -> Result<(Vec<HostEvent>, Seq), ()> {
    let status = handle.status_snapshot();
    let wire = SessionStatusWire {
        last_seq: status.last_seq,
        awaiting_permission: status.awaiting_permission,
    };
    let mut frames = Vec::new();
    let mut cursor = after_seq;
    for (seq, event) in handle.events_since(after_seq) {
        let frame = HostEvent::new(session_id, seq, &event, wire.clone()).map_err(|_| ())?;
        cursor = cursor.max(seq);
        frames.push(frame);
    }
    Ok((frames, cursor))
}

impl Dispatcher {
    /// Set up a live subscription. Authorizes + scope-checks, then:
    /// - **First subscribe on this connection** for the session: takes the
    ///   broadcast receiver **before** snapshotting the backlog (so no event slips
    ///   through the gap), seeds the dedup cursor, and returns the immediate
    ///   [`Response::Events`] backlog plus the live stream to register.
    /// - **Repeat subscribe** (already streaming this session on this connection):
    ///   serves the requested backlog only — no second live stream is opened (that
    ///   would leak a receiver for the life of the connection) and the live cursor
    ///   is left untouched so dedup keeps holding.
    ///
    /// Returns the immediate response and, only on the first subscribe, the stream.
    pub(super) fn begin_subscribe(
        &self,
        pubkey: &AppPubkey,
        session_id: &str,
        after_seq: u64,
        cursors: &mut HashMap<SessionId, Seq>,
    ) -> (Response, Option<BoxStream<'static, LiveFrame>>) {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return (Response::Error(RpcError::Unauthorized), None);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return (Response::Error(RpcError::UnknownSession), None);
        };

        // Already streaming this session on this connection → backlog only,
        // idempotent, cursor preserved.
        if cursors.contains_key(session_id) {
            return match backlog_frames(&handle, session_id, after_seq) {
                Ok((frames, _)) => (Response::Events(frames), None),
                Err(()) => (Response::Error(RpcError::Internal("event encode failed".into())), None),
            };
        }

        // First subscribe: receiver before snapshot so an event landing in the gap
        // is caught live and deduped by the cursor, never dropped.
        let rx = handle.subscribe();
        match backlog_frames(&handle, session_id, after_seq) {
            Ok((frames, cursor)) => {
                cursors.insert(session_id.to_string(), cursor);
                (Response::Events(frames), Some(live_stream(session_id.to_string(), rx)))
            }
            Err(()) => (Response::Error(RpcError::Internal("event encode failed".into())), None),
        }
    }

    /// Forward one live frame to the client, honoring the per-frame authorization
    /// recheck (revocation/scope) and the backlog dedup cursor. Returns whether the
    /// transport is still writable (`false` → the serve loop should stop).
    pub(super) async fn forward_live(
        &self,
        pubkey: &AppPubkey,
        cursors: &mut HashMap<SessionId, Seq>,
        transport: &dyn Transport,
        frame: LiveFrame,
    ) -> bool {
        // Revocation / scope bites the live stream too: suppress silently but keep
        // the connection open (other sessions may still be permitted).
        if !self.auth.is_allowed_for(pubkey, &frame.session_id) {
            return true;
        }
        // Never re-send a `seq` already covered by the backlog batch (or an
        // out-of-order late duplicate).
        let cursor = cursors.entry(frame.session_id.clone()).or_insert(0);
        if frame.seq <= *cursor {
            return true;
        }
        *cursor = frame.seq;

        let status = self
            .registry
            .get(&frame.session_id)
            .map(|h| h.status_snapshot())
            .unwrap_or_default();
        let wire = SessionStatusWire {
            last_seq: status.last_seq,
            awaiting_permission: status.awaiting_permission,
        };
        let host_event = match HostEvent::new(&frame.session_id, frame.seq, &frame.event, wire) {
            Ok(e) => e,
            Err(_) => return true, // can't encode this one; keep the stream alive
        };
        transport
            .send(Response::Event(host_event).to_bytes().expect("response is always encodable"))
            .await
            .is_ok()
    }
}
