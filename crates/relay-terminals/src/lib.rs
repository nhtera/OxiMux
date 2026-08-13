//! `remote-host`'s [`TerminalSource`] seam implemented over the relay daemon —
//! shared by both hosts (the desktop app and `oximux serve`), extracted from
//! the desktop for the same reason the relay supervisor was.
//!
//! This is the only place the remote protocol and the PTY daemon meet.
//! `remote-host` deliberately knows nothing about the relay — it holds a trait —
//! so every relay concept (attachment ids, the Unix-socket request/response
//! shape, the notification fan-out) is translated here and nowhere else.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Notification, Request, Response};
use oximux_remote_host::{
    AttachmentId, TerminalAttach, TerminalError, TerminalFrame, TerminalSource,
};
use oximux_remote_proto::messages::TerminalSummary;
use tokio::sync::{mpsc, oneshot};

/// How many terminal frames to buffer per remote attachment.
///
/// Bounded on purpose: a phone on a slow link must not be able to grow the
/// desktop's memory without limit, which is the same reasoning the daemon's own
/// subscriber queue follows. When it fills, the overflow is reported as a gap
/// rather than dropped quietly — the client re-attaches and resyncs.
const FRAME_QUEUE: usize = 256;

/// Terminals served from the relay daemon.
///
/// Nothing here is keyed by PTY, deliberately. One of these is shared by every
/// paired device, so a per-PTY entry would be one device's overwriting
/// another's — and the daemon addresses `Resize`/`Detach` to an *attachment*,
/// because it runs each PTY at the smallest size any attachment asks for. The
/// attachment id therefore rides back to the caller in [`TerminalAttach`] and
/// returns with the resize, so each connection keeps naming the attachment it
/// opened. Attachment ids are unique per daemon, so keying by one is safe where
/// keying by PTY is not.
pub struct RelayTerminals {
    client: Arc<RelayClient>,
    /// One entry per live forwarding task; dropping the sender tells that task
    /// to unwind. See [`TerminalSource::detach`] for why a task cannot be left
    /// to notice on its own.
    releases: Arc<Mutex<HashMap<AttachmentId, oneshot::Sender<()>>>>,
}

impl RelayTerminals {
    pub fn new(client: Arc<RelayClient>) -> Self {
        Self { client, releases: Arc::new(Mutex::new(HashMap::new())) }
    }

    async fn request(&self, req: Request) -> Result<Response, TerminalError> {
        self.client.request(req).await.map_err(|e| {
            // Relay error text can carry socket paths and internal state; it is
            // logged here and never forwarded, matching the git handlers.
            tracing::warn!(error = %e, "relay request failed");
            TerminalError::Unavailable
        })
    }
}

#[async_trait::async_trait]
impl TerminalSource for RelayTerminals {
    async fn list(&self) -> Result<Vec<TerminalSummary>, TerminalError> {
        match self.request(Request::ListPtys).await? {
            Response::PtyList(ptys) => Ok(ptys
                .into_iter()
                .map(|p| TerminalSummary {
                    pty_id: p.pty_id,
                    cwd: p.cwd,
                    cols: p.cols,
                    rows: p.rows,
                })
                .collect()),
            other => {
                tracing::warn!(?other, "unexpected response to ListPtys");
                Err(TerminalError::Unavailable)
            }
        }
    }

