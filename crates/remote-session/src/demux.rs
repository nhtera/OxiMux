//! The multiplexed client connection: a single read loop (the "pump") that
//! demuxes the host's frames — pushed [`Response::Event`]s go to the live event
//! stream, every other reply goes to the outstanding RPC — so a client can issue
//! RPCs *while* subscribed to a live session, all over one transport.
//!
//! The wire carries no per-request correlation id: replies come back in send
//! order. So RPCs are **serialized** (one outstanding at a time, `rpc_lock`) and
//! the pump routes the next non-event frame to the single pending reply slot.
//! Events are distinguished purely by variant and never consume the reply slot,
//! so they interleave freely between an RPC's request and its reply.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use futures::lock::Mutex as AsyncMutex;
use oximux_remote_proto::messages::SessionSummary;
use oximux_remote_proto::proto::{Request, Response};
use oximux_remote_proto::transport::TransportError;
use oximux_remote_proto::{HostEvent, Transport};

use crate::error::SessionError;

type Result<T> = std::result::Result<T, SessionError>;

/// The receiver end of the live event stream the pump feeds. Ends (`None`) once
/// the pump stops — the host closed the connection, or the connection's owner was
/// dropped.
pub type EventStream = mpsc::UnboundedReceiver<HostEvent>;

/// One unsolicited terminal frame from the host.
///
/// Terminal pushes ride their own channel rather than sharing the session-event
/// stream: the two have different consumers with different lifetimes (a phone
/// can watch a terminal without subscribing to any agent session), and merging
/// them would make either consumer's backpressure the other's problem.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalPush {
    Output { pty_id: String, bytes: Vec<u8> },
    /// The host dropped output for us; re-attach to resync.
    Gapped { pty_id: String },
    Exited { pty_id: String, code: Option<i32> },
}

/// The receiver end of the live terminal stream the pump feeds.
pub type TerminalStream = mpsc::UnboundedReceiver<TerminalPush>;

/// The receiver end of the pushed session-list stream. Each item is a fresh, full,
/// per-device-filtered snapshot (not a delta): the host recomputes and pushes the
/// whole list on every change, so the client replaces its list wholesale. Rides its
/// own channel for the same reason terminal pushes do — a different consumer with a
/// different lifetime, and its own backpressure.
pub type SessionsStream = mpsc::UnboundedReceiver<Vec<SessionSummary>>;

/// The shared reply slot. Each [`Demux::call`] registers a one-shot sender here
/// before sending its request; the pump moves the matching reply into it.
///
/// `alive` guards the teardown race: when the pump stops it clears `slot` (so an
/// in-flight `call` sees its sender dropped → `Closed`) and clears `alive`; a
/// `call` that registers *after* the pump already exited would otherwise orphan
/// its sender forever, so it checks `alive` post-send and reclaims.
struct Pending {
    /// Carries a `Result` rather than a bare `Response` so the pump can fail one
    /// call without inventing a wire error for it — an oversize reply has no
    /// `Response` representation, and there is no correlation id to attach a
    /// separate error frame to.
    slot: Mutex<Option<oneshot::Sender<Result<Response>>>>,
    alive: AtomicBool,
}

/// The RPC-handle half of a multiplexed connection. Clone-shareable via `Arc`;
/// every `call` serializes on `rpc_lock` so exactly one reply is ever
/// outstanding, which is what lets a correlation-id-free wire stay aligned.
pub struct Demux {
    transport: Arc<dyn Transport>,
    pending: Arc<Pending>,
    rpc_lock: AsyncMutex<()>,
}

/// The read-loop half. Drive it once with [`DemuxPump::run`] for the connection's
/// whole lifetime — spawned onto an executor in prod, joined in tests.
pub struct DemuxPump {
    transport: Arc<dyn Transport>,
    pending: Arc<Pending>,
    events: mpsc::UnboundedSender<HostEvent>,
    terminals: mpsc::UnboundedSender<TerminalPush>,
    sessions: mpsc::UnboundedSender<Vec<SessionSummary>>,
    shutdown: oneshot::Receiver<()>,
}

/// Wire a transport into an RPC handle, its pump, the live event stream, and a
/// shutdown sender. Holding the [`oneshot::Sender`] keeps the pump running;
/// dropping it stops the pump — so the connection's owner just holds it and drops
/// it on teardown (no explicit close call needed).
pub fn demux(
    transport: Arc<dyn Transport>,
) -> (Arc<Demux>, DemuxPump, EventStream, TerminalStream, SessionsStream, oneshot::Sender<()>) {
    let pending = Arc::new(Pending { slot: Mutex::new(None), alive: AtomicBool::new(true) });
    let (events_tx, events_rx) = mpsc::unbounded();
    let (terminals_tx, terminals_rx) = mpsc::unbounded();
    let (sessions_tx, sessions_rx) = mpsc::unbounded();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = Arc::new(Demux {
        transport: transport.clone(),
        pending: pending.clone(),
        rpc_lock: AsyncMutex::new(()),
    });
    let pump = DemuxPump {
        transport,
        pending,
        events: events_tx,
        terminals: terminals_tx,
        sessions: sessions_tx,
        shutdown: shutdown_rx,
    };
    (handle, pump, events_rx, terminals_rx, sessions_rx, shutdown_tx)
}

