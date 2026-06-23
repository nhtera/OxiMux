//! Per-agent rail row + the per-workspace list it composes into.
//!
//! Today the rail shows one collapsed status per workspace. To list every
//! agent under a workspace, the merge step (`session_merge`) produces one
//! `RailAgentRow` per agent — live sessions (from the runtime's
//! `LiveAgentMap`) unified with DB history rows by their shared
//! `agent_sessions.id` UUID. Phase 3's render reads `WorkspaceAgentList`.

use gpui::SharedString;
use oximux_agents::AgentStatusStream;
use oximux_core::AgentStatus;

/// One agent in a workspace's rail list — a live session, a finished
/// history row, or a live session whose DB insert hasn't landed yet.
#[derive(Clone)]
pub struct RailAgentRow {
    /// `agent_sessions.id` UUID — the unified key across live + history.
    pub db_id: String,
    /// Owning workspace key (`workspaces.id` or `primary:<project_id>`).
    pub workspace_key: String,
    /// Adapter slug (`claude-code`, `codex`, …).
    pub adapter_id: String,
    /// Display label (adapter name; Phase 3 may add a per-workspace index).
    pub label: SharedString,
    /// `true` when a live `LiveAgentEntry` backs this row.
    pub is_live: bool,
    /// Live status receiver — `Some` only when `is_live`. Render reads the
    /// current status via `status_rx.borrow()`; falls back to `db_status`.
    pub status_rx: Option<AgentStatusStream>,
    /// DB-persisted status (the last-known / terminal state for history).
    pub db_status: AgentStatus,
    /// Launch timestamp (RFC-3339), for ordering + relative-age rendering.
    pub started_at: Option<String>,
    /// Terminal timestamp (RFC-3339), `None` while live/non-terminal.
    pub ended_at: Option<String>,
    /// Pre-formatted relative age shown on the right of the row (e.g. "now",
    /// "3d"). Computed at merge time against a captured `now` so render needs
    /// no clock. Empty when no timestamp is available.
    pub age_label: String,
    /// DB-persisted title (the agent's most recent prompt). The rail title
    /// falls back to this when the live status channel carries no prompt — so a
    /// restored or re-adopted session keeps its title across an app restart.
    /// `None` for a session that never captured a prompt.
    pub persisted_title: Option<String>,
}

impl RailAgentRow {
    /// Effective status for render: the live snapshot when present, else the
    /// DB-persisted status. Live always wins — it is fresher than the row.
    pub fn effective_status(&self) -> AgentStatus {
        match &self.status_rx {
            Some(rx) => rx.borrow().status.clone(),
            None => self.db_status.clone(),
        }
    }
}

/// Per-workspace agent lists keyed by workspace key. Each value is sorted
/// live-first, then most-recent `started_at` first.
pub type WorkspaceAgentList = std::collections::HashMap<String, Vec<RailAgentRow>>;
