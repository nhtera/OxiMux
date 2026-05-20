//! Runtime-side agent session types.
//!
//! Lives in `oximux-core` so the UI (badge, sidebar dot) and storage
//! (`SQLite` row mapping in Phase 4) share one source of truth without
//! pulling `oximux-agents` (which owns the runtime traits + tokio).
//!
//! `AgentSessionId` is a transient handle minted by the runtime per launch;
//! the persisted `AgentSession::id: Id` in `lib.rs` is the SQLite primary
//! key. They are deliberately distinct types so a transient runtime handle
//! cannot be mistaken for a persisted row id at compile time.

use serde::{Deserialize, Serialize};

/// Opaque transient handle to one live agent session, minted monotonically
/// by the runtime. Not persisted; use the SQLite `Id` for that.
///
/// Inner `u64` is private — `AgentRuntime` impls construct via `new()`, UI
/// callers receive opaque values they can `Eq`/`Hash` but not forge. A
/// hand-crafted id from outside the runtime would look up nothing in the
/// runtime's session table; making it unforgeable removes a footgun class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSessionId(u64);

impl AgentSessionId {
    /// Construct from a monotonic counter. Intended for `AgentRuntime`
    /// implementations only — callers that did not mint the id should
    /// not be calling this.
    pub fn new(n: u64) -> Self {
        Self(n)
    }

    /// Underlying counter — exposed only for logging and SQLite key
    /// mapping (Phase 4). Not for re-construction.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Lifecycle state surfaced to the UI badge and the multi-agent dashboard.
///
/// Variants intentionally carry payload (reason / exit code) so the badge
/// can show a tooltip without a side-channel lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// No output for `IDLE_AFTER`; agent is parked.
    Idle,
    /// Producing output recently (within `RUNNING_WITHIN`).
    Running,
    /// Generic prompt detected — agent is waiting on free-form user input.
    WaitingForInput,
    /// Approval prompt detected — distinct because the macOS notifier only
    /// pings on this transition (not on every keystroke prompt).
    NeedsApproval(String),
    /// Process exited cleanly. `code` is `None` when killed by signal.
    Done { code: Option<i32> },
    /// Process exited non-zero or runtime failed to spawn.
    Failed(String),
}

impl AgentStatus {
    /// True when the user is being asked something. Drives the badge color
    /// and the notifier (only `NeedsApproval` fires a macOS notification).
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::WaitingForInput | Self::NeedsApproval(_))
    }

    /// True when the session has exited (clean or error).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Failed(_))
    }
}
