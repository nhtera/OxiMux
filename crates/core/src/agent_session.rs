//! Runtime + persisted agent session types.
//!
//! Lives in `oximux-core` so the UI (badge, sidebar dot) and storage
//! (`AgentSessionRepo` row mapping in Phase 4) share one source of truth
//! without pulling `oximux-agents` (which owns the runtime traits + tokio).
//!
//! `AgentSessionId` is a transient handle minted by the runtime per
//! launch; the persisted `AgentSession::id` (`String` UUID) below is the
//! SQLite primary key. They are deliberately distinct types so a
//! transient runtime handle cannot be mistaken for a persisted row id at
//! compile time.

use serde::{Deserialize, Serialize};

/// Opaque transient handle to one live agent session, minted monotonically
/// by the runtime. Not persisted; use the UUID `AgentSession::id` for that.
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

    /// Underlying counter — exposed only for logging and dedupe-map keys.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Lifecycle state surfaced to the UI badge and persisted via the
/// three-column codec (`status TEXT`, `exit_code INTEGER NULL`,
/// `status_detail TEXT NULL`).
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
    /// Session was alive at shutdown and could not be resumed on restart.
    /// Set by the boot sweep on every row returned from
    /// `AgentSessionRepo::list_unfinished_at_shutdown`.
    Interrupted,
}

impl AgentStatus {
    /// True when the user is being asked something. Drives the badge color
    /// and the notifier (only `NeedsApproval` fires a macOS notification).
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::WaitingForInput | Self::NeedsApproval(_))
    }

    /// True when the session has exited (clean or error or interrupted).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done { .. } | Self::Failed(_) | Self::Interrupted
        )
    }

    /// Storage slug — deterministic, lowercase, no spaces. Stable across
    /// schema migrations; new variants append, never rename.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingForInput => "waiting_input",
            Self::NeedsApproval(_) => "needs_approval",
            Self::Done { .. } => "done",
            Self::Failed(_) => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// Optional exit-code column value — populated only for `Done { code }`.
    pub fn exit_code_for_storage(&self) -> Option<i32> {
        match self {
            Self::Done { code } => *code,
            _ => None,
        }
    }

    /// Optional detail column value — populated only for variants that
    /// carry a free-form payload.
    pub fn detail_for_storage(&self) -> Option<&str> {
        match self {
            Self::NeedsApproval(s) | Self::Failed(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Reconstruct from stored columns. Returns `None` on unknown status
    /// slug — callers (e.g. `AgentSessionRow::from_row`) degrade to
    /// `AgentStatus::Interrupted` rather than panicking.
    pub fn from_row(
        status: &str,
        exit_code: Option<i32>,
        status_detail: Option<String>,
    ) -> Option<Self> {
        match status {
            "idle" => Some(Self::Idle),
            "running" => Some(Self::Running),
            "waiting_input" => Some(Self::WaitingForInput),
            "needs_approval" => Some(Self::NeedsApproval(status_detail.unwrap_or_default())),
            "done" => Some(Self::Done { code: exit_code }),
            "failed" => Some(Self::Failed(status_detail.unwrap_or_default())),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

/// Persisted agent session — one row in the `agent_sessions` table.
/// Distinct from the transient `AgentSessionId` runtime handle above.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub workspace_id: String,
    pub adapter_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: AgentStatus,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}
