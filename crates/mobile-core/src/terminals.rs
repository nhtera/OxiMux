//! Terminal attach over the FFI: the RPC methods, and the pump that forwards
//! pushed frames to the app's [`TerminalSink`].
//!
//! The core stays out of terminal *rendering* entirely — it forwards raw bytes
//! and the app feeds them to a real emulator. Keeping a parsed grid on this side
//! would mean shipping a second emulator and keeping it in sync with the one the
//! app already draws with, for no gain.

use std::sync::Arc;

use futures::StreamExt;
use oximux_remote_session::{TerminalPush, TerminalStream};

use crate::callbacks::TerminalSink;
use crate::client::{MobileClient, Shared};
use crate::ffi_types::{MobileError, TerminalInfo, TerminalScreen};

/// Forward pushed terminal frames to the registered sink until the stream ends
/// (the connection closed).
///
/// The sink is read per frame rather than captured once: the app may register or
/// replace it at any point, including after frames have started arriving, and a
/// captured `None` would silently swallow every frame for the life of the
/// connection.
pub(crate) async fn run_terminal_pump(shared: Arc<Shared>, mut pushes: TerminalStream) {
    while let Some(push) = pushes.next().await {
        let sink = shared.terminal_sink.lock().unwrap().clone();
        let Some(sink) = sink else { continue };
        match push {
            TerminalPush::Output { pty_id, bytes } => sink.on_output(pty_id, bytes),
            TerminalPush::Gapped { pty_id } => sink.on_gap(pty_id),
            TerminalPush::Exited { pty_id, code } => {
                // The terminal is gone, so nothing is left to restore on a
                // reconnect. Forgetting it here keeps a dead pty from being
                // re-attached on every redial for the life of the connection.
                shared.attached.lock().unwrap().remove(&pty_id);
                sink.on_exit(pty_id, code);
            }
        }
    }
}

/// Tell the app to re-attach every terminal it was watching before a reconnect.
///
/// Sessions resume from a fold cursor; terminals cannot — there is no seq to
/// resume from, and the host rebuilt its attachment map empty when it accepted
/// the new connection. So recovery reuses the gap path, which already exists and
/// is already exercised: the app answers `on_gap` by re-attaching and replacing
/// the screen, which is exactly the right response here too. A reconnect *is* a
/// gap — bytes were missed — so this reports what happened rather than
/// impersonating an unrelated signal.
///
/// The core deliberately does not re-attach on the app's behalf. It would have to
/// deliver the fresh screen through a sink that only carries appendable output,
/// and appending a replay to a screen the WebView still holds would duplicate it
/// rather than replace it.
///
/// Called after the new session is published, since the `on_gap` handler dials
/// straight back in with an `attach` RPC.
pub(crate) fn resync_attached(shared: &Arc<Shared>) {
    let sink = shared.terminal_sink.lock().unwrap().clone();
    let Some(sink) = sink else { return };
    // Snapshot before notifying: the callback re-enters the core to attach, which
    // takes this same lock.
    let ptys: Vec<String> = shared.attached.lock().unwrap().iter().cloned().collect();
    for pty_id in ptys {
        sink.on_gap(pty_id);
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// Register the sink that receives live terminal frames. Replaces any
    /// previous one; pass before attaching so no frame arrives unheard.
    pub fn set_terminal_sink(&self, sink: Arc<dyn TerminalSink>) {
        *self.shared.terminal_sink.lock().unwrap() = Some(sink);
    }

    /// Every terminal the host exposes to this device.
    ///
    /// A device the desktop narrowed to a single agent session gets
    /// `Unauthorized` here rather than an empty list — terminals are not scoped
    /// to a session, so there is nothing for a narrowed device to be shown.
    pub async fn list_terminals(&self) -> Result<Vec<TerminalInfo>, MobileError> {
        let session = self.shared.session()?;
        let rows = session.list_terminals().await.map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(rows.into_iter().map(TerminalInfo::from).collect())
    }

    /// Attach to a terminal and get its current screen.
    ///
    /// Safe and cheap to call again on a terminal already attached — that is
    /// exactly how the app recovers from [`TerminalSink::on_gap`], and the host
    /// serves a fresh snapshot without opening a second stream.
    pub async fn attach_terminal(&self, pty_id: String) -> Result<TerminalScreen, MobileError> {
        let session = self.shared.session()?;
        let attached =
            session.term_attach(&pty_id).await.map_err(|e| MobileError::Rpc(e.to_string()))?;
        // Recorded only on success, so a refused attach (a read-only device, a
        // pty that has since exited) is not resurrected on every later reconnect.
        self.shared.attached.lock().unwrap().insert(pty_id);
        Ok(TerminalScreen {
            replay: attached.replay,
            cols: attached.cols,
            rows: attached.rows,
        })
    }

    /// Send keystrokes. Refused for a device the desktop set to read-only.
    pub async fn send_terminal_input(
        &self,
        pty_id: String,
        bytes: Vec<u8>,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session.term_input(&pty_id, &bytes).await.map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Resize the terminal's grid.
    ///
    /// Shared with every other attachment: the daemon runs the PTY at the
    /// smallest size any attachment requests, so a phone that asks for its own
    /// narrow screen reflows the desktop's window too. Refused for a read-only
    /// device for that reason, not because resizing is dangerous alone.
    pub async fn resize_terminal(
        &self,
        pty_id: String,
        cols: u16,
        rows: u16,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session
            .term_resize(&pty_id, cols, rows)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Stop streaming a terminal. Idempotent.
    pub async fn detach_terminal(&self, pty_id: String) -> Result<(), MobileError> {
        // Forgotten before the RPC, and regardless of how it lands: the app has
        // said it is done with this terminal, so a reconnect must not restore it
        // even if the detach itself fails on a connection that is already going
        // away — which is exactly when a failure here is likely.
        self.shared.attached.lock().unwrap().remove(&pty_id);
        let session = self.shared.session()?;
        session.term_detach(&pty_id).await.map_err(|e| MobileError::Rpc(e.to_string()))
    }
}
