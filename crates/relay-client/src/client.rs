use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use oximux_relay_proto::{Frame, Hello, Notification, PROTOCOL_VERSION, Request, Response};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::codec::{CodecError, read_frame, write_frame};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    #[error("daemon error ({code:?}): {message}")]
    Daemon {
        code: oximux_relay_proto::ErrCode,
        message: String,
    },
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
    #[error("daemon closed the connection")]
    Disconnected,
    #[error("daemon did not respond within {0:?}")]
    Timeout(Duration),
    #[error("handshake failed: {0}")]
    Handshake(String),
}

// Upper bound on how long a synchronous RPC waits for the daemon's
// reply. Control requests (Spawn/Attach/Resize/ListPtys) normally
// complete in well under a second; this ceiling exists only so a
// daemon that is connected but wedged (not replying, not dropping the
// socket) cannot block the caller forever. The sync backend bridge
// runs `request` on the GPUI main thread via `Handle::block_on`, so an
// unbounded wait here freezes the whole UI — the timeout converts that
// into a recoverable error instead.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// Per-PTY subscribers. A `Vec` of id-tagged Senders, NOT a single Sender,
// because more than one local session can attach the SAME daemon PTY at once
// (e.g. a main-pane tab and a floating terminal both restoring the same
// external id, or a reconnect racing the old session's teardown). With a
// single Sender per pty_id the second `subscribe` silently overwrote the
// first, orphaning the earlier pane's output pump: it kept rendering the
// replayed scrollback but went deaf to all live output — typing reached the
// shell yet nothing echoed back, so the pane looked frozen. Each subscription
// carries a unique id so teardown removes only its own Sender. Unbounded
// because the daemon's fan_out already back-pressures at the socket and the
// client must surface every byte to the renderer.
type SubId = u64;
type PtySubscribers = Arc<DashMap<String, Vec<(SubId, mpsc::UnboundedSender<Notification>)>>>;

pub struct RelayClient {
    write_tx: mpsc::Sender<Frame>,
    pending: Arc<DashMap<u64, oneshot::Sender<Response>>>,
    pty_subscribers: PtySubscribers,
    next_sub_id: AtomicU64,
    next_request_id: AtomicU64,
    // Daemon's `HelloAck.session_id`. Persisted alongside every PTY id
    // so phase-06 reconciliation can detect "daemon restarted" with a
    // single string comparison instead of N PtyNotFound round trips.
    server_session_id: String,
    _reader_task: JoinHandle<()>,
    _writer_task: JoinHandle<()>,
}