impl Demux {
    /// Send one request and await its reply, demuxed past any interleaved live
    /// events by the pump. Serialized: concurrent callers queue on `rpc_lock`, so
    /// the pump only ever holds one pending reply slot at a time.
    pub async fn call(&self, req: Request) -> Result<Response> {
        let _guard = self.rpc_lock.lock().await;
        let (tx, rx) = oneshot::channel();
        // Register the reply slot BEFORE sending, so the pump can never observe
        // the reply before a slot exists to route it into.
        *self.pending.slot.lock().unwrap() = Some(tx);
        let bytes = req.to_bytes().map_err(|e| SessionError::Wire(e.to_string()))?;
        if let Err(e) = self.transport.send(bytes).await {
            self.pending.slot.lock().unwrap().take(); // reclaim: no reply is coming
            return Err(SessionError::Transport(e.to_string()));
        }
        // If the pump already exited (host dropped mid-call), it can no longer
        // route or cancel our sender — reclaim it and fail fast instead of hanging.
        // If it is still alive, either it delivers the reply or its teardown drops
        // our sender, resolving `rx` to `Closed` — never an orphan.
        if !self.pending.alive.load(Ordering::Acquire) {
            self.pending.slot.lock().unwrap().take();
            return Err(SessionError::Closed);
        }
        rx.await.map_err(|_| SessionError::Closed)?
    }
}

impl Drop for DemuxPump {
    /// On any exit — host close, transport error, or the owner's shutdown — cancel
    /// an in-flight RPC so its `call` returns `Closed` rather than waiting on a
    /// reply that can never arrive.
    fn drop(&mut self) {
        self.pending.alive.store(false, Ordering::Release);
        drop(self.pending.slot.lock().unwrap().take());
    }
}

