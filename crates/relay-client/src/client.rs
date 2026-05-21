use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use oximux_relay_proto::{
    Frame, Hello, Notification, PROTOCOL_VERSION, Request, Response,
};
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
    #[error("handshake failed: {0}")]
    Handshake(String),
}

// Per-PTY subscriber Sender. Unbounded because the relay's own
// fan_out applies back-pressure at the socket level and the client
// process must surface every byte to the renderer — dropping bytes
// here would silently corrupt the visible terminal.
type PtySubscribers = Arc<DashMap<String, mpsc::UnboundedSender<Notification>>>;

pub struct RelayClient {
    write_tx: mpsc::Sender<Frame>,
    pending: Arc<DashMap<u64, oneshot::Sender<Response>>>,
    pty_subscribers: PtySubscribers,
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
        let (write_tx, mut write_rx) = mpsc::channel::<Frame>(256);

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
        match rx.await {
            Ok(response) => Ok(response),
            Err(_) => Err(ClientError::Disconnected),
        }
    }

    // Register a per-PTY notification stream. Returned Receiver gets
    // every Output / Exit frame for `pty_id` until the matching
    // `unsubscribe_pty` call or until the daemon disconnects.
    pub fn subscribe_pty(&self, pty_id: &str) -> mpsc::UnboundedReceiver<Notification> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.pty_subscribers.insert(pty_id.to_owned(), tx);
        rx
    }

    pub fn unsubscribe_pty(&self, pty_id: &str) {
        self.pty_subscribers.remove(pty_id);
    }
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
                    tracing::debug!(request_id, "response for unknown request_id");
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
        Notification::Output { pty_id, .. } | Notification::Exit { pty_id, .. } => pty_id.clone(),
    };
    if let Some(entry) = subscribers.get(&pty_id) {
        if entry.value().send(notif).is_err() {
            // Receiver dropped — clean up so the next subscribe
            // doesn't race against the stale entry.
            drop(entry);
            subscribers.remove(&pty_id);
        }
    } else {
        tracing::debug!(pty_id, "notification for unknown pty");
    }
}
