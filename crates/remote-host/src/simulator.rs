//! The iOS Simulator seam: `oximux sim …` verbs, expressed without depending on
//! the desktop that carries them out.
//!
//! The simulator lives in the desktop's view layer — its device registry, the
//! stream helper, the consent banner the user answers — none of which this
//! crate can reach. So the dispatcher talks to this trait and the desktop
//! supplies the implementation, as it does for
//! [`SessionLauncher`](crate::launcher::SessionLauncher) and
//! [`RewindService`](crate::rewind::RewindService). A headless host installs
//! none and answers an authorized caller `Unsupported`.
//!
//! The dispatcher has already decided **who** may call (local callers only)
//! and **which worktree** a confined caller means (its own session's). What is
//! left to the implementation is everything about the device: resolving the
//! worktree to one the desktop knows, the per-device consent, and the verb.

use oximux_remote_proto::simulator::{SimCmdWire, SimErrorWire, SimReplyWire, SimViewKind, SimViewableWire};

/// Who drives the device, as the caller's credential proved it: consent is
/// kept per principal, so an outside MCP client never rides the approval
/// given to agents (or the operator's CLI), nor they its.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SimPrincipal {
    /// The operator's CLI or an agent's.
    Agent,
    /// An outside MCP client (`oximux mcp`).
    Mcp,
}

#[async_trait::async_trait]
pub trait SimulatorControl: Send + Sync {
    /// Run `cmd`, for `principal`, against the simulator attached to the
    /// worktree containing `worktree` (a path; the implementation resolves
    /// it). The future may be dropped (the caller went away): an
    /// implementation stops the verb then.
    async fn run(&self, worktree: &str, principal: SimPrincipal, cmd: SimCmdWire) -> Result<SimReplyWire, SimErrorWire>;

    /// Whether this host can show a device to a paired phone at all. A host
    /// without (the CLI's stand-ins) answers `Unsupported` to the view RPCs.
    fn supports_view(&self) -> bool {
        false
    }

    /// The devices `viewer` may watch. [`SimErrorWire::Refused`] while the
    /// desktop's "Let paired phones view" setting is off.
    async fn view_list(&self, viewer: &SimViewer) -> Result<Vec<SimViewableWire>, SimErrorWire> {
        let _ = viewer;
        Err(SimErrorWire::Unavailable("this host cannot show a device".into()))
    }

    /// Start showing `device` to `viewer`. The first ask for a device in a
    /// desktop run raises the desktop's question and answers
    /// [`SimErrorWire::ConsentPending`]; the phone asks again.
    async fn view_subscribe(&self, viewer: &SimViewer, device: &str) -> Result<SimViewFeed, SimErrorWire> {
        let _ = (viewer, device);
        Err(SimErrorWire::Unavailable("this host cannot show a device".into()))
    }
}

/// Who is watching: one subscription of one paired phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimViewer {
    /// The subscription's number, unique for the host's run.
    pub id: u64,
    /// The phone's paired-device key: grants are kept per phone.
    pub phone: [u8; 32],
    /// The name the phone paired with, for the desktop to show.
    pub phone_name: String,
}

/// A subscription's picture: its size now, where its frames come from, and
/// the rate to write them at.
pub struct SimViewFeed {
    pub width: u32,
    pub height: u32,
    /// The stream is written no faster than this (0: as fast as the link
    /// takes it). Pacing is what keeps a slow link usable: a key frame
    /// written at once fills the link's queue, and every RPC waits behind it
    /// there, where no stream priority reaches.
    pub bits_per_sec: u64,
    pub source: Box<dyn SimViewSource>,
}

/// One frame for the phone, as the view stream carries it.
#[derive(Clone, Debug)]
pub struct SimViewOut {
    pub kind: SimViewKind,
    pub pts_us: u64,
    pub payload: std::sync::Arc<[u8]>,
}

/// The frames of one subscription. Dropping it ends the subscription on the
/// desktop (the phone no longer counts as a viewer).
#[async_trait::async_trait]
pub trait SimViewSource: Send + Sync {
    /// The next frame to send, or `None` once the device stopped streaming.
    /// Cancel-safe: the pump drops it to re-check authorization.
    async fn next(&mut self) -> Option<SimViewOut>;

    /// Whether the desktop still lets this phone watch (the setting is on,
    /// the user did not stop sharing). Asked before every frame and at least
    /// once a second while the screen is still.
    fn allowed(&self) -> bool;
}