impl DemuxPump {
    /// The single read loop: route each frame until the host closes the transport
    /// or the connection's owner drops the shutdown sender.
    pub async fn run(mut self) -> Result<()> {
        loop {
            let frame = match select(self.transport.recv(), &mut self.shutdown).await {
                Either::Left((Ok(Some(frame)), _)) => frame,
                Either::Left((Ok(None), _)) => break, // host closed the connection
                // The transport already resynchronized past the oversize frame, so
                // the connection is still good: fail the call that was waiting on
                // it and keep pumping. Ending the loop here instead would drop every
                // live subscription over one reply, and would surface as `Closed` —
                // a lost link, which is not what happened and not what to retry.
                Either::Left((Err(TransportError::FrameTooLarge { len, cap }), _)) => {
                    if let Some(tx) = self.pending.slot.lock().unwrap().take() {
                        let _ = tx.send(Err(SessionError::OversizeReply { len, cap }));
                    }
                    continue;
                }
                Either::Left((Err(e), _)) => {
                    return Err(SessionError::Transport(e.to_string()));
                }
                Either::Right(_) => break, // owner dropped the sender → shut down
            };
            match Response::from_bytes(&frame).map_err(|e| SessionError::Wire(e.to_string()))? {
                Response::Event(event) => {
                    // A dropped receiver just means the owner stopped consuming
                    // events; keep pumping so in-flight RPC replies still route.
                    let _ = self.events.unbounded_send(event);
                }
                // Terminal frames are pushed, exactly like `Event`, and MUST be
                // routed away from the reply slot. Falling through to the
                // catch-all below would hand terminal output to whatever RPC
                // happened to be outstanding and desync the connection for the
                // rest of its life — the wire has no correlation id, so a single
                // misrouted frame is unrecoverable.
                Response::TermOutput { pty_id, bytes } => {
                    let _ = self.terminals.unbounded_send(TerminalPush::Output { pty_id, bytes });
                }
                Response::TermGapped { pty_id } => {
                    let _ = self.terminals.unbounded_send(TerminalPush::Gapped { pty_id });
                }
                Response::TermExited { pty_id, code } => {
                    let _ = self.terminals.unbounded_send(TerminalPush::Exited { pty_id, code });
                }
                // The pushed session list is unsolicited like `Event`/`Term*` and
                // MUST route away from the reply slot — the catch-all below would
                // hand it to whatever RPC is outstanding and desync the connection.
                Response::SessionsChanged(rows) => {
                    let _ = self.sessions.unbounded_send(rows);
                }
                reply => {
                    if let Some(tx) = self.pending.slot.lock().unwrap().take() {
                        let _ = tx.send(Ok(reply));
                    }
                    // No pending slot for a non-event frame means the host sent an
                    // unsolicited reply — it never does; drop rather than desync.
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::executor::block_on;
    use futures::future::join;
    use oximux_remote_proto::transport::TransportError;

    /// A transport whose peer has closed the read side: `send` still succeeds, but
    /// `recv` immediately reports end-of-stream. Models the host dropping the
    /// connection while an RPC is in flight.
    struct HostClosedTransport;

    #[async_trait]
    impl Transport for HostClosedTransport {
        async fn send(&self, _frame: Vec<u8>) -> std::result::Result<(), TransportError> {
            Ok(())
        }
        async fn recv(&self) -> std::result::Result<Option<Vec<u8>>, TransportError> {
            Ok(None)
        }
    }

    /// An RPC in flight when the pump stops must resolve to `Closed`, never hang on
    /// a reply that can no longer arrive.
    #[test]
    fn call_errors_instead_of_hanging_when_the_pump_stops() {
        let (handle, pump, _events, _terminals, _sessions, _shutdown) =
            demux(Arc::new(HostClosedTransport));
        let (pump_res, call_res) = block_on(join(pump.run(), handle.call(Request::Ping)));
        assert!(pump_res.is_ok(), "clean stop on host close");
        assert!(
            matches!(call_res, Err(SessionError::Closed)),
            "in-flight call reports Closed, got {call_res:?}"
        );
    }

    /// A transport that answers the first request with an oversize frame and the
    /// second normally. Models a host whose transcript reply was too large while
    /// other traffic rides the same connection.
    ///
    /// `recv` blocks until a request has actually been sent, rather than handing
    /// out a scripted sequence on demand: the pump is polled first, so a transport
    /// that answers immediately would run the whole script before any caller had
    /// registered a reply slot, and the test would prove nothing about routing.
    struct OversizeThenHealthy {
        requests: AsyncMutex<mpsc::UnboundedReceiver<()>>,
        sent: mpsc::UnboundedSender<()>,
        answered: Mutex<usize>,
    }

    impl OversizeThenHealthy {
        fn new() -> Self {
            let (sent, requests) = mpsc::unbounded();
            Self { requests: AsyncMutex::new(requests), sent, answered: Mutex::new(0) }
        }
    }

    #[async_trait]
    impl Transport for OversizeThenHealthy {
        async fn send(&self, _frame: Vec<u8>) -> std::result::Result<(), TransportError> {
            let _ = self.sent.unbounded_send(());
            Ok(())
        }
        async fn recv(&self) -> std::result::Result<Option<Vec<u8>>, TransportError> {
            use futures::StreamExt as _;
            // Wait for a request before answering one.
            if self.requests.lock().await.next().await.is_none() {
                return Ok(None);
            }
            let mut answered = self.answered.lock().unwrap();
            *answered += 1;
            match *answered {
                1 => Err(TransportError::FrameTooLarge { len: 20_000_000, cap: 16_777_216 }),
                _ => Ok(Some(Response::Pong.to_bytes().unwrap())),
            }
        }
    }

    /// The bug this guards: an over-cap reply used to end the pump, so a single
    /// too-large transcript reported itself as a dropped link and took every live
    /// subscription on the connection with it. The failure must land on the one
    /// call that caused it, and the connection must keep serving.
    #[test]
    fn an_oversize_reply_fails_one_call_without_ending_the_connection() {
        let (handle, pump, _events, _terminals, _sessions, _shutdown) =
            demux(Arc::new(OversizeThenHealthy::new()));

        let calls = async {
            // The doomed call: named for what happened, not as a lost link.
            let first = handle.call(Request::Ping).await;
            assert!(
                matches!(first, Err(SessionError::OversizeReply { len, cap })
                    if len == 20_000_000 && cap == 16_777_216),
                "the caller learns its reply was too large, got {first:?}",
            );
            // The connection survived: the very next RPC is answered.
            let second = handle.call(Request::Ping).await;
            assert_eq!(second.unwrap(), Response::Pong, "the connection still serves");
        };

        // `select`, not `join`: the pump only stops when its owner drops the
        // shutdown sender, so waiting for it here would hang after the assertions
        // have already been made.
        let outcome = block_on(select(Box::pin(pump.run()), Box::pin(calls)));
        assert!(
            matches!(outcome, Either::Right(_)),
            "the calls finished, meaning the pump never ended the connection",
        );
    }
}