    async fn attach(
        &self,
        pty_id: &str,
    ) -> Result<(TerminalAttach, mpsc::Receiver<TerminalFrame>), TerminalError> {
        // Subscribe BEFORE attaching. The daemon starts fanning output to this
        // connection the moment `Attach` returns, and a local subscriber that
        // does not exist yet cannot receive it — so the other order has a window
        // where live bytes land between the replay snapshot and the first
        // listener, and vanish. Subscribing first costs nothing: no output is
        // routed to us until the attach lands.
        let (sub_id, mut notifications) = self.client.subscribe_pty(pty_id);

        let attached = match self.request(Request::Attach { pty_id: pty_id.to_owned() }).await {
            Ok(Response::AttachOk { replay, cols, rows, attachment_id }) => {
                // Bind before anything else reads the stream: until the
                // subscription knows its attachment, it receives every copy the
                // daemon fans for this PTY — including the ones addressed to a
                // desktop pane watching the same terminal.
                self.client.bind_attachment(pty_id, sub_id, attachment_id);
                TerminalAttach { replay, cols, rows, attachment: AttachmentId(attachment_id) }
            }
            Ok(Response::Err { code: oximux_relay_proto::ErrCode::PtyNotFound, .. }) => {
                self.client.unsubscribe_pty(pty_id, sub_id);
                return Err(TerminalError::NotFound);
            }
            Ok(other) => {
                self.client.unsubscribe_pty(pty_id, sub_id);
                tracing::warn!(?other, "unexpected response to Attach");
                return Err(TerminalError::Unavailable);
            }
            Err(e) => {
                self.client.unsubscribe_pty(pty_id, sub_id);
                return Err(e);
            }
        };

        let (tx, rx) = mpsc::channel(FRAME_QUEUE);
        let client = Arc::clone(&self.client);
        let owned_pty = pty_id.to_owned();
        // The attachment id travels with the forwarding task: it is what the
        // task must hand back to the daemon when it ends.
        let mine = attached.attachment.0;
        // How `detach` reaches this task. Dropping the sender is the signal, so
        // a caller only has to forget the attachment for the task to unwind.
        let (release_tx, mut release) = oneshot::channel::<()>();
        self.releases.lock().unwrap().insert(attached.attachment, release_tx);
        let releases = Arc::clone(&self.releases);
        tokio::spawn(async move {
            // A gap the remote client has not been told about yet. Same shape as
            // the daemon's own subscriber flag, and for the same reason: the
            // queue being full is what causes the gap, so the notice has to wait
            // for room.
            let mut gapped = false;
            // Waiting on the release alongside the stream is what makes leaving
            // a QUIET terminal work. Notifications are addressed to an
            // attachment, so the moment the daemon stops fanning to this one
            // nothing can arrive here again — a task parked on `recv` alone
            // would never wake, never unsubscribe, and never hand the
            // attachment back. The terminal would keep honouring the size vote
            // of a viewer that had already gone.
            loop {
                let n = tokio::select! {
                    // Biased so an already-pending release wins over a queued
                    // frame: the client is no longer rendering this terminal.
                    biased;
                    _ = &mut release => break,
                    n = notifications.recv() => match n {
                        Some(n) => n,
                        None => break,
                    },
                };
                let frame = match n {
                    Notification::Output { bytes, .. } => TerminalFrame::Output(bytes),
                    Notification::Exit { code, .. } => {
                        // Deliver the exit with `send` rather than `try_send`: it
                        // is the last frame this terminal will ever produce, and
                        // dropping it would leave the phone rendering a dead
                        // shell as live. Waiting is safe — the loop ends next.
                        let _ = tx.send(TerminalFrame::Exited(code)).await;
                        break;
                    }
                    // The daemon dropped output destined for US. Forward it: the
                    // remote client's re-attach is what recovers the missed bytes.
                    Notification::Gapped { .. } => TerminalFrame::Gapped,
                    // Attention is a desktop pane signal with no terminal-screen
                    // meaning; the phone renders bytes, not pane decorations.
                    Notification::Attention { .. } => continue,
                };
                if gapped {
                    match tx.try_send(TerminalFrame::Gapped) {
                        Ok(()) => gapped = false,
                        Err(mpsc::error::TrySendError::Full(_)) => continue,
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
                match tx.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(pty_id = %owned_pty, "remote terminal queue full; dropping output");
                        gapped = true;
                    }
                    // The remote client detached (its receiver dropped).
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            client.unsubscribe_pty(&owned_pty, sub_id);
            // Retire the release handle. Harmless if `detach` already took it —
            // that is the path that woke this task.
            releases.lock().unwrap().remove(&AttachmentId(mine));
            // Release the daemon's attachment too, not just our local subscriber.
            //
            // These are two different things and only one of them used to be
            // cleaned up. The daemon fans output out once per *attachment*, and
            // sizes the PTY to the `min` across them — so an attachment that is
            // never released keeps a share of both. A phone that opens the same
            // terminal N times therefore made the host send N copies of every
            // byte (one keystroke echoing back N times), and pinned the PTY to
            // the smallest grid any of those dead attachments had asked for,
            // which survived the phone disconnecting and even the app being
            // killed — nothing was left that could ever release it.
            //
            // Best-effort: the connection may already be gone, in which case the
            // daemon reaps the attachment with the connection anyway.
            let _ =
                client.request(Request::Detach { pty_id: owned_pty, attachment_id: mine }).await;
        });

        Ok((attached, rx))
    }

    async fn input(&self, pty_id: &str, bytes: &[u8]) -> Result<(), TerminalError> {
        match self
            .request(Request::Write { pty_id: pty_id.to_owned(), bytes: bytes.to_vec() })
            .await?
        {
            Response::Ok => Ok(()),
            Response::Err { code: oximux_relay_proto::ErrCode::PtyNotFound, .. } => {
                Err(TerminalError::NotFound)
            }
            other => {
                tracing::warn!(?other, "unexpected response to Write");
                Err(TerminalError::Unavailable)
            }
        }
    }

    async fn resize(
        &self,
        pty_id: &str,
        attachment: AttachmentId,
        cols: u16,
        rows: u16,
    ) -> Result<(), TerminalError> {
        match self
            .request(Request::Resize {
                pty_id: pty_id.to_owned(),
                attachment_id: attachment.0,
                cols,
                rows,
            })
            .await?
        {
            Response::Ok => Ok(()),
            Response::Err { code: oximux_relay_proto::ErrCode::PtyNotFound, .. } => {
                Err(TerminalError::NotFound)
            }
            other => {
                tracing::warn!(?other, "unexpected response to Resize");
                Err(TerminalError::Unavailable)
            }
        }
    }

    async fn detach(&self, pty_id: &str, attachment: AttachmentId) {
        // Dropping the release handle is the whole signal: the forwarding task
        // is waiting on it, and unwinds by unsubscribing and handing the
        // attachment back to the daemon. Doing the work there rather than here
        // keeps one owner of the teardown, so a detach racing a task that is
        // already ending cannot send `Detach` twice or unsubscribe a
        // subscription that has moved on.
        //
        // Absent means already released — an idempotent no-op, as the trait
        // requires. A caller on a teardown path should not have to know whether
        // it is the first one through.
        if self.releases.lock().unwrap().remove(&attachment).is_none() {
            tracing::debug!(pty_id, id = attachment.0, "detach for an attachment already released");
        }
    }
}