impl RelayClient {
    pub async fn connect(socket_path: &Path, token: &str) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket_path).await?;
        let (mut read_half, mut write_half) = stream.into_split();

        // --- Handshake (must complete before the reader task starts
        //     consuming response frames) ----------------------------
        let client_id = Uuid::new_v4().to_string();
        let hello = Frame::Request {
            request_id: 0,
            request: Request::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                token: token.to_owned(),
                client_id,
            }),
        };
        write_frame(&mut write_half, &hello).await?;

        let mut buf = Vec::with_capacity(4 * 1024);
        let ack = read_frame(&mut read_half, &mut buf).await?;
        let server_session_id = match ack {
            Frame::Response {
                request_id: 0,
                response: Response::HelloAck(ack),
            } => ack.session_id,
            Frame::Response {
                response: Response::Err { code, message },
                ..
            } => {
                return Err(ClientError::Daemon { code, message });
            }
            other => {
                return Err(ClientError::Handshake(format!(
                    "expected HelloAck, got {other:?}"
                )));
            }
        };

        // --- Long-lived I/O tasks -----------------------------------
        let pending: Arc<DashMap<u64, oneshot::Sender<Response>>> = Arc::new(DashMap::new());
        let pty_subscribers: PtySubscribers = Arc::new(DashMap::new());
        // Larger queue: keystroke writes use the sync `try_send_oneway`
        // path so we want enough headroom to absorb a long paste while
        // the writer task flushes to UDS. 4096 frames at ~64B/frame is
        // ~256 KiB of worst-case buffering — negligible.
        let (write_tx, mut write_rx) = mpsc::channel::<Frame>(4096);

        let writer_task = tokio::spawn(async move {
            while let Some(frame) = write_rx.recv().await {
                if write_frame(&mut write_half, &frame).await.is_err() {
                    break;
                }
            }
        });

        let pending_for_reader = Arc::clone(&pending);
        let subs_for_reader = Arc::clone(&pty_subscribers);
        let reader_task = tokio::spawn(async move {
            reader_loop(read_half, buf, pending_for_reader, subs_for_reader).await;
        });

        Ok(Self {
            write_tx,
            pending,
            pty_subscribers,
            next_sub_id: AtomicU64::new(1),
            next_request_id: AtomicU64::new(1),
            server_session_id,
            _reader_task: reader_task,
            _writer_task: writer_task,
        })
    }

    pub fn server_session_id(&self) -> &str {
        &self.server_session_id
    }

    pub async fn request(&self, request: Request) -> Result<Response, ClientError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(request_id, tx);

        let frame = Frame::Request {
            request_id,
            request,
        };
        if self.write_tx.send(frame).await.is_err() {
            self.pending.remove(&request_id);
            return Err(ClientError::Disconnected);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            // Sender dropped without a value — reader_loop cleared
            // `pending` on disconnect.
            Ok(Err(_)) => Err(ClientError::Disconnected),
            // Daemon is still connected but never answered. Drop our
            // pending slot so a late reply is discarded as an unknown
            // request_id rather than resolving a stale awaiter, and
            // surface a recoverable error instead of hanging.
            Err(_elapsed) => {
                self.pending.remove(&request_id);
                Err(ClientError::Timeout(REQUEST_TIMEOUT))
            }
        }
    }

    // Fire-and-forget send, synchronous. Skips both the pending oneshot
    // AND the tokio Handle::block_on bridge — keystroke writes called
    // from the GPUI render/input thread must not pay async runtime
    // overhead per character. `try_send` is lock-free; on Full we log
    // and drop (queue is 4096 frames, only reachable if UDS is hung,
    // in which case the daemon is already toast). The daemon emits a
    // Response::Ok we never pair with a pending entry; the reader_loop
    // drops it at trace level.
    pub fn try_send_oneway(&self, request: Request) -> Result<(), ClientError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let frame = Frame::Request {
            request_id,
            request,
        };
        match self.write_tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("relay write queue full; dropping frame");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(ClientError::Disconnected),
        }
    }

    // Register a per-PTY notification stream. Returns a unique `SubId` plus a
    // Receiver that gets every Output / Exit frame for `pty_id` until the
    // matching `unsubscribe_pty(pty_id, sub_id)` call or until the daemon
    // disconnects. Multiple live subscriptions per PTY coexist (see
    // `PtySubscribers`); the id lets teardown target exactly one.
    pub fn subscribe_pty(&self, pty_id: &str) -> (SubId, mpsc::UnboundedReceiver<Notification>) {
        let sub_id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        let rx = subscribe_into(&self.pty_subscribers, sub_id, pty_id);
        (sub_id, rx)
    }

    pub fn unsubscribe_pty(&self, pty_id: &str, sub_id: SubId) {
        unsubscribe_from(&self.pty_subscribers, pty_id, sub_id);
    }
}

/// Append a subscriber Sender for `pty_id`. Split out from the method so the
/// fan-out bookkeeping can be unit-tested without a live socket.
fn subscribe_into(
    subscribers: &PtySubscribers,
    sub_id: SubId,
    pty_id: &str,
) -> mpsc::UnboundedReceiver<Notification> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut entry = subscribers.entry(pty_id.to_owned()).or_default();
    entry.push((sub_id, tx));
    // Coexisting subscribers on one PTY are now handled, but they're still
    // unusual — surface it so a stuck "shows content, can't type" pane can be
    // traced to the attachment that would previously have been clobbered.
    if entry.len() > 1 {
        tracing::debug!(pty_id, subscribers = entry.len(), "multiple attachments on one PTY");
    }
    rx
}

/// Remove exactly the `sub_id` subscriber from `pty_id`, leaving any sibling
/// attachments on the same PTY untouched, and reap the key once its last
/// subscriber is gone. `remove_if` re-checks emptiness under the shard lock,
/// so a `subscribe` that raced in between is never clobbered.
fn unsubscribe_from(subscribers: &PtySubscribers, pty_id: &str, sub_id: SubId) {
    if let Some(mut subs) = subscribers.get_mut(pty_id) {
        subs.retain(|(id, _)| *id != sub_id);
    }
    subscribers.remove_if(pty_id, |_, subs| subs.is_empty());
}

