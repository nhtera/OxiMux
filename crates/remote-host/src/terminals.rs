//! The terminal seam: what the dispatcher needs from the desktop's PTY layer,
//! expressed without depending on it.
//!
//! `remote-host` cannot reach the relay directly — the relay client lives in the
//! app, speaks a Unix-socket protocol of its own, and pulling it in here would
//! make this crate untestable without a running daemon. So the dispatcher talks
//! to this trait and the app supplies the implementation, exactly as
//! [`DeviceStore`](crate::auth::DeviceStore) does for device persistence.
//!
//! The shape is deliberately narrow. Terminals are the highest-risk surface on
//! this protocol — bytes into a live shell is arbitrary code execution on the
//! developer's machine — so this exposes listing, attaching, writing, and
//! resizing, and nothing else. There is no path argument anywhere, and no way to
//! spawn or kill a terminal remotely: a phone can drive terminals the desktop
//! user already opened, not create new ones.

use oximux_remote_proto::messages::TerminalSummary;

/// One frame from an attached terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalFrame {
    /// Raw bytes drawn by the terminal's process.
    Output(Vec<u8>),
    /// The host dropped output destined for this attachment. Forwarded rather
    /// than swallowed: a client that keeps rendering after a gap draws a screen
    /// with a hole in it, and nothing downstream can detect that on its own.
    Gapped,
    /// The terminal's process ended.
    Exited(Option<i32>),
}

/// Identifies one attachment to a terminal, minted by the host.
///
/// Opaque here on purpose: this crate does not know how the PTY layer numbers
/// its attachments, only that resizing has to name one. A host may run a single
/// PTY at the smallest grid any of its attachments asked for, so a resize is a
/// statement about *this* viewer's window rather than about the terminal — and
/// with several devices watching one terminal, nothing else in a resize request
/// distinguishes them.
///
/// Never crosses the remote wire. It is minted by the host when a connection
/// attaches and handed back by that same connection, so a client cannot name an
/// attachment belonging to someone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AttachmentId(pub u64);

/// What an attach returns before the live frames start: the replay ring, the
/// dims it was drawn at, and the attachment those belong to.
///
/// The dims are not decoration. Replay bytes carry absolute-position escape
/// sequences that only land correctly in a grid of the size that produced them,
/// which is why the desktop's own attach path returns them too.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalAttach {
    pub replay: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    /// The attachment this call opened — the handle a later resize must name.
    pub attachment: AttachmentId,
}

/// Errors a terminal operation can fail with. Deliberately coarse: the detail a
/// PTY layer produces routinely embeds absolute host paths, and the git handlers
/// already learned not to forward that text to a client.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("no such terminal")]
    NotFound,
    #[error("the terminal host is unavailable")]
    Unavailable,
}

/// The desktop's terminals, as much of them as the remote protocol exposes.
#[async_trait::async_trait]
pub trait TerminalSource: Send + Sync {
    /// Every terminal the host is willing to expose.
    async fn list(&self) -> Result<Vec<TerminalSummary>, TerminalError>;

    /// Attach to a terminal, returning its replay snapshot and a stream of live
    /// frames. Dropping the returned receiver detaches.
    async fn attach(
        &self,
        pty_id: &str,
    ) -> Result<(TerminalAttach, tokio::sync::mpsc::Receiver<TerminalFrame>), TerminalError>;

    /// Send keystrokes. The caller has already checked write scope; this must
    /// not be reachable from a read-only device.
    async fn input(&self, pty_id: &str, bytes: &[u8]) -> Result<(), TerminalError>;

    /// Resize the grid of one attachment.
    ///
    /// The attachment is named explicitly rather than looked up from the PTY,
    /// because a terminal can have several — two paired phones and the desktop's
    /// own window — and each votes on the size separately. Keying that lookup by
    /// PTY inside an implementation makes the last device to attach the owner of
    /// every subsequent resize, whoever sent it.
    async fn resize(
        &self,
        pty_id: &str,
        attachment: AttachmentId,
        cols: u16,
        rows: u16,
    ) -> Result<(), TerminalError>;

    /// Release an attachment.
    ///
    /// Must be called when a client stops watching — dropping the frame receiver
    /// is not enough on its own. An attachment holds a standing size vote, and a
    /// host that runs the terminal at the smallest vote keeps honouring a
    /// departed viewer's until it is withdrawn. A phone that shrinks a terminal
    /// and then closes the screen would otherwise leave the desktop's window
    /// narrow, with nothing left anywhere that could widen it again.
    ///
    /// Infallible by design: every caller is on a teardown path, where there is
    /// nothing useful to do with a failure and nothing that may be skipped
    /// because of one. Implementations log instead. Idempotent — releasing an
    /// attachment twice, or one the host has already reaped, is a no-op.
    async fn detach(&self, pty_id: &str, attachment: AttachmentId);
}
