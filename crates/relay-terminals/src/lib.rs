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
use oximux_remote_host::{TerminalAttach, TerminalError, TerminalFrame, TerminalSource};
use oximux_remote_proto::messages::TerminalSummary;
use tokio::sync::mpsc;

/// How many terminal frames to buffer per remote attachment.
///
/// Bounded on purpose: a phone on a slow link must not be able to grow the
/// desktop's memory without limit, which is the same reasoning the daemon's own
/// subscriber queue follows. When it fills, the overflow is reported as a gap
/// rather than dropped quietly — the client re-attaches and resyncs.
const FRAME_QUEUE: usize = 256;

/// Terminals served from the relay daemon.
pub struct RelayTerminals {
    client: Arc<RelayClient>,
    /// Attachment ids the daemon minted for us, per PTY.
    ///
    /// Needed because `Resize` is addressed to an *attachment*, not a PTY: the
    /// daemon runs each PTY at the smallest size any attachment asks for, so it
    /// has to know which one is changing its mind.
    attachments: Mutex<HashMap<String, u64>>,
}

impl RelayTerminals {
    pub fn new(client: Arc<RelayClient>) -> Self {
        Self { client, attachments: Mutex::new(HashMap::new()) }
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
                self.attachments.lock().unwrap().insert(pty_id.to_owned(), attachment_id);
                TerminalAttach { replay, cols, rows }
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
        tokio::spawn(async move {
            // A gap the remote client has not been told about yet. Same shape as
            // the daemon's own subscriber flag, and for the same reason: the
            // queue being full is what causes the gap, so the notice has to wait
            // for room.
            let mut gapped = false;
            while let Some(n) = notifications.recv().await {
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

    async fn resize(&self, pty_id: &str, cols: u16, rows: u16) -> Result<(), TerminalError> {
        // Without an attachment id there is nothing to resize: the daemon tracks
        // requested sizes per attachment. A resize before attach is a client bug,
        // reported as "no such terminal" rather than silently resizing whatever
        // attachment happened to be first.
        let Some(attachment_id) = self.attachments.lock().unwrap().get(pty_id).copied() else {
            return Err(TerminalError::NotFound);
        };
        match self
            .request(Request::Resize { pty_id: pty_id.to_owned(), attachment_id, cols, rows })
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
}

