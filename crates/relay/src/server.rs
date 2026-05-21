use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use oximux_relay_proto::{
    ErrCode, Frame, HelloAck, Notification, PROTOCOL_VERSION, Request, Response,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::codec::{CodecError, read_frame, write_frame};
use crate::registry::{PtyRegistry, RegistryError, SUBSCRIBER_QUEUE, SpawnArgs};

pub struct ServerConfig {
    pub socket_path: PathBuf,
    pub token_file: PathBuf,
}

// Drop guard: removes the bound socket path on drop so a crashed
// relay can be replaced by a fresh spawn without manual cleanup.
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn run_server(cfg: ServerConfig) -> Result<()> {
    let token = std::fs::read_to_string(&cfg.token_file)
        .with_context(|| format!("read token file: {}", cfg.token_file.display()))?
        .trim()
        .to_owned();
    if token.is_empty() {
        bail!("token file is empty");
    }

    // Best-effort: clean a stale socket left by a previous crash. If
    // the path is held by a *live* daemon, bind() will fail loudly.
    let _ = std::fs::remove_file(&cfg.socket_path);
    let listener = UnixListener::bind(&cfg.socket_path)
        .with_context(|| format!("bind {}", cfg.socket_path.display()))?;
    let _guard = SocketGuard(cfg.socket_path.clone());

    let registry = Arc::new(PtyRegistry::new());
    let session_id = Uuid::new_v4().to_string();
    tracing::info!(session_id, "relay listening");

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(?e, "accept failed");
                continue;
            }
        };
        let registry = Arc::clone(&registry);
        let token = token.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            if let Err(e) = session_loop(stream, registry, token, session_id).await {
                tracing::debug!(?e, "session ended");
            }
        });
    }
}

// Per-connection logic. Hello first; reject anything else until a
// valid Hello arrives. After Hello, dispatch Request → Response and
// forward subscriber Notifications to the outbound writer.
async fn session_loop(
    stream: UnixStream,
    registry: Arc<PtyRegistry>,
    token: String,
    session_id: String,
) -> Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();
    let mut buf = Vec::with_capacity(8 * 1024);

    // === Hello handshake (must be the first frame) ============================
    let first = read_frame(&mut read_half, &mut buf).await?;
    let Frame::Request {
        request_id,
        request,
    } = first
    else {
        bail!("first frame from client was not a request");
    };
    let Request::Hello(hello) = request else {
        let resp = err_response(request_id, ErrCode::AuthFailed, "expected Hello first");
        let _ = write_frame(&mut write_half, &resp).await;
        bail!("first request from client was not Hello");
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        let resp = err_response(
            request_id,
            ErrCode::VersionMismatch,
            &format!(
                "server protocol {PROTOCOL_VERSION}, client {}",
                hello.protocol_version
            ),
        );
        let _ = write_frame(&mut write_half, &resp).await;
        bail!("version mismatch");
    }
    if hello.token != token {
        let resp = err_response(request_id, ErrCode::AuthFailed, "bad token");
        let _ = write_frame(&mut write_half, &resp).await;
        bail!("bad token");
    }
    let ack = Frame::Response {
        request_id,
        response: Response::HelloAck(HelloAck {
            server_protocol_version: PROTOCOL_VERSION,
            session_id: session_id.clone(),
        }),
    };
    write_frame(&mut write_half, &ack).await?;
    tracing::debug!(client_id = hello.client_id, "client authenticated");

    // === Outbound writer task ================================================
    // Bounded to keep a slow client from ballooning daemon RSS. On
    // full, the pump's `send().await` provides natural back-pressure
    // up the chain to the subscriber's `try_send` in `fan_out`.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Frame>(SUBSCRIBER_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            if write_frame(&mut write_half, &frame).await.is_err() {
                break;
            }
        }
    });

    // === Per-session subscriber pump =========================================
    // Sessions register `notif_tx` clones with the registry when
    // attaching. This pump forwards every Notification into the
    // outbound writer as a Frame::Notification.
    let (notif_tx, mut notif_rx) = mpsc::channel::<Notification>(SUBSCRIBER_QUEUE);
    let outbound_for_pump = outbound_tx.clone();
    let pump = tokio::spawn(async move {
        while let Some(n) = notif_rx.recv().await {
            if outbound_for_pump.send(Frame::Notification(n)).await.is_err() {
                break;
            }
        }
    });

    // === Request dispatch loop ===============================================
    let dispatch_result = dispatch_loop(
        &mut read_half,
        &mut buf,
        &registry,
        &outbound_tx,
        &notif_tx,
    )
    .await;

    // Drop outbound senders so the writer + pump drain naturally.
    drop(outbound_tx);
    drop(notif_tx);
    let _ = writer.await;
    let _ = pump.await;

    dispatch_result
}

