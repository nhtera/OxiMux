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

use oximux_remote_proto::simulator::{SimCmdWire, SimErrorWire, SimReplyWire};

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
}
