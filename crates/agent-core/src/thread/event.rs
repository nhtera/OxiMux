//! High-level thread events — the decoded, transport-agnostic vocabulary the
//! `ChatThread` state machine and the UI consume.
//!
//! The stream-json decoder (Claude) and a future ACP decoder both normalize
//! their wire events into `ThreadEvent`, so the state machine and view never
//! learn which backend produced them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::entry::ChatImage;
use super::question::AskQuestion;
use super::tool_call::{PermissionKind, PermissionSuggestion};

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

/// One authentication method an ACP agent advertises when it needs login, in a
/// gpui-free shape mirroring ACP's `AuthMethod` union. Rendered by the auth card
/// as a pill (Agent/Terminal) or an instructions block (EnvVar).
#[derive(Debug, Clone, PartialEq)]
pub struct AuthMethodInfo {
    /// The method id echoed back in `authenticate` / used to route the click.
    pub id: String,
    /// Human-readable label for the pill/heading.
    pub name: String,
    /// Optional one-line description shown muted.
    pub description: Option<String>,
    /// How this method authenticates — decides the card affordance + worker flow.
    pub kind: AuthMethodKind,
}

/// The authentication style of an [`AuthMethodInfo`], mirroring the ACP
/// `AuthMethod` variants. Drives both the card rendering and the worker's
/// per-kind flow.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthMethodKind {
    /// The agent handles login itself — clicking the pill runs `authenticate`.
    Agent,
    /// The user sets environment variables the agent reads, then re-authenticates.
    /// Instructions-only: the card lists the variable names (+ optional docs
    /// `link`) and a Retry pill; OxiMux never stores the secret values.
    EnvVar { vars: Vec<String>, link: Option<String> },
    /// The client runs the agent binary with `args` (and extra `env`) in an
    /// embedded terminal so the user logs in via a TUI; on exit the session is
    /// retried.
    Terminal { args: Vec<String>, env: Vec<(String, String)> },
    /// The agent hands back an OAuth URL the client opens in a browser (Codex's
    /// merged ChatGPT sign-in: `account/login/start` → `authUrl`). Clicking the
    /// pill calls `AgentConnection::begin_browser_login` (in `oximux-agents`),
    /// which returns the URL to open; a later `account/login/completed` resolves
    /// the card via [`ThreadEvent::AuthOutcome`]. No `args`/`env`: the agent runs
    /// its own callback server, the client only opens the URL.
    BrowserOauth,
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

/// What a session advertised about itself at init — the read-only facts behind
/// the session-detail popover (which tools are loaded, which MCP servers
/// connected, where it's rooted).
///
/// Serialized so a restored chat can show the same detail without waiting for a
/// fresh init: `--resume` stays silent until the first message. Every field is
/// `#[serde(default)]` for the same reason the sibling fields on the persisted
/// transcript are — blobs written before this existed must stay loadable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Working directory the agent was rooted at.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Tool names the agent loaded.
    #[serde(default)]
    pub tools: Vec<String>,
    /// MCP servers and the status each reported (`connected`, `pending`, …).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerStatus>,
    /// Subagent types available to the session.
    #[serde(default)]
    pub agents: Vec<String>,
}