async fn dispatch_loop(
    read_half: &mut (impl tokio::io::AsyncRead + Unpin),
    buf: &mut Vec<u8>,
    registry: &Arc<PtyRegistry>,
    outbound_tx: &mpsc::Sender<Frame>,
    notif_tx: &mpsc::Sender<Notification>,
) -> Result<()> {
    loop {
        let frame = match read_frame(read_half, buf).await {
            Ok(f) => f,
            Err(CodecError::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let Frame::Request {
            request_id,
            request,
        } = frame
        else {
            tracing::debug!("non-request frame from client, dropping");
            continue;
        };

        let response = handle_request(registry, notif_tx, request).await;
        let frame = Frame::Response {
            request_id,
            response,
        };
        if outbound_tx.send(frame).await.is_err() {
            return Ok(());
        }
    }
}

async fn handle_request(
    registry: &Arc<PtyRegistry>,
    notif_tx: &mpsc::Sender<Notification>,
    request: Request,
) -> Response {
    match request {
        Request::Hello(_) => Response::Err {
            code: ErrCode::AuthFailed,
            message: "Hello already completed".into(),
        },
        Request::Spawn {
            cwd,
            cols,
            rows,
            shell,
            env,
        } => match registry.spawn(SpawnArgs {
            cwd: PathBuf::from(cwd),
            cols,
            rows,
            shell,
            env,
        }) {
            Ok(pty_id) => {
                // Auto-attach the spawning session so Output frames
                // start flowing without a separate Attach round trip
                // (matches user mental model: "I asked for this PTY,
                // I want to hear from it"). The replay buffer is
                // empty at this point so we discard the Vec.
                let _ = registry.attach(&pty_id, notif_tx.clone());
                Response::SpawnOk { pty_id }
            }
            Err(e) => err_from(&e),
        },
        Request::Attach { pty_id } => match registry.attach(&pty_id, notif_tx.clone()) {
            Ok(replay) => Response::AttachOk { replay },
            Err(e) => err_from(&e),
        },
        Request::Write { pty_id, bytes } => match registry.write(&pty_id, &bytes) {
            Ok(()) => Response::Ok,
            Err(e) => err_from(&e),
        },
        Request::Resize { pty_id, cols, rows } => match registry.resize(&pty_id, cols, rows) {
            Ok(()) => Response::Ok,
            Err(e) => err_from(&e),
        },
        Request::Close { pty_id, grace_ms } => {
            match registry
                .close(&pty_id, Duration::from_millis(grace_ms as u64))
                .await
            {
                Ok(()) => Response::Ok,
                Err(e) => err_from(&e),
            }
        }
        Request::ListPtys => Response::PtyList(registry.list()),
        Request::Shutdown => {
            // v1: cooperative-only — refuse if anything is alive,
            // otherwise the daemon stays up. True shutdown wiring
            // (oneshot to break the accept loop) is phase-04 polish.
            if registry.live_count() > 0 {
                Response::Err {
                    code: ErrCode::Internal,
                    message: "live PTYs present; refusing shutdown".into(),
                }
            } else {
                Response::Ok
            }
        }
    }
}

fn err_from(e: &RegistryError) -> Response {
    Response::Err {
        code: e.err_code(),
        message: e.to_string(),
    }
}

fn err_response(request_id: u64, code: ErrCode, message: &str) -> Frame {
    Frame::Response {
        request_id,
        response: Response::Err {
            code,
            message: message.into(),
        },
    }
}
