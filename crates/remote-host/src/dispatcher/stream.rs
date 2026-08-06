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
use tokio::sync::{broadcast, watch};

use super::Dispatcher;
use crate::auth::Peer;

/// One live event as it leaves the merged subscription stream, tagged with the
/// session it belongs to (the merge erases which stream produced it).
pub(super) struct LiveFrame {
    pub session_id: SessionId,
    pub seq: Seq,
    pub event: ThreadEvent,
}

/// Anything the serve loop can push to the client unsolicited.
///
/// Session events and terminal output share **one** merged stream rather than
/// getting a select arm each. The loop alternates which side leads so neither
/// requests nor live frames can starve the other; adding a third arm would make
/// that rotation a three-way fairness problem, and a terminal repainting a
/// full-screen TUI is exactly the flood that would win it.
pub(super) enum Live {
    Session(LiveFrame),
    Terminal { pty_id: String, frame: crate::terminals::TerminalFrame },
    /// The live session list changed (a session opened, closed, was renamed/
    /// remodeled, or flipped its permission flag). Carries no payload: the serve
    /// loop recomputes the per-device-filtered snapshot on receipt, so the scope
    /// filter is applied at forward time exactly like the per-frame recheck on
    /// events. Low-frequency and coalesced by the registry's generation counter, so
    /// it never floods the merge the way a full-screen terminal repaint would.
    SessionList,
    /// A schedule run was recorded (scheduled or manual, success or failure).
    /// Carries the run itself: unlike [`Live::SessionList`] there is no cheap
    /// snapshot to recompute at forward time, and the payload is one small row.
    /// Forwarded as [`Response::ScheduleRunsChanged`] behind a per-frame
    /// `may_read_schedules` recheck.
    ScheduleRun(oximux_remote_proto::messages::ScheduleRunWire),
    /// A coordination key was written or deleted (`None` is a delete). Carries
    /// the entry for the same reason [`Live::ScheduleRun`] does — there is no
    /// cheap snapshot to recompute, and it is one small row. Prefix filtering
    /// happens in the stream, not at forward time, because the prefix belongs
    /// to *this* subscription rather than to the peer.
    /// `with_cursor` is a property of THIS subscription, not of the peer: one
    /// connection may hold a v18 `StateWatch` and a v19 `StateWatchFrom` at
    /// once, and a peer-level flag has no way to tell them apart. Sending the
    /// cursor-bearing ordinal to the cursor-less watcher would drop its whole
    /// connection, since it has no decoder for it.
    StateChange { change: oximux_remote_proto::messages::StateChangeWire, with_cursor: bool },
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

/// A stream that waits for `session_id` to enter the registry, then forwards its
/// events — whatever it accumulated while waiting first, then live.
///
/// Subscribing to a dormant session has nothing to stream *yet*: nothing is
/// running, so nothing produces events. Building one just to watch it sit idle
/// would defeat reading a session for free, but leaving the client with no stream
/// at all is worse — it would have to poll to notice that its own next prompt had
/// brought the session to life. This waits instead, and costs one watch receiver.
///
/// Ends when the registry goes away (the host is shutting down), which is the only
/// way the session can become permanently unreachable.
fn deferred_live_stream(
    registry: std::sync::Arc<oximux_agents::session_registry::SessionRegistry>,
    session_id: SessionId,
    after_seq: Seq,
) -> BoxStream<'static, LiveFrame> {
    stream::once(async move {
        // Subscribed before the first lookup, so a session materializing between
        // the two is caught by the very next tick rather than waited on forever.
        let mut changes = registry.subscribe_changes();
        loop {
            if let Some(handle) = registry.get(&session_id) {
                // Receiver before snapshot, exactly as a live subscribe does: an
                // event landing in the gap is caught by the live half, and the
                // serve loop's cursor drops whichever copy arrives second.
                let rx = handle.subscribe();
                let caught_up: Vec<LiveFrame> = handle
                    .events_since(after_seq)
                    .into_iter()
                    .map(|(seq, event)| LiveFrame { session_id: session_id.clone(), seq, event })
                    .collect();
                return stream::iter(caught_up).chain(live_stream(session_id, rx)).boxed();
            }
            if changes.changed().await.is_err() {
                return stream::empty().boxed();
            }
        }
    })
    .flatten()
    .boxed()
}

