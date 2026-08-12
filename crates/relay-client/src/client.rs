use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};
use oximux_relay_proto::{
    Endpoint, Frame, Hello, HelloProof, NONCE_LEN, Nonce, Notification, PROTOCOL_VERSION, Request,
    Response, client_proof, endpoint_for, proofs_match, server_proof,
};
use rand::RngCore;
use rand::rngs::OsRng;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
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

/// One local subscription to a daemon PTY.
///
/// `attachment_id` is set by [`RelayClient::bind_attachment`] once `AttachOk`
/// names it, and is what makes routing exact: the daemon sends one copy of
/// every notification per attachment, so a connection holding two attachments
/// to one PTY receives two identical frames and must give each to the
/// subscriber it belongs to. Delivering both to both is how every byte ended up
/// rendered twice.
struct PtySub {
    sub_id: SubId,
    /// `None` until `AttachOk` lands — see [`fan_to_subscriber`] for why that
    /// window still has to receive.
    attachment_id: Option<u64>,
    tx: mpsc::UnboundedSender<Notification>,
}

type PtySubscribers = Arc<DashMap<String, Vec<PtySub>>>;

/// Open the byte stream to the daemon and hand back its two halves. The only
/// platform-specific step in the client — everything above it is framing.
///
/// Splitting here rather than in the caller is deliberate: this yields two
/// independently-owned halves with no shared lock, while the generic
/// `tokio::io::split` puts a `BiLock` between reader and writer. This socket
/// carries every keystroke one way and every byte of PTY output the other, so
/// that lock would sit on the hottest path in the app.
async fn dial(socket_path: &Path) -> Result<(RecvHalf, SendHalf), ClientError> {
    let name = match endpoint_for(socket_path) {
        Endpoint::FsPath(path) => path.to_fs_name::<GenericFilePath>()?,
        Endpoint::Namespaced(name) => name.to_ns_name::<GenericNamespaced>()?,
    };
    Ok(Stream::connect(name).await?.split())
}

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
    /// Dial the daemon's local socket and complete the handshake.
    ///
    /// Only the dial is platform-specific; everything after it is framing over
    /// a byte stream, so [`Self::handshake`] takes the connected halves rather
    /// than the path. That is the seam a named-pipe transport plugs into.
    pub async fn connect(socket_path: &Path, token: &str) -> Result<Self, ClientError> {
        let (read_half, write_half) = dial(socket_path).await?;
        Self::handshake(read_half, write_half, token).await
    }

    /// Handshake and start the reader/writer tasks over an already-connected
    /// pair of stream halves.
    async fn handshake<R, W>(
        mut read_half: R,
        mut write_half: W,
        token: &str,
    ) -> Result<Self, ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {

        // --- Handshake (must complete before the reader task starts
        //     consuming response frames) ----------------------------
        // The token is never sent. Each side proves it holds it by MACing both
        // nonces; see `oximux_relay_proto::auth` for what that buys and why the
        // daemon goes first.
        let client_id = Uuid::new_v4().to_string();
        let mut client_nonce: Nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut client_nonce);
        let hello = Frame::Request {
            request_id: 0,
            request: Request::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                client_id,
                client_nonce,
            }),
        };
        write_frame(&mut write_half, &hello).await?;

        let mut buf = Vec::with_capacity(4 * 1024);
        let challenge = read_frame(&mut read_half, &mut buf).await?;
        let challenge = match challenge {
            Frame::Response {
                request_id: 0,
                response: Response::HelloChallenge(c),
            } => c,
            Frame::Response {
                response: Response::Err { code, message },
                ..
            } => {
                return Err(ClientError::Daemon { code, message });
            }
            other => {
                return Err(ClientError::Handshake(format!(
                    "expected HelloChallenge, got {other:?}"
                )));
            }
        };

        // Whoever answered has to prove it can read the token file before we
        // say anything else. Failing here means the endpoint is held by
        // something that is not our daemon — on Windows, a squatted pipe name —
        // so we abandon the connection rather than hand it a session.
        let expected = server_proof(token, &challenge.server_nonce, &client_nonce);
        if !proofs_match(&challenge.server_proof, &expected) {
            return Err(ClientError::Handshake(
                "daemon could not prove it holds the token — endpoint may be impersonated".into(),
            ));
        }

        let proof = Frame::Request {
            request_id: 1,
            request: Request::HelloProof(HelloProof {
                client_proof: client_proof(token, &challenge.server_nonce, &client_nonce),
            }),
        };
        write_frame(&mut write_half, &proof).await?;

        let ack = read_frame(&mut read_half, &mut buf).await?;
        let server_session_id = match ack {
            Frame::Response {
                request_id: 1,
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

    /// Name the attachment a subscription belongs to, once `AttachOk` reports
    /// it.
    ///
    /// Until this is called the subscription is unbound and receives anything
    /// on the PTY that no bound subscriber claims — the daemon can fan output
    /// before the attach response reaches the caller. After it, the
    /// subscription receives exactly its own attachment's copies, which is what
    /// stops a second attachment on the same connection from doubling the
    /// stream.
    pub fn bind_attachment(&self, pty_id: &str, sub_id: SubId, attachment_id: u64) {
        if let Some(mut subs) = self.pty_subscribers.get_mut(pty_id)
            && let Some(sub) = subs.iter_mut().find(|s| s.sub_id == sub_id)
        {
            sub.attachment_id = Some(attachment_id);
        }
    }

    pub fn unsubscribe_pty(&self, pty_id: &str, sub_id: SubId) {
        unsubscribe_from(&self.pty_subscribers, pty_id, sub_id);
    }
}

impl Drop for RelayClient {
    /// Close the inbound half explicitly instead of relying on the daemon to
    /// hang up first.
    ///
    /// Dropping `write_tx` retires the writer task once it has drained, and on
    /// a unix socket that alone is enough: dropping the write half shuts down
    /// the write direction, the daemon reads EOF and ends the session, and the
    /// reader here then sees EOF and exits on its own.
    ///
    /// A named pipe has no half-shutdown. Nothing would reach the daemon, its
    /// session would stay parked on a read that never completes, and the reader
    /// task here would block forever holding the pipe open — leaking a task and
    /// a daemon session per client. Aborting the reader drops that half on
    /// every platform, so the handle closes once the writer has finished.
    ///
    /// Only the reader is aborted. The writer is left to drain so frames
    /// already queued still reach the daemon; it stops on its own when the
    /// channel closes.
    fn drop(&mut self) {
        self._reader_task.abort();
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
    entry.push(PtySub { sub_id, attachment_id: None, tx });
    // Coexisting subscribers on one PTY are now handled, but they're still
    // unusual — surface it so a stuck "shows content, can't type" pane can be
    // traced to the attachment that would previously have been clobbered.
    //
    // ⚠️ KNOWN DEFECT while more than one is live. The daemon fans output out
    // once per *attachment*, but `Notification::Output` carries only `pty_id` —
    // so a connection holding two attachments to one PTY (a desktop pane plus a
    // remote peer watching the same terminal) receives two identical
    // notifications and delivers BOTH to every subscriber here. Each one then
    // renders every byte twice. Measured on device 2026-08-13: one keystroke,
    // one attachment minted, two frames forwarded under the same
    // `(attachment_id, sub_id)`.
    //
    // The fix is to route by subscription identity rather than by PTY: carry the
    // attachment id on the notification and deliver to the one subscriber that
    // owns it. That is a relay-proto wire change, so it needs the handshake
    // version bump the daemon and client share.
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
        subs.retain(|sub| sub.sub_id != sub_id);
    }
    subscribers.remove_if(pty_id, |_, subs| subs.is_empty());
}

async fn reader_loop<R: AsyncRead + Unpin>(
    mut read_half: R,
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
        | Notification::Attention { pty_id, .. }
        | Notification::Gapped { pty_id, .. } => pty_id.clone(),
    };
    let addressed_to = notif.attachment_id();
    let had_entry = if let Some(mut subs) = subscribers.get_mut(&pty_id) {
        // Route by attachment, not by PTY.
        //
        // The daemon sends one copy per attachment. Handing every copy to every
        // subscriber is therefore not "belt and braces" — it multiplies the
        // stream by however many attachments this connection holds, and a
        // desktop pane watching the same terminal as a paired phone is enough
        // to make both render every byte twice.
        //
        // An unaddressed notification (`Attention`) is a genuine broadcast and
        // still goes to all.
        let deliver_to_all = match addressed_to {
            None => true,
            // Nothing claims this id yet: the daemon starts fanning the moment
            // `Attach` lands, which can beat the `AttachOk` that tells us our
            // own id. Falling back to the still-unbound subscribers covers
            // exactly that window instead of dropping live bytes on the floor.
            Some(id) => !subs.iter().any(|s| s.attachment_id == Some(id)),
        };
        subs.retain(|sub| {
            let wanted = match addressed_to {
                None => true,
                Some(id) => {
                    sub.attachment_id == Some(id)
                        || (deliver_to_all && sub.attachment_id.is_none())
                }
            };
            if !wanted {
                return true;
            }
            // Drop a subscriber whose receiver has gone away. The RefMut is
            // released at the end of this block so `remove_if` below can take
            // the shard lock.
            sub.tx.send(notif.clone()).is_ok()
        });
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
            attachment_id: 0,
            pty_id: pty.to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    fn output_for(pty: &str, attachment_id: u64, bytes: &[u8]) -> Notification {
        Notification::Output {
            attachment_id,
            pty_id: pty.to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    fn bind(subs: &PtySubscribers, pty_id: &str, sub_id: SubId, attachment_id: u64) {
        if let Some(mut list) = subs.get_mut(pty_id)
            && let Some(s) = list.iter_mut().find(|s| s.sub_id == sub_id)
        {
            s.attachment_id = Some(attachment_id);
        }
    }

    fn fresh_subs() -> PtySubscribers {
        Arc::new(DashMap::new())
    }

    // Two local sessions attaching the SAME daemon PTY must BOTH receive its
    // output. The previous single-Sender map overwrote the first subscriber,
    // so an earlier still-rendered pane went deaf to live output while input
    // kept reaching the shell — i.e. "shows content, can't type".
    //
    // Unbound here on purpose: before `AttachOk` names an attachment, a
    // subscription has no address and must still receive.
    #[test]
    fn two_attachments_to_same_pty_both_receive_output() {
        let subs = fresh_subs();
        let mut a = subscribe_into(&subs, 1, "pty-X");
        let mut b = subscribe_into(&subs, 2, "pty-X");

        fan_to_subscriber(&subs, output("pty-X", b"hi"));

        assert!(matches!(a.try_recv(), Ok(Notification::Output { .. })));
        assert!(matches!(b.try_recv(), Ok(Notification::Output { .. })));
    }

    /// Once bound, each attachment receives ONLY its own copy.
    ///
    /// The daemon sends one per attachment. Giving every copy to every
    /// subscriber is what made a desktop pane and a paired phone watching the
    /// same terminal each render every byte twice.
    #[test]
    fn a_bound_attachment_receives_only_its_own_copy() {
        let subs = fresh_subs();
        let mut pane = subscribe_into(&subs, 1, "pty-X");
        let mut phone = subscribe_into(&subs, 2, "pty-X");
        bind(&subs, "pty-X", 1, 7);
        bind(&subs, "pty-X", 2, 9);

        // The daemon fans one copy per attachment; both arrive on this one
        // connection.
        fan_to_subscriber(&subs, output_for("pty-X", 7, b"hi"));
        fan_to_subscriber(&subs, output_for("pty-X", 9, b"hi"));

        assert!(matches!(pane.try_recv(), Ok(Notification::Output { .. })));
        assert!(pane.try_recv().is_err(), "pane saw the phone's copy too");
        assert!(matches!(phone.try_recv(), Ok(Notification::Output { .. })));
        assert!(phone.try_recv().is_err(), "phone saw the pane's copy too");
    }

    /// The daemon starts fanning the moment `Attach` lands, which can beat the
    /// `AttachOk` that names the attachment. Those bytes must reach the
    /// subscription that is still waiting to learn its id, not be dropped.
    #[test]
    fn output_for_an_unclaimed_attachment_reaches_the_unbound_subscriber() {
        let subs = fresh_subs();
        let mut bound = subscribe_into(&subs, 1, "pty-X");
        let mut pending = subscribe_into(&subs, 2, "pty-X");
        bind(&subs, "pty-X", 1, 7);

        fan_to_subscriber(&subs, output_for("pty-X", 9, b"early"));

        assert!(matches!(pending.try_recv(), Ok(Notification::Output { .. })));
        assert!(bound.try_recv().is_err(), "a bound sub took another's bytes");
    }

    /// Attention is a pane-level signal with no attachment, so it is a genuine
    /// broadcast — every viewer of the PTY should raise it.
    #[test]
    fn attention_still_reaches_every_subscriber() {
        let subs = fresh_subs();
        let mut a = subscribe_into(&subs, 1, "pty-X");
        let mut b = subscribe_into(&subs, 2, "pty-X");
        bind(&subs, "pty-X", 1, 7);
        bind(&subs, "pty-X", 2, 9);

        fan_to_subscriber(
            &subs,
            Notification::Attention {
                pty_id: "pty-X".into(),
                title: "t".into(),
                body: "b".into(),
            },
        );

        assert!(matches!(a.try_recv(), Ok(Notification::Attention { .. })));
        assert!(matches!(b.try_recv(), Ok(Notification::Attention { .. })));
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