/// One MCP server as reported at session init.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    /// Verbatim status string — displayed, never matched on, so a new status
    /// value the backend invents shows up rather than being dropped.
    pub status: String,
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
        /// What this session was started with, for the session-detail popover.
        /// Empty/`None` fields when the backend doesn't advertise them (Codex
        /// and ACP send far less than Claude's `system/init`), which the popover
        /// renders by omitting those rows rather than showing blanks.
        meta: SessionMeta,
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
    /// A fragment of a tool call's arguments streaming in before the finalized
    /// `tool_use` block (Claude `stream_event` `input_json_delta`). The card opens
    /// on the earlier `content_block_start` (`ToolCallStarted` with empty input);
    /// each fragment is appended to the accumulating partial-JSON string, which is
    /// best-effort parsed to preview the growing args. The authoritative input
    /// still arrives in the finalized `ToolCallStarted`, which supersedes the
    /// preview. Correlated by the tool-call `id`. Claude-only.
    ToolInputDelta {
        tool_call_id: String,
        partial_json: String,
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
    /// An ACP tool call's `ToolKind` (`execute`, `read`, `edit`, `search`,
    /// `fetch`, …), emitted as a follow-up to `ToolCallStarted` so the widely
    /// constructed start event stays untouched. Correlated by `tool_call_id`;
    /// the fold stashes it on `ToolCall.kind`, feeding the tool-detail classifier
    /// so an ACP tool (whose `name` is a freeform human title) still routes to a
    /// rich body. ACP-only — Claude/Codex classify by `name` and never emit it.
    ToolKind {
        tool_call_id: String,
        kind: String,
    },
    /// A tool needs the user's permission (`can_use_tool` control request).
    PermissionRequested {
        request_id: String,
        tool_use_id: Option<String>,
        tool_name: String,
        input: Value,
        description: String,
        suggestions: Vec<PermissionSuggestion>,
        /// What the request is for — `Tool` for an ordinary approval, `Plan` for
        /// Claude's `ExitPlanMode`, `Mcp` for a Codex MCP elicitation. Routes the
        /// request to a dedicated card in the view.
        kind: PermissionKind,
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
    /// The backend began compacting context (Claude `system/status
    /// status="compacting"`). A long compaction is otherwise silent until the
    /// boundary lands, so this drives a "Compacting context…" spinner; the state
    /// clears when [`CompactBoundary`](Self::CompactBoundary) or `TurnEnded`
    /// arrives. Claude-only.
    CompactionStarted,
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
        /// The unified diff of everything this turn changed, when the backend
        /// reports one. Codex accumulates it over the turn (`turn/diff/updated`,
        /// cumulative — the last one wins) and the mapper attaches it here at
        /// `turn/completed`; it covers files written by SHELL COMMANDS as well as
        /// by patch items, which is why it beats summing the turn's edit cards.
        ///
        /// `None` for backends that report no such diff (Claude, ACP, Pi) — the
        /// fold then derives the turn's changes from its own edit cards instead.
        turn_diff: Option<String>,
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
    /// `hints` is likewise parallel: the argument hint an ACP command advertises
    /// (`AvailableCommand.input`), shown as trailing muted text in the palette;
    /// empty entry (or empty list) when the command takes no argument.
    SlashCommandsUpdated {
        commands: Vec<String>,
        descriptions: Vec<String>,
        hints: Vec<String>,
    },
    /// The ACP agent requires authentication before a session can open
    /// (`session/new`/`session/load` failed with JSON-RPC `-32000`). Carries the
    /// advertised methods for the auth card; `error` is set when a prior
    /// `authenticate` attempt failed, so the card shows a retry state. ACP-only —
    /// Claude/Codex never emit it. Ephemeral (not persisted): a restored mid-auth
    /// tab fails closed to a fresh AuthRequired.
    AuthRequired {
        methods: Vec<AuthMethodInfo>,
        error: Option<String>,
    },
    /// A terminal-kind auth method launched the agent's login command in an
    /// embedded terminal; the app mounts an inline `TerminalView` bound to
    /// `terminal_id` inside the auth card while the user logs in. ACP-only.
    AuthTerminal {
        terminal_id: String,
    },
    /// The agent produced a browser sign-in URL to open (Codex `account/login/
    /// start` → `authUrl`). Emitted asynchronously by the worker after a
    /// `begin_browser_login` (on `AgentConnection`, in `oximux-agents`)
    /// request (so the click that triggers it never blocks the UI on the RPC).
    /// The app opens it in the system browser; the flow resolves later via
    /// [`ThreadEvent::AuthOutcome`]. Ephemeral, view-owned — the fold ignores it.
    AuthUrl {
        url: String,
    },
    /// A browser OAuth sign-in resolved (Codex `account/login/completed`). On
    /// `success` the app clears the auth card and the session continues (the
    /// backend now has credentials); on failure it re-shows the card with
    /// `error`. Ephemeral, view-owned — the fold ignores it, like `AuthRequired`.
    AuthOutcome {
        success: bool,
        error: Option<String>,
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
    /// Live, mid-turn token usage — emitted BEFORE `TurnEnded` for all three
    /// adapters where the counts are already on the wire (Claude
    /// `message_start`/`message_delta`; Codex `thread/tokenUsage/updated`; ACP
    /// `UsageUpdate`). Drives the composer's live context meter. Reuses
    /// [`TurnUsage`]; `cost_usd` stays `None` (cost is known only at turn-end) and
    /// `context_window` is `None` for Claude (its live events omit the window
    /// size) but present for Codex/ACP. Additive to — never a replacement for —
    /// the settled `TurnEnded.usage` that feeds the transcript footer.
    LiveUsage(TurnUsage),
    /// A best-effort diagnostic (the drained tail of the child's stderr)
    /// explaining the error turn it immediately precedes. The Claude reader
    /// thread emits it just before an error `TurnEnded`, already de-duplicated
    /// against the turn's own error text and redacted of secret-shaped values.
    /// `state.rs` stashes it and folds it into `last_error` on the next error
    /// `TurnEnded`, clearing the stash regardless so it can't leak into a later
    /// turn. Attribution is best-effort, not turn-precise: stdout and stderr are
    /// independently scheduled OS pipes with no happens-before between them.
    Diagnostic(String),
    /// A `--resume <session_id>` targeted a session the CLI no longer has
    /// (deleted, expired, or a bad restore). Carries the id whose resume was
    /// attempted; the fold clears the stored `session_id` ONLY when it still
    /// equals this (a fresh id an interleaved `SessionInit` minted must survive)
    /// and drops a one-line transcript notice so the next send starts fresh
    /// instead of looping the same error. Claude-only.
    SessionResumeStale {
        attempted_id: String,
    },
    /// A completed action inside a running subagent/child thread, routed into its
    /// spawning tool card's log rather than the root transcript. Claude emits one
    /// per `parent_tool_use_id`-tagged `tool_use`/`assistant` block (a child tool
    /// call or a first-line text summary); Codex emits one per foreign child-thread
    /// `item/started`/`item/completed` whose thread is registered to a collab tool
    /// card. `line` is a short pre-formatted summary (e.g. `Read src/main.rs`); the
    /// fold appends it to `ToolCall.subagent_log` (a capped ring). Child assistant
    /// deltas are NOT streamed here — only completed items — so the log can't churn
    /// the repaint loop. The root transcript stays free of child bubbles.
    SubagentAction {
        /// The spawning tool call's id — Claude's `parent_tool_use_id`, Codex's
        /// registered parent-item id. The fold matches it to a `ToolCall`.
        parent_tool_call_id: String,
        line: String,
    },
    /// Something the agent tried that the app quietly handled on the user's
    /// behalf, worth a trace in the transcript but not an error and not a
    /// prompt. Today: a server → client request we don't implement and
    /// auto-decline — without this the agent's attempt leaves no mark and the
    /// turn just looks like it stalled. Folds to a muted divider, like the
    /// other non-message notices.
    Notice(String),
    /// A protocol/parse/transport error to surface in the thread.
    Error(String),
}

impl ThreadEvent {
    /// Whether this event only extends content the transcript already shows — a
    /// streamed token, a growing tool argument or output, a usage tick — rather
    /// than changing its shape.
    ///
    /// A repaint re-parses the whole (growing) markdown body of the streaming
    /// message, so a fast model emitting hundreds of tokens a second would cost
    /// hundreds of full re-parses. Deltas are safe to coalesce into one repaint
    /// because the intermediate frames only differ by a few characters nobody
    /// can read at that rate. Everything else — a tool card appearing, a
    /// permission prompt, a turn ending — changes what the user can act on, so
    /// it must paint immediately.
    pub fn is_delta(&self) -> bool {
        matches!(
            self,
            Self::AssistantTextDelta(_)
                | Self::ThinkingDelta(_)
                | Self::ToolInputDelta { .. }
                | Self::ToolOutputDelta { .. }
                | Self::LiveUsage(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_content_extending_events_are_deltas() {
        assert!(ThreadEvent::AssistantTextDelta("hi".into()).is_delta());
        assert!(ThreadEvent::ThinkingDelta("hm".into()).is_delta());
        assert!(
            ThreadEvent::ToolInputDelta {
                tool_call_id: "t".into(),
                partial_json: "{".into()
            }
            .is_delta()
        );
        assert!(ThreadEvent::ToolOutputDelta { id: "t".into(), chunk: "out".into() }.is_delta());
        assert!(ThreadEvent::LiveUsage(TurnUsage::default()).is_delta());
    }

    #[test]
    fn turn_shape_events_are_never_deltas() {
        // These change what the user can see or act on, so coalescing them
        // behind a timer would delay a tool card, a prompt, or a turn ending.
        assert!(!ThreadEvent::AssistantText("done".into()).is_delta());
        assert!(!ThreadEvent::AssistantThinking("thought".into()).is_delta());
        assert!(
            !ThreadEvent::ToolCallStarted {
                id: "t".into(),
                name: "Bash".into(),
                input: serde_json::Value::Null
            }
            .is_delta()
        );
        assert!(
            !ThreadEvent::ToolResult {
                tool_use_id: "t".into(),
                content: "ok".into(),
                is_error: false,
                structured: None
            }
            .is_delta()
        );
        assert!(
            !ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None }.is_delta()
        );
        assert!(!ThreadEvent::Error("boom".into()).is_delta());
    }
}
