//! High-level thread events — the decoded, transport-agnostic vocabulary the
//! `ChatThread` state machine and the UI consume.
//!
//! The stream-json decoder (Claude) and a future ACP decoder both normalize
//! their wire events into `ThreadEvent`, so the state machine and view never
//! learn which backend produced them.

use serde_json::Value;

use super::question::AskQuestion;
use super::tool_call::PermissionSuggestion;

/// Per-turn token/cost usage, decoded from the final `result` event. All counts
/// are best-effort (0 when the field is absent); `cost_usd`/`context_window` are
/// optional because not every turn reports them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// The model's context-window size (from `modelUsage`), for a "% of Nk" readout.
    pub context_window: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadEvent {
    /// Session bootstrap (`system/init`).
    SessionInit {
        session_id: String,
        model: String,
        permission_mode: String,
        /// Command names the backend advertises for a `/`-prefixed message
        /// (built-ins, skills, plugin commands). Names only — no descriptions.
        /// Empty when the backend doesn't advertise any. The UI offers these in
        /// a composer palette; the command itself rides as ordinary user text.
        slash_commands: Vec<String>,
    },
    /// A live streaming text delta (from `content_block_delta` text_delta).
    /// The UI may render these for smooth typing; the authoritative text
    /// arrives in the finalized `AssistantText`.
    AssistantTextDelta(String),
    /// A live streaming thinking delta.
    ThinkingDelta(String),
    /// Finalized assistant visible text block (from the `assistant` event).
    AssistantText(String),
    /// Finalized assistant thinking block.
    AssistantThinking(String),
    /// A tool call began (assistant `tool_use` block).
    ToolCallStarted {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool produced its result (`user` tool_result echo).
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// A tool needs the user's permission (`can_use_tool` control request).
    PermissionRequested {
        request_id: String,
        tool_use_id: Option<String>,
        tool_name: String,
        input: Value,
        description: String,
        suggestions: Vec<PermissionSuggestion>,
    },
    /// Claude called `AskUserQuestion`: a multiple-choice clarification the user
    /// answers via the interactive question card. Distinct from a permission
    /// prompt — it arrives on the same `can_use_tool` control channel but is
    /// answered with selections, not Allow/Reject.
    QuestionAsked {
        request_id: String,
        tool_use_id: Option<String>,
        questions: Vec<AskQuestion>,
    },
    /// One-line turn summary (`system/post_turn_summary`).
    TurnSummary {
        detail: String,
        category: String,
    },
    /// The turn finished (`result`). `usage` carries the token/cost breakdown
    /// when the result reports it (see [`TurnUsage`]).
    TurnEnded {
        result: Option<String>,
        usage: Option<TurnUsage>,
        is_error: bool,
    },
    /// A background task (subagent / background bash) started
    /// (`system/task_started`). Feeds the Background Tasks panel; the task's own
    /// internal stream stays out of the main transcript (see the decoder).
    BackgroundTaskStarted {
        task_id: String,
        tool_use_id: String,
        kind: super::background_task::BackgroundTaskKind,
        description: String,
    },
    /// Progress on a background task (`system/task_progress`): the tool it is now
    /// running. Advances the panel's per-task activity readout.
    BackgroundTaskProgress {
        task_id: String,
        last_tool: Option<String>,
    },
    /// A background task reached a terminal state (`system/task_updated` completion
    /// patch or `system/task_notification`). Either signal can arrive — fields are
    /// optional so a partial one still transitions the status; the fold merges
    /// them.
    BackgroundTaskFinished {
        task_id: String,
        failed: bool,
        ended_at_ms: Option<u64>,
        summary: Option<String>,
        output_file: Option<String>,
    },
    /// A protocol/parse/transport error to surface in the thread.
    Error(String),
}