async fn reader_loop(
    mut read_half: tokio::net::unix::OwnedReadHalf,
    mut buf: Vec<u8>,
    pending: Arc<DashMap<u64, oneshot::Sender<Response>>>,
    subscribers: PtySubscribers,
) {
    loop {
        let frame = match read_frame(&mut read_half, &mut buf).await {
            Ok(f) => f,
            Err(CodecError::Eof) => {
                tracing::info!("relay closed connection");
                break;
            }
            Err(e) => {
                tracing::warn!(?e, "relay reader error");
                break;
            }
        };
        match frame {
            Frame::Response {
                request_id,
                response,
            } => {
                if let Some((_, tx)) = pending.remove(&request_id) {
                    let _ = tx.send(response);
                } else {
                    // Expected for fire-and-forget writes (every
                    // keystroke). Keep at trace level so a debug-level
                    // session doesn't pay format-string + I/O cost
                    // for what is now normal traffic.
                    tracing::trace!(request_id, "response for unknown request_id");
                }
            }
            Frame::Notification(n) => fan_to_subscriber(&subscribers, n),
            Frame::Request { request_id, .. } => {
                tracing::warn!(request_id, "daemon sent us a Request frame, ignoring");
            }
        }
    }
    // Cleanup: drop every pending oneshot so awaiters return
    // `Disconnected`, and drop every subscriber sender so per-pty
    // pump tasks exit naturally.
    pending.clear();
    subscribers.clear();
}

fn fan_to_subscriber(subscribers: &PtySubscribers, notif: Notification) {
    let pty_id = match &notif {
        Notification::Output { pty_id, .. }
        | Notification::Exit { pty_id, .. }
        | Notification::Attention { pty_id, .. } => pty_id.clone(),
    };
    let had_entry = if let Some(mut subs) = subscribers.get_mut(&pty_id) {
        // Deliver to EVERY attachment on this PTY, dropping any whose
        // receiver has gone away. Cloning per-subscriber is cheap next to
        // the socket read that produced this frame, and there is rarely
        // more than one. The RefMut is released at the end of this block
        // so the `remove_if` below can take the shard lock.
        subs.retain(|(_, tx)| tx.send(notif.clone()).is_ok());
        true
    } else {
        false
    };
    if had_entry {
        // Reap the key if that send emptied it (all receivers dropped).
        subscribers.remove_if(&pty_id, |_, subs| subs.is_empty());
    } else {
        tracing::debug!(pty_id, "notification for unknown pty");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(pty: &str, bytes: &[u8]) -> Notification {
        Notification::Output {
            pty_id: pty.to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    fn fresh_subs() -> PtySubscribers {
        Arc::new(DashMap::new())
    }

    // Two local sessions attaching the SAME daemon PTY must BOTH receive its
    // output. The previous single-Sender map overwrote the first subscriber,
    // so an earlier still-rendered pane went deaf to live output while input
    // kept reaching the shell — i.e. "shows content, can't type".
    #[test]
    fn two_attachments_to_same_pty_both_receive_output() {
        let subs = fresh_subs();
        let mut a = subscribe_into(&subs, 1, "pty-X");
        let mut b = subscribe_into(&subs, 2, "pty-X");

        fan_to_subscriber(&subs, output("pty-X", b"hi"));

        assert!(matches!(a.try_recv(), Ok(Notification::Output { .. })));
        assert!(matches!(b.try_recv(), Ok(Notification::Output { .. })));
    }

    // Tearing down one attachment must leave its PTY siblings live. The old
    // unsubscribe removed the whole pty_id entry, killing every co-attached
    // pane's output stream.
    #[test]
    fn unsubscribe_one_leaves_sibling_live() {
        let subs = fresh_subs();
        let mut a = subscribe_into(&subs, 1, "pty-X");
        let mut b = subscribe_into(&subs, 2, "pty-X");

        unsubscribe_from(&subs, "pty-X", 1);
        fan_to_subscriber(&subs, output("pty-X", b"x"));

        assert!(a.try_recv().is_err(), "unsubscribed attachment gets nothing");
        assert!(
            matches!(b.try_recv(), Ok(Notification::Output { .. })),
            "sibling on the same PTY stays live"
        );
    }

    // The key is reaped once its last subscriber leaves, so a later attach to
    // the same id does not accrete a stale Vec.
    #[test]
    fn last_unsubscribe_reaps_the_key() {
        let subs = fresh_subs();
        let _a = subscribe_into(&subs, 1, "pty-X");
        unsubscribe_from(&subs, "pty-X", 1);
        assert!(!subs.contains_key("pty-X"));
    }

    // A dropped receiver is pruned on the next fan-out, and emptying the Vec
    // that way also reaps the key.
    #[test]
    fn dropped_receiver_is_pruned_on_fanout() {
        let subs = fresh_subs();
        let a = subscribe_into(&subs, 1, "pty-X");
        drop(a);
        fan_to_subscriber(&subs, output("pty-X", b"gone"));
        assert!(!subs.contains_key("pty-X"));
    }
}