/// Turn the registry's change receiver into a `'static` stream that yields a
/// [`Live::SessionList`] tick on every session-list change. The generation counter
/// coalesces, so a burst of changes wakes the serve loop once; the stream ends when
/// the registry (the sender) is dropped.
fn sessions_stream(rx: watch::Receiver<u64>) -> BoxStream<'static, Live> {
    stream::unfold(rx, |mut rx| async move {
        match rx.changed().await {
            Ok(()) => Some((Live::SessionList, rx)),
            Err(_) => None,
        }
    })
    .boxed()
}

/// Turn a recorded-runs broadcast receiver into a `'static` stream of
/// [`Live::ScheduleRun`]s. Lag skips the dropped runs rather than ending —
/// they are still in run history, and `schedule logs` is the resync path. The
/// stream ends when the host drops the sender.
fn schedule_runs_stream(
    rx: broadcast::Receiver<oximux_remote_proto::messages::ScheduleRunWire>,
) -> BoxStream<'static, Live> {
    stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(run) => return Some((Live::ScheduleRun(run), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .boxed()
}

/// One `StateWatch`'s change stream, narrowed to its prefix.
///
/// Lag **skips** the dropped writes rather than ending, like the run stream —
/// but the consequence differs and is worth naming: a watcher that lagged has a
/// stale view of the keys it missed, and its recovery is a `StateGet` (or a
/// re-`StateWatch`), not the transcript-style resync a session stream gets. The
/// versioned write path is what makes that safe: a caller acting on a stale
/// value loses its conditional write rather than clobbering.
fn state_changes_stream(
    rx: broadcast::Receiver<oximux_remote_proto::messages::StateChangeWire>,
    prefix: Option<String>,
    with_cursor: bool,
) -> BoxStream<'static, Live> {
    stream::unfold((rx, prefix), move |(mut rx, prefix)| async move {
        loop {
            match rx.recv().await {
                Ok(change) => {
                    if prefix.as_ref().is_some_and(|p| !change.key.starts_with(p.as_str())) {
                        continue;
                    }
                    return Some((Live::StateChange { change, with_cursor }, (rx, prefix)));
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
        peer: &Peer,
        session_id: &str,
        after_seq: u64,
        cursors: &mut HashMap<SessionId, Seq>,
    ) -> (Response, Option<BoxStream<'static, LiveFrame>>) {
        if !self.auth.is_allowed_for(peer, session_id) {
            return (Response::Error(RpcError::Unauthorized), None);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return self.begin_dormant_subscribe(session_id, after_seq, cursors);
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

    /// Subscribe to a session with no live views behind it.
    ///
    /// Answers with an empty backlog — a session that is not running has produced
    /// no events — and a stream that starts forwarding once something brings it to
    /// life. Building it here instead would make opening a conversation to read it
    /// cost an agent process, which is the whole thing [`SessionCatalog::transcript`]
    /// exists to avoid.
    ///
    /// [`SessionCatalog::transcript`]: crate::catalog::SessionCatalog::transcript
    fn begin_dormant_subscribe(
        &self,
        session_id: &str,
        after_seq: Seq,
        cursors: &mut HashMap<SessionId, Seq>,
    ) -> (Response, Option<BoxStream<'static, LiveFrame>>) {
        // An id nothing on this desktop knows is not a session to wait on. Without
        // this a typo would open a stream that could never produce anything, and
        // the client would sit on it instead of being told there is no such
        // session — the answer it got before dormant sessions were reachable.
        if self.catalog.as_ref().and_then(|c| c.transcript(session_id)).is_none() {
            return (Response::Error(RpcError::UnknownSession), None);
        }
        // Already waiting on this session for this connection: a second stream
        // would deliver every event twice once it materializes.
        if cursors.contains_key(session_id) {
            return (Response::Events(Vec::new()), None);
        }
        cursors.insert(session_id.to_string(), after_seq);
        let stream = deferred_live_stream(self.registry.clone(), session_id.to_string(), after_seq);
        (Response::Events(Vec::new()), Some(stream))
    }

    /// Forward one live frame to the client, honoring the per-frame authorization
    /// recheck (revocation/scope) and the backlog dedup cursor. Returns whether the
    /// transport is still writable (`false` → the serve loop should stop).
    pub(super) async fn forward_live(
        &self,
        peer: &Peer,
        cursors: &mut HashMap<SessionId, Seq>,
        transport: &dyn Transport,
        frame: LiveFrame,
    ) -> bool {
        // Revocation / scope bites the live stream too: suppress silently but keep
        // the connection open (other sessions may still be permitted).
        if !self.auth.is_allowed_for(peer, &frame.session_id) {
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

    /// Set up a session-list subscription: reply with the current per-device
    /// snapshot, then — on the first subscribe for this connection — a stream that
    /// pushes a fresh snapshot on every registry change. A repeat subscribe serves
    /// the snapshot only: a second change-receiver would leak for the connection's
    /// life and double every push.
    pub(super) fn begin_subscribe_sessions(
        &self,
        peer: &Peer,
        already: &mut bool,
    ) -> (Response, Option<BoxStream<'static, Live>>) {
        if *already {
            return (Response::Sessions(self.snapshot_sessions(peer)), None);
        }
        // Receiver before snapshot so a change landing in the gap fires a tick
        // rather than being missed — an extra identical push is harmless. The
        // immediate reply is a plain `Sessions` (routed to the client's RPC slot);
        // only the later change pushes are `SessionsChanged`.
        let rx = self.registry.subscribe_changes();
        let snapshot = self.snapshot_sessions(peer);
        *already = true;
        (Response::Sessions(snapshot), Some(sessions_stream(rx)))
    }

    /// Push the current session list, recomputed per change so the per-device scope
    /// filter is honoured on every push — mirroring `forward_live`'s per-frame
    /// recheck. Returns whether the transport is still writable.
    pub(super) async fn forward_sessions(&self, peer: &Peer, transport: &dyn Transport) -> bool {
        let snapshot = self.snapshot_sessions(peer);
        transport
            .send(
                Response::SessionsChanged(snapshot)
                    .to_bytes()
                    .expect("response is always encodable"),
            )
            .await
            .is_ok()
    }

    /// The recorded-runs push stream for one connection, when this host has a
    /// schedule-events channel at all. The caller gates on the peer's declared
    /// protocol version — an older peer cannot decode the push frame.
    pub(super) fn schedule_runs_push_stream(&self) -> Option<BoxStream<'static, Live>> {
        Some(schedule_runs_stream(self.schedule_events.as_ref()?.subscribe()))
    }

    /// The coordination-change push stream for one `StateWatch`, narrowed to
    /// its own prefix. Filtered here rather than at forward time because two
    /// watches on one connection may hold different prefixes — a peer-level
    /// check has nothing to filter against.
    pub(super) fn state_push_stream(
        &self,
        prefix: Option<String>,
        with_cursor: bool,
    ) -> Option<BoxStream<'static, Live>> {
        Some(state_changes_stream(self.state_events.as_ref()?.subscribe(), prefix, with_cursor))
    }

    /// Push one coordination change, behind the same per-frame authorization
    /// recheck every live surface applies. Returns whether the transport is
    /// still writable.
    pub(super) async fn forward_state_change(
        &self,
        peer: &Peer,
        transport: &dyn Transport,
        change: oximux_remote_proto::messages::StateChangeWire,
        with_cursor: bool,
    ) -> bool {
        if !self.auth.may_read_state(peer) {
            return true;
        }
        // The cursor-less shape is not a legacy path to be migrated off: a v18
        // watcher subscribed with `StateWatch` and has a decoder for exactly
        // one of these two ordinals.
        let response = if with_cursor {
            Response::StateChangedAt(change)
        } else {
            Response::StateChanged { key: change.key, entry: change.entry }
        };
        transport
            .send(response.to_bytes().expect("response is always encodable"))
            .await
            .is_ok()
    }

    /// Push one recorded run, behind the same per-frame authorization recheck
    /// every live surface applies: a device that may not read schedules gets
    /// silence, not a filtered-out error. Returns whether the transport is
    /// still writable.
    pub(super) async fn forward_schedule_run(
        &self,
        peer: &Peer,
        transport: &dyn Transport,
        run: oximux_remote_proto::messages::ScheduleRunWire,
    ) -> bool {
        if !self.auth.may_read_schedules(peer) {
            return true;
        }
        transport
            .send(
                Response::ScheduleRunsChanged(run)
                    .to_bytes()
                    .expect("response is always encodable"),
            )
            .await
            .is_ok()
    }
}

impl Dispatcher {
    /// List the terminals this device may see.
    pub(super) async fn list_terminals(&self, peer: &Peer) -> Response {
        if !self.auth.may_use_terminals(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        // No terminal source wired is a CAPABILITY fact, not an access one: a
        // headless host without a relay serves everything except terminals, and
        // that is documented as normal. Reporting it as `Unauthorized` sent
        // operators hunting for a credential problem that does not exist.
        let Some(source) = &self.terminals else {
            return Response::Error(RpcError::Unsupported);
        };
        match source.list().await {
            Ok(rows) => Response::Terminals(rows),
            Err(e) => {
                tracing::warn!(error = %e, "list terminals failed");
                Response::Error(RpcError::Internal("terminals unavailable".into()))
            }
        }
    }

    /// Attach to a terminal: immediate replay, plus a live stream to register.
    ///
    /// A repeat attach to a terminal this connection already streams serves the
    /// replay again WITHOUT opening a second stream — the same rule `Subscribe`
    /// follows, and for the same reason: a second stream would leak a receiver
    /// for the life of the connection and double every subsequent frame.
    /// Re-attaching is also the documented gap recovery, so it must be cheap and
    /// repeatable rather than something a client is punished for.
    pub(super) async fn begin_term_attach(
        &self,
        peer: &Peer,
        pty_id: &str,
        attached: &mut HashMap<String, tokio::sync::oneshot::Sender<()>>,
    ) -> (Response, Option<BoxStream<'static, Live>>) {
        if !self.auth.may_use_terminals(peer) {
            return (Response::Error(RpcError::Unauthorized), None);
        }
        // Capability, not access — see `list_terminals`.
        let Some(source) = &self.terminals else {
            return (Response::Error(RpcError::Unsupported), None);
        };
        let (attach, rx) = match source.attach(pty_id).await {
            Ok(v) => v,
            Err(crate::terminals::TerminalError::NotFound) => {
                return (Response::Error(RpcError::UnknownSession), None);
            }
            Err(e) => {
                tracing::warn!(error = %e, "terminal attach failed");
                return (Response::Error(RpcError::Internal("terminals unavailable".into())), None);
            }
        };
        let response = Response::TermAttached {
            replay: attach.replay,
            cols: attach.cols,
            rows: attach.rows,
        };
        if attached.contains_key(pty_id) {
            // Already streaming: `rx` drops here, which detaches the duplicate
            // attachment rather than leaving it fanning into nothing.
            return (response, None);
        }
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        attached.insert(pty_id.to_owned(), cancel_tx);
        (response, Some(term_stream(pty_id.to_owned(), rx, cancel_rx)))
    }

    /// Type into a terminal. The gate here is the write tier, not merely
    /// terminal visibility — this is arbitrary code execution on the desktop.
    pub(super) async fn term_input(&self, peer: &Peer, pty_id: &str, bytes: &[u8]) -> Response {
        if !self.auth.may_drive_terminals(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        // Capability, not access — see `list_terminals`.
        let Some(source) = &self.terminals else {
            return Response::Error(RpcError::Unsupported);
        };
        match source.input(pty_id, bytes).await {
            Ok(()) => Response::Ack,
            Err(crate::terminals::TerminalError::NotFound) => {
                Response::Error(RpcError::UnknownSession)
            }
            Err(e) => {
                tracing::warn!(error = %e, "terminal input failed");
                Response::Error(RpcError::Internal("terminals unavailable".into()))
            }
        }
    }

    /// Resize a terminal — write-gated, because the size is shared with every
    /// other attachment including the desktop's own window.
    pub(super) async fn term_resize(
        &self,
        peer: &Peer,
        pty_id: &str,
        cols: u16,
        rows: u16,
    ) -> Response {
        if !self.auth.may_drive_terminals(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        // Capability, not access — see `list_terminals`.
        let Some(source) = &self.terminals else {
            return Response::Error(RpcError::Unsupported);
        };
        // A zero dimension is nonsense a PTY layer may accept and then divide by.
        // Rejected here rather than clamped: a client asking for a 0-column grid
        // has a bug, and silently serving it something else hides that.
        if cols == 0 || rows == 0 {
            return Response::Error(RpcError::BadRequest("terminal size must be non-zero".into()));
        }
        match source.resize(pty_id, cols, rows).await {
            Ok(()) => Response::Ack,
            Err(crate::terminals::TerminalError::NotFound) => {
                Response::Error(RpcError::UnknownSession)
            }
            Err(e) => {
                tracing::warn!(error = %e, "terminal resize failed");
                Response::Error(RpcError::Internal("terminals unavailable".into()))
            }
        }
    }
}

/// Turn an attached terminal's frame receiver into a `'static` merged stream.
///
/// Ends when the source drops the sender (the terminal is gone) **or** when
/// `cancel` fires. The cancel half is not optional bookkeeping: `SelectAll` has
/// no way to remove one stream, so without it a detach could only forget the
/// entry while the stream kept forwarding — and the next attach would add a
/// second one beside it. Duplicated output accumulates per attach/detach cycle,
/// which is a screen full of doubled characters rather than a leak anyone would
/// notice as one.
fn term_stream(
    pty_id: String,
    rx: tokio::sync::mpsc::Receiver<crate::terminals::TerminalFrame>,
    cancel: tokio::sync::oneshot::Receiver<()>,
) -> BoxStream<'static, Live> {
    stream::unfold((pty_id, rx, cancel), |(pty_id, mut rx, mut cancel)| async move {
        tokio::select! {
            // Biased so a pending cancel wins over a frame that is already
            // queued: after a detach the client is no longer rendering this
            // terminal, and delivering one more frame would reach a screen that
            // has moved on.
            biased;
            _ = &mut cancel => None,
            frame = rx.recv() => {
                let frame = frame?;
                Some((Live::Terminal { pty_id: pty_id.clone(), frame }, (pty_id, rx, cancel)))
            }
        }
    })
    .boxed()
}

/// Forward one terminal frame. Returns whether the transport is still writable.
pub(super) async fn forward_terminal(
    auth: &crate::auth::AuthStore,
    peer: &Peer,
    transport: &dyn Transport,
    pty_id: String,
    frame: crate::terminals::TerminalFrame,
) -> bool {
    // Per-frame recheck, matching the session path: a device revoked or
    // downgraded mid-stream stops seeing a terminal it was already watching.
    // Read access is the right gate here — this direction never types.
    if !auth.may_use_terminals(peer) {
        return true;
    }
    let response = match frame {
        crate::terminals::TerminalFrame::Output(bytes) => Response::TermOutput { pty_id, bytes },
        crate::terminals::TerminalFrame::Gapped => Response::TermGapped { pty_id },
        crate::terminals::TerminalFrame::Exited(code) => Response::TermExited { pty_id, code },
    };
    transport
        .send(response.to_bytes().expect("response is always encodable"))
        .await
        .is_ok()
}
