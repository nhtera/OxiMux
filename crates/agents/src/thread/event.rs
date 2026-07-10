//! High-level thread events — the decoded, transport-agnostic vocabulary the
//! `ChatThread` state machine and the UI consume.
//!
//! The stream-json decoder (Claude) and a future ACP decoder both normalize
//! their wire events into `ThreadEvent`, so the state machine and view never
//! learn which backend produced them.

use serde_json::Value;

use super::entry::ChatImage;
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

/// One entry of an agent execution plan, in a gpui-free shape mirroring ACP's
/// `PlanEntry` (`content` + a three-state status + a priority). Rendered by the
/// plan panel as a checklist row.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanEntryLite {
    pub content: String,
    /// Lifecycle: `"pending"`, `"in_progress"`, or `"completed"`. String-typed so
    /// the view reuses the same `from_wire` mapping the `TodoWrite` path already
    /// uses (no second status enum to keep in sync).
    pub status: String,
    /// Relative importance: `"high"`, `"medium"`, or `"low"`.
    pub priority: String,
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
        /// The line's top-level `tool_use_result` (snake_case on the live wire) —
        /// the structured sibling of the flattened `content`, carrying Bash
        /// `{stdout, stderr, interrupted}`, subagent stats, a Read's `numLines`,
        /// etc. `None` when the backend didn't emit one. Fed into
        /// `ToolCall.structured` so the shared renderers enrich live chats the
        /// same way they enrich imported history.
        structured: Option<Value>,
    },
    /// Inline images carried by a tool result — the actual base64 pixels the
    /// flattened `[image]` placeholder stands in for (a `Read` of an image file,
    /// a screenshot tool, …). Emitted right after the matching `ToolResult` so
    /// the tool card renders a thumbnail instead of the placeholder text.
    /// Correlated by `tool_use_id`. Claude-only today; Codex/ACP never emit it.
    ToolResultImages {
        tool_use_id: String,
        images: Vec<ChatImage>,
    },
    /// A chunk of live tool output streaming before completion (Codex
    /// `item/commandExecution/outputDelta`). Appended to the open tool card's
    /// result body as it arrives, keyed by the tool-call `id`. The authoritative
    /// full output still lands in `ToolResult` at completion, which replaces the
    /// accumulated chunks (so out-of-order interleaving can't corrupt the final).
    ToolOutputDelta {
        id: String,
        chunk: String,
    },
    /// An ACP tool call embeds a live terminal created via `terminal/create`
    /// (`ToolCallContent::Terminal`). Correlated to its card by `tool_call_id`;
    /// carries the client-minted `terminal_id` the app uses to mount an inline
    /// `TerminalView` bound to that PTY. ACP-only — Claude/Codex never emit it.
    ToolTerminal {
        tool_call_id: String,
        terminal_id: String,
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
    /// The backend compacted earlier context to reclaim window space (Claude
    /// `system/compact_boundary`, Codex `thread/compacted`). Rendered as a subtle
    /// centered divider (reusing the session-import `ContextCompaction` entry) so
    /// the gap in history is visible rather than silent.
    CompactBoundary {
        summary: String,
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
    /// The agent replaced its execution plan (ACP `session/update` `Plan`). Carries
    /// the full entry list (ACP sends a complete replacement each time), rendered
    /// as one pinned checklist card that survives turn boundaries.
    PlanUpdated {
        entries: Vec<PlanEntryLite>,
    },
    /// The backend published/changed its slash commands mid-session (ACP
    /// `available_commands_update`) — e.g. Cursor, which advertises them
    /// asynchronously after session start. Refreshes the composer's palette.
    /// `descriptions` is parallel to `commands` (same order/length) when the
    /// backend supplies them (ACP), else empty (Claude/Codex advertise names
    /// only) — the palette shows a description under the name when present.
    SlashCommandsUpdated {
        commands: Vec<String>,
        descriptions: Vec<String>,
    },
    /// The session's permission/edit mode changed (ACP `current_mode_update`),
    /// whether the user picked it or the agent switched it itself. Keeps the mode
    /// picker in sync.
    ModeChanged {
        mode_id: String,
    },
    /// The agent replaced its session config options at runtime (ACP
    /// `config_option_update`) — the full set of models / reasoning options and
    /// their current values. Some agents advertise no models at session start and
    /// populate them only after auth or a workspace probe, or switch the current
    /// model themselves; this signal tells the UI to re-pull the composer's model
    /// and reasoning pickers from the live connection. Carries no payload: the
    /// backend has already absorbed the new options, so the view re-reads them.
    ControlsUpdated,
    /// The session title changed (ACP `session_info_update`), for the tab label.
    TitleUpdated {
        title: String,
    },
    /// A protocol/parse/transport error to surface in the thread.
    Error(String),
}
