//! `ChatThread` — the in-memory conversation state machine.
//!
//! Folds a stream of `ThreadEvent`s into an ordered `Vec<ThreadEntry>` (the
//! message history OxiMux lacks today). Pure and gpui-free: the app crate
//! owns a GPUI entity that holds a `ChatThread`, calls `apply` on each event,
//! and repaints.
//!
//! Reconciliation: streaming deltas build the current assistant block; the
//! FIRST finalized block of that kind replaces the streamed text (so it isn't
//! doubled), and any FURTHER finalized block of the same kind in the same
//! message is appended (a message may legitimately carry more than one text
//! block). A tool call ends the current assistant block, so later text starts
//! a fresh assistant message.

use super::background_task::{BackgroundTask, TaskStatus};
use super::entry::{AssistantMessage, ChatImage, CheckpointState, ThreadEntry};
use super::event::{PlanEntryLite, SessionMeta, ThreadEvent, TurnUsage};
use super::question::QuestionRequest;
use super::tool_call::{PermissionRequest, ToolCall, ToolCallStatus};
use super::turn_diff;

/// Stands in for a secret answer's echoed result everywhere a result is read —
/// the transcript blob, the settled card summary, Copy, the raw sheet.
pub const SECRET_PLACEHOLDER: &str = "[secret]";

#[derive(Debug, Default)]
pub struct ChatThread {
    pub entries: Vec<ThreadEntry>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Command names advertised at session init (from `SessionInit`), for the
    /// composer's slash-command palette. Empty until init arrives or when the
    /// backend advertises none. Rides the restore snapshot so a resumed session
    /// keeps its palette without waiting for a fresh init.
    pub slash_commands: Vec<String>,
    /// What the session advertised about itself at init (tools, MCP servers,
    /// subagents, cwd), behind the session-detail popover. Rides the restore
    /// snapshot for the same reason the palette does: `--resume` sends no init
    /// until the first message, so without the cache a restored chat would show
    /// an empty popover until then.
    pub session_meta: SessionMeta,
    /// One-line description per slash command, keyed by command name (ACP agents
    /// like Cursor advertise them; Claude/Codex don't). Ephemeral — NOT persisted;
    /// repopulated when the agent re-advertises via `SlashCommandsUpdated`, so a
    /// restored session shows names-only until the live refresh arrives. The
    /// palette renders a muted description under a command when one is present.
    pub slash_command_descriptions: std::collections::HashMap<String, String>,
    /// Argument hint per slash command, keyed by command name (ACP
    /// `AvailableCommand.input`) — shown as trailing muted text in the palette.
    /// Ephemeral like [`Self::slash_command_descriptions`]: NOT persisted,
    /// repopulated on the live `SlashCommandsUpdated` refresh.
    pub slash_command_hints: std::collections::HashMap<String, String>,
    /// The agent's current execution plan (ACP `Plan` session update), rendered as
    /// a pinned checklist. `None` when there's no plan (or it was cleared). Held
    /// in-memory only — a restored chat starts planless and re-renders when the
    /// agent re-emits (like `background_tasks`). Survives turn boundaries.
    pub plan: Option<Vec<PlanEntryLite>>,
    /// The agent-set session title (ACP `session_info_update`), for the tab label.
    /// `None` until the agent sets one.
    pub title: Option<String>,
    /// Latest one-line turn summary (`post_turn_summary`), for a status chip.
    pub last_summary: Option<String>,
    /// Latest transport/protocol error, surfaced non-fatally.
    pub last_error: Option<String>,
    /// Whether a turn is currently in flight (between a user send and
    /// `TurnEnded`). Drives the composer's send/stop affordance.
    pub turn_active: bool,
    /// Whether the backend is compacting context right now (Claude
    /// `system/status status="compacting"`). Set on `CompactionStarted`, cleared
    /// when the `CompactBoundary` divider lands or the turn ends without one — so
    /// a long compaction shows a "Compacting context…" spinner instead of looking
    /// like a hang. Transient runtime state (not persisted).
    pub compacting: bool,
    /// Latest per-turn token/cost usage (from the most recent `TurnEnded`), for
    /// the usage footer. `None` until a turn reports usage.
    pub usage: Option<TurnUsage>,
    /// Live, mid-turn token usage (from `LiveUsage`), for the composer's context
    /// meter. Separate from [`Self::usage`] (turn-settled): cleared at the start
    /// of each new turn so a stale meter never lingers with no data yet.
    pub live_usage: Option<TurnUsage>,
    /// The last context-window size seen from ANY usage (live or settled) — the
    /// denominator cache for the % meter. Claude's live events omit the window,
    /// so this is seeded from a settled turn and reused for the current turn's
    /// live %. Never cleared on a new turn (the window doesn't change per turn).
    ///
    /// A turn is not the only writer: a backend that knows its window without
    /// running one ([`AgentConnection::context_window`], which Pi answers from
    /// its handshake) seeds this at connect, and refreshes it when a live model
    /// switch changes the window — pi's models range from 128K to 372K, so the
    /// denominator genuinely follows the model.
    pub last_known_context_window: Option<u64>,
    /// Cumulative USD spend across every settled turn THIS app-session ("cost
    /// since open"). Resets to zero on restore (persisted entries carry no
    /// per-turn cost) and deliberately does NOT decrement on rewind — money spent
    /// doesn't come back, so a cost meter keeps counting all real spend.
    pub session_cost_usd: f64,
    /// Background tasks (subagents + background bash) folded from `task_*` events,
    /// in start order, for the Background Tasks panel. In-memory only (not
    /// persisted): a task's output files are ephemeral, so a restored chat starts
    /// with none rather than dangling references.
    pub background_tasks: Vec<BackgroundTask>,
    /// Index of the assistant entry currently being streamed, if any.
    current_assistant: Option<usize>,
    /// True once a text delta has streamed into the current assistant block
    /// but its finalized text hasn't reconciled it yet.
    text_streaming: bool,
    /// Same, for the thinking block.
    thinking_streaming: bool,
    /// A stderr diagnostic emitted just before an error `TurnEnded`, held until
    /// that `TurnEnded` folds it into `last_error`. Transient: the `TurnEnded`
    /// handler `take()`s it regardless of outcome so a stray diagnostic can
    /// never leak into a later turn's error text.
    pending_diagnostic: Option<String>,
    /// The id of the tool call whose arguments are currently streaming in as
    /// `input_json_delta` fragments. Set when a `content_block_start` opens a
    /// tool card early; the `input_json_delta` events carry no id, so they route
    /// to this. Cleared when the finalized `tool_use` block supersedes the
    /// preview. Transient (Claude live tool-input only).
    streaming_tool_id: Option<String>,
    /// Monotonic mutation counter, bumped by every mutating method. Lets
    /// persistence ask "did the thread change since the last save?" as a
    /// structural fact (compare against a remembered value) instead of a
    /// dirty flag every mutation call site must remember to set — a missed
    /// site there silently serves a stale blob after quit. Direct writes to
    /// the `pub` fields do NOT bump it; view-held metadata keeps its own
    /// mark. Private so nothing outside can rewind it.
    revision: u64,
}

impl ChatThread {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current mutation count. Persistence remembers the value it saved
    /// at; inequality means the transcript changed since.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Bump the mutation counter — every `&mut self` method that changes
    /// transcript-visible state calls this.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Rebuild a thread from a persisted transcript on session restore. Seeds
    /// `entries` + the `session_id`/`model` needed to `--resume`, and leaves the
    /// streaming/turn flags at rest (no turn is in flight after a relaunch).
    ///
    /// **Fail-closed:** any tool call left `WaitingForConfirmation` when the app
    /// last quit is downgraded to `Rejected`. The process that asked is dead, so
    /// the request can never be answered — restoring it as pending would strand a
    /// permanently-unanswerable prompt in the UI (and, worse, imply the edit is
    /// still gated). Denying is the safe default, matching the live-disconnect
    /// fail-closed rule in the view.
    pub fn rehydrated(
        session_id: Option<String>,
        model: Option<String>,
        mut entries: Vec<ThreadEntry>,
        slash_commands: Vec<String>,
    ) -> Self {
        for entry in &mut entries {
            if let ThreadEntry::ToolCall(tc) = entry
                && matches!(
                    tc.status,
                    ToolCallStatus::WaitingForConfirmation(_) | ToolCallStatus::AwaitingAnswer(_)
                )
            {
                tc.status = ToolCallStatus::Rejected;
            }
        }
        Self {
            entries,
            session_id,
            model,
            slash_commands,
            ..Self::default()
        }
    }

    /// Reset to a blank conversation ("New chat" / the CLI's own `/clear`) while
    /// keeping the picker-facing config the composer reads — the model, the
    /// permission mode, and the slash-command palette. Everything tied to the old
    /// turn/session (entries, session id, streaming state, usage, background
    /// tasks) is dropped so a fresh (non-resumed) session can start clean.
    pub fn clear(&mut self) {
        let model = self.model.take();
        let permission_mode = self.permission_mode.take();
        let slash_commands = std::mem::take(&mut self.slash_commands);
        let slash_command_descriptions = std::mem::take(&mut self.slash_command_descriptions);
        let slash_command_hints = std::mem::take(&mut self.slash_command_hints);
        *self = Self {
            model,
            permission_mode,
            slash_commands,
            slash_command_descriptions,
            slash_command_hints,
            // Carry the counter PAST the reset — `Self::default()` would
            // rewind it to a value persistence may have already recorded as
            // saved, making the wipe look like "nothing changed".
            revision: self.revision.wrapping_add(1),
            ..Self::default()
        };
    }

    /// Record a user prompt (called when we send one to the agent).
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.push_user_message_with_images(text, Vec::new());
    }

    /// Append turns imported from the session's on-disk log — turns run in a
    /// companion terminal on the same session id, which never flow through
    /// this thread's connection. Bumps the revision so persistence saves them;
    /// leaves the turn flags alone (imported turns already finished — or are
    /// still running in the terminal, which owns them either way).
    pub fn append_imported(&mut self, tail: Vec<ThreadEntry>) {
        if tail.is_empty() {
            return;
        }
        self.entries.extend(tail);
        self.touch();
    }

    /// Record a user prompt that carries attached images.
    pub fn push_user_message_with_images(
        &mut self,
        text: impl Into<String>,
        images: Vec<ChatImage>,
    ) {
        self.touch();
        self.entries.push(ThreadEntry::User { text: text.into(), images, checkpoint: None });
        self.end_assistant_window();
        // A new turn begins: the prior turn's usage/summary no longer apply.
        // Clear them so the footer never implies stale numbers belong to this
        // turn (e.g. if this turn ends in error before reporting any usage).
        // Clearing `last_error` too dismisses a prior turn's error card the
        // moment the user sends again (a fresh send is an implicit retry).
        self.usage = None;
        self.live_usage = None;
        self.last_summary = None;
        self.last_error = None;
        self.turn_active = true;
    }

    /// Fold one decoded event into the transcript.
    pub fn apply(&mut self, event: &ThreadEvent) {
        // Over-approximates (an event that folds to nothing still bumps) —
        // the cost is one avoidable serialize on the next save, never a
        // missed one.
        self.touch();
        match event {
            ThreadEvent::SessionInit { session_id, model, permission_mode, slash_commands, meta } => {
                self.session_id = Some(session_id.clone());
                self.model = Some(model.clone());
                self.permission_mode = Some(permission_mode.clone());
                // Keep the last non-empty list: a resumed session may seed the
                // palette before init, and a later init should never blank it.
                if !slash_commands.is_empty() {
                    self.slash_commands = slash_commands.clone();
                }
                // Same rule as the palette: a restored session seeds this from
                // the persisted blob, so an init that advertises nothing (Codex,
                // ACP) must not blank what restore already showed.
                if meta != &SessionMeta::default() {
                    self.session_meta = meta.clone();
                }
            }
            // Reuses the same push the desktop calls directly, so a remote fold
            // and a local one produce an identical entry (and the same turn reset).
            ThreadEvent::UserMessage { text, images } => {
                self.push_user_message_with_images(text.clone(), images.clone());
            }
            ThreadEvent::AssistantTextDelta(t) => {
                self.assistant_mut().text.push_str(t);
                self.text_streaming = true;
            }
            ThreadEvent::ThinkingDelta(t) => {
                self.assistant_mut().thinking.push_str(t);
                self.thinking_streaming = true;
            }
            // First finalized block reconciles (replaces) the streamed value;
            // a further finalized block of the same kind appends.
            ThreadEvent::AssistantText(t) => {
                if self.text_streaming {
                    self.assistant_mut().text = t.clone();
                    self.text_streaming = false;
                } else {
                    let m = self.assistant_mut();
                    if !m.text.is_empty() {
                        m.text.push('\n');
                    }
                    m.text.push_str(t);
                }
            }
            ThreadEvent::AssistantThinking(t) => {
                if self.thinking_streaming {
                    self.assistant_mut().thinking = t.clone();
                    self.thinking_streaming = false;
                } else {
                    let m = self.assistant_mut();
                    if !m.thinking.is_empty() {
                        m.thinking.push('\n');
                    }
                    m.thinking.push_str(t);
                }
            }
            ThreadEvent::ToolCallStarted { id, name, input } => {
                // A tool card can open twice for one id: first from the live
                // `content_block_start` (empty input, args still streaming), then
                // from the finalized `tool_use` block (authoritative input). The
                // second is an upgrade-in-place, not a duplicate card — reconcile
                // onto the existing entry, replacing the streamed preview.
                if let Some(tc) = self.tool_call_mut(id) {
                    tc.name = name.clone();
                    // The finalized block carries the real input; a still-empty
                    // upgrade (defensive) keeps the streamed preview intact.
                    if !is_empty_input(input) {
                        tc.input = input.clone();
                        tc.partial_input.clear();
                        if self.streaming_tool_id.as_deref() == Some(id.as_str()) {
                            self.streaming_tool_id = None;
                        }
                    }
                } else {
                    self.entries.push(ThreadEntry::ToolCall(ToolCall::new(
                        id.clone(),
                        name.clone(),
                        input.clone(),
                    )));
                    // An early open (empty input) marks this call as the one live
                    // `input_json_delta` fragments should accumulate onto.
                    if is_empty_input(input) {
                        self.streaming_tool_id = Some(id.clone());
                    }
                }
                // A tool call ends the current assistant block; later text
                // starts a fresh assistant message.
                self.end_assistant_window();
            }
            ThreadEvent::ToolInputDelta { tool_call_id, partial_json } => {
                // Append the streamed argument fragment to the open tool card. The
                // delta carries no id (Claude streams one block at a time), so fall
                // back to the tracked streaming tool call. Bounded so a giant Write
                // can't grow the preview buffer without limit.
                let target = if tool_call_id.is_empty() {
                    self.streaming_tool_id.clone()
                } else {
                    Some(tool_call_id.clone())
                };
                if let Some(id) = target
                    && let Some(tc) = self.tool_call_mut(&id)
                    && tc.partial_input.len() < super::tool_call::MAX_PARTIAL_INPUT_BYTES
                {
                    tc.partial_input.push_str(partial_json);
                }
            }
            ThreadEvent::ToolResult { tool_use_id, content, is_error, structured } => {
                // A result for an unknown id is silently dropped (no matching
                // tool call to attach it to) rather than mutating the wrong
                // entry — see the `tool_result_for_unknown_id_is_ignored` test.
                if let Some(tc) = self.tool_call_mut(tool_use_id) {
                    // This call answered a secret question, so the backend's
                    // report echoes the credential back at us. Redact it HERE —
                    // the one place a result becomes part of an entry — so the
                    // secret reaches neither the persisted blob nor any surface
                    // that reads the card (the settled summary, Copy, the sheet).
                    // The whole result goes, not just the secret span: the echo is
                    // backend-formatted prose, so there is no reliable way to
                    // excise one answer from it, and under-redacting is the one
                    // failure this must not have.
                    let (content, structured) = if tc.redact_result {
                        (&SECRET_PLACEHOLDER.to_string(), &None)
                    } else {
                        (content, structured)
                    };
                    // Completion replaces the streamed body — but a blank final
                    // (a cancelled/partial Codex command whose `aggregatedOutput`
                    // is empty) must not erase text the user already saw stream in
                    // via `ToolOutputDelta`.
                    if !content.is_empty() || tc.result.is_none() {
                        tc.result = Some(content.clone());
                    }
                    // Only overwrite when the wire actually carried a structured
                    // sibling — a bare result must not wipe one captured earlier.
                    if structured.is_some() {
                        tc.structured = structured.clone();
                    }
                    tc.status = if *is_error {
                        ToolCallStatus::Failed(content.clone())
                    } else {
                        ToolCallStatus::Completed
                    };
                }
            }
            ThreadEvent::ToolResultImages { tool_use_id, images } => {
                // Attach the tool result's inline images to the matching card.
                // Arrives right after `ToolResult`; an unknown id is dropped like
                // an orphan result (no card to attach to).
                if !images.is_empty()
                    && let Some(tc) = self.tool_call_mut(tool_use_id)
                {
                    tc.images = images.clone();
                }
            }
            ThreadEvent::ToolOutputDelta { id, chunk } => {
                // Live output streaming before completion: append to the open
                // card's result body. `ToolResult` at completion replaces this
                // with the authoritative full output.
                if let Some(tc) = self.tool_call_mut(id) {
                    // A card that answered a secret question refuses streamed
                    // output for the same reason it refuses the final result: no
                    // backend echo of that answer becomes transcript content. No
                    // backend streams deltas onto a question card today — this
                    // keeps the "a secret never reaches disk" invariant a property
                    // of the fold rather than of which events happen to be wired.
                    if tc.redact_result {
                        return;
                    }
                    match &mut tc.result {
                        Some(existing) => existing.push_str(chunk),
                        None => tc.result = Some(chunk.clone()),
                    }
                }
            }
            ThreadEvent::ToolTerminal { tool_call_id, terminal_id } => {
                // Bind the ACP embedded terminal to its card so the app mounts an
                // inline `TerminalView`. An unknown id is dropped (no card yet) —
                // the `ToolCallStarted` for a terminal tool always precedes it.
                if let Some(tc) = self.tool_call_mut(tool_call_id) {
                    tc.terminal_id = Some(terminal_id.clone());
                }
            }
            ThreadEvent::ToolKind { tool_call_id, kind } => {
                // Record the ACP tool kind on its card so the renderer can
                // classify it into a rich body. Follows `ToolCallStarted`; an
                // unknown id is dropped (no card to annotate).
                if let Some(tc) = self.tool_call_mut(tool_call_id) {
                    tc.kind = Some(kind.clone());
                }
            }
            ThreadEvent::PermissionRequested {
                request_id, tool_use_id, tool_name, input, description, suggestions, kind,
            } => {
                let req = PermissionRequest {
                    request_id: request_id.clone(),
                    kind: *kind,
                    description: description.clone(),
                    suggestions: suggestions.clone(),
                };
                let matched = tool_use_id
                    .as_deref()
                    .and_then(|id| self.tool_call_mut(id));
                match matched {
                    Some(tc) => tc.status = ToolCallStatus::WaitingForConfirmation(req),
                    None => {
                        // Permission arrived without a matching tool_use block
                        // (unexpected ordering, or a null tool_use_id) —
                        // synthesize the card anyway so the user can still act.
                        let mut tc = ToolCall::new(
                            tool_use_id.clone().unwrap_or_else(|| request_id.clone()),
                            tool_name.clone(),
                            input.clone(),
                        );
                        tc.status = ToolCallStatus::WaitingForConfirmation(req);
                        self.entries.push(ThreadEntry::ToolCall(tc));
                    }
                }
            }
            ThreadEvent::QuestionAsked { request_id, tool_use_id, questions } => {
                // Ignore a malformed/empty question set rather than entering an
                // unanswerable `AwaitingAnswer` — an empty-indexed card render
                // would panic, and there is nothing for the user to answer. The
                // CLI validates 1-4 questions, so this only guards garbled input.
                if !questions.is_empty() {
                    let req = QuestionRequest {
                        request_id: request_id.clone(),
                        questions: questions.clone(),
                    };
                    let matched = tool_use_id.as_deref().and_then(|id| self.tool_call_mut(id));
                    match matched {
                        Some(tc) => tc.status = ToolCallStatus::AwaitingAnswer(req),
                        None => {
                            // No matching tool_use block (unexpected ordering /
                            // null id) — synthesize the card anyway so the user
                            // can still answer, keyed on the tool_use_id (fallback
                            // request_id).
                            let mut tc = ToolCall::new(
                                tool_use_id.clone().unwrap_or_else(|| request_id.clone()),
                                "AskUserQuestion",
                                serde_json::Value::Null,
                            );
                            tc.status = ToolCallStatus::AwaitingAnswer(req);
                            self.entries.push(ThreadEntry::ToolCall(tc));
                        }
                    }
                }
            }
            ThreadEvent::TurnSummary { detail, .. } => {
                self.last_summary = Some(detail.clone());
                self.end_assistant_window();
            }
            // Compaction began — show the pending spinner until the boundary or
            // turn end supersedes it.
            ThreadEvent::CompactionStarted => {
                self.compacting = true;
            }
            ThreadEvent::CompactBoundary { summary } => {
                // Close the current assistant block and drop in a divider so the
                // reclaimed-context gap reads as a boundary, not a silent jump.
                // The boundary supersedes any pending "Compacting…" spinner.
                self.compacting = false;
                self.end_assistant_window();
                self.entries.push(ThreadEntry::ContextCompaction { summary: summary.clone() });
            }
            ThreadEvent::TurnEnded { is_error, result, usage, turn_diff } => {
                self.turn_active = false;
                // Clear a dangling compaction spinner if the turn ended without a
                // boundary event (e.g. a compaction that failed or was aborted).
                self.compacting = false;
                self.end_assistant_window();
                if let Some(u) = usage {
                    // Accumulate real spend (cost is known only at turn-end) and
                    // refresh the window-size cache the live meter reads.
                    if let Some(cost) = u.cost_usd {
                        self.session_cost_usd += cost;
                    }
                    if let Some(w) = u.context_window {
                        self.last_known_context_window = Some(w);
                    }
                    self.usage = Some(u.clone());
                }
                // Consume any stashed diagnostic unconditionally — even on a
                // successful turn — so it can't survive to a later error turn.
                let diagnostic = self.pending_diagnostic.take();
                if *is_error {
                    let mut msg = result.clone().unwrap_or_else(|| "turn ended with error".into());
                    // The diagnostic arrived already de-duped against this text,
                    // so append it as extra detail when it adds something.
                    if let Some(diag) = diagnostic.filter(|d| !d.is_empty()) {
                        msg.push_str("\n\n");
                        msg.push_str(&diag);
                    }
                    self.last_error = Some(msg);
                }
                self.close_turn_with_diff(turn_diff.as_deref());
            }
            ThreadEvent::BackgroundTaskStarted { task_id, tool_use_id, kind, description } => {
                // Idempotent: a duplicate start (defensive against a re-emit) just
                // refreshes the existing record rather than adding a second row.
                if let Some(t) = self.background_task_mut(task_id) {
                    t.tool_use_id = tool_use_id.clone();
                    t.kind = kind.clone();
                    t.description = description.clone();
                } else {
                    self.background_tasks.push(BackgroundTask::started(
                        task_id.clone(),
                        tool_use_id.clone(),
                        kind.clone(),
                        description.clone(),
                    ));
                }
            }
            ThreadEvent::BackgroundTaskProgress { task_id, last_tool } => {
                if let Some(t) = self.background_task_mut(task_id) {
                    t.tool_uses = t.tool_uses.saturating_add(1);
                    if last_tool.is_some() {
                        t.last_tool = last_tool.clone();
                    }
                }
            }
            ThreadEvent::BackgroundTaskFinished {
                task_id,
                failed,
                ended_at_ms,
                summary,
                output_file,
            } => {
                if let Some(t) = self.background_task_mut(task_id) {
                    // Terminal status wins; two completion signals (task_updated +
                    // task_notification) may both arrive — merge, don't overwrite
                    // populated fields with a later `None`.
                    t.status = if *failed { TaskStatus::Failed } else { TaskStatus::Completed };
                    if ended_at_ms.is_some() {
                        t.ended_at_ms = *ended_at_ms;
                    }
                    if summary.is_some() {
                        t.summary = summary.clone();
                    }
                    if output_file.is_some() {
                        t.output_file = output_file.clone();
                    }
                }
            }
            ThreadEvent::PlanUpdated { entries } => {
                // ACP sends a full replacement each time; an empty list clears the
                // plan card rather than pinning an empty one.
                self.plan = (!entries.is_empty()).then(|| entries.clone());
            }
            ThreadEvent::SlashCommandsUpdated { commands, descriptions, hints } => {
                // An explicit mid-session refresh is authoritative (unlike the
                // keep-last-non-empty `SessionInit` seed): the agent is telling us
                // its current command set.
                self.slash_commands = commands.clone();
                // Rebuild the description lookup from the parallel list (only the
                // entries the agent actually described; a blank one is skipped).
                self.slash_command_descriptions = commands
                    .iter()
                    .zip(descriptions.iter())
                    .filter(|(_, d)| !d.is_empty())
                    .map(|(c, d)| (c.clone(), d.clone()))
                    .collect();
                // Same for the parallel argument-hint list.
                self.slash_command_hints = commands
                    .iter()
                    .zip(hints.iter())
                    .filter(|(_, h)| !h.is_empty())
                    .map(|(c, h)| (c.clone(), h.clone()))
                    .collect();
            }
            ThreadEvent::ModeChanged { mode_id } => {
                self.permission_mode = Some(mode_id.clone());
            }
            ThreadEvent::TitleUpdated { title } => {
                self.title = Some(title.clone());
            }
            // Live mid-turn usage → the composer's context meter. Kept separate
            // from the settled `usage` footer; a window size (Codex/ACP carry it
            // live) refreshes the denominator cache.
            ThreadEvent::LiveUsage(u) => {
                if let Some(w) = u.context_window {
                    self.last_known_context_window = Some(w);
                }
                self.live_usage = Some(u.clone());
            }
            // Stash the stderr diagnostic; the next error `TurnEnded` folds it in.
            ThreadEvent::Diagnostic(text) => {
                self.pending_diagnostic = Some(text.clone());
            }
            // A `--resume` hit a session the CLI no longer has. Clear the dead id
            // ONLY when it still equals the one we tried to resume — a fresh id an
            // interleaved `SessionInit` minted (before this error arrived) must
            // survive — then drop a divider notice so the next send omits
            // `--resume` and starts a clean session.
            // A quietly-handled event: same muted divider as the other notices,
            // so it persists with the transcript and can't be mistaken for
            // something the agent said. An agent that retries the same
            // unimplemented request repeats the notice verbatim; collapse those
            // so a retry loop leaves one divider rather than a wall of them
            // (each would also split the streaming reply into another bubble).
            ThreadEvent::Notice(text) => {
                let repeat = matches!(
                    self.entries.last(),
                    Some(ThreadEntry::ContextCompaction { summary }) if summary == text
                );
                if !repeat {
                    self.end_assistant_window();
                    self.entries.push(ThreadEntry::ContextCompaction { summary: text.clone() });
                }
            }
            ThreadEvent::SessionResumeStale { attempted_id } => {
                if self.session_id.as_deref() == Some(attempted_id.as_str()) {
                    self.session_id = None;
                }
                self.end_assistant_window();
                self.entries.push(ThreadEntry::ContextCompaction {
                    summary: "Session no longer resumable — starting fresh".into(),
                });
            }
            // A pure UI-refresh signal: the live vocabulary lives on the connection
            // (which the worker already updated), and the view re-pulls it. Nothing
            // in the transcript changes, so the fold is a no-op.
            ThreadEvent::ControlsUpdated => {}
            // Auth is ephemeral, view-owned card state (mounted by the app's event
            // handler, never persisted in the transcript) — the fold is a no-op.
            ThreadEvent::AuthRequired { .. }
            | ThreadEvent::AuthTerminal { .. }
            | ThreadEvent::AuthUrl { .. }
            | ThreadEvent::AuthOutcome { .. } => {}
            // A child thread's completed action → append to its spawning tool
            // card's capped log. An unknown parent id is dropped like a
            // `ToolResult` for an unknown id — the child outlived its card, or the
            // registration raced (Codex buffers to avoid that). Never touches the
            // root transcript.
            ThreadEvent::SubagentAction { parent_tool_call_id, line } => {
                if let Some(tc) = self.tool_call_mut(parent_tool_call_id) {
                    tc.push_subagent_action(line.clone());
                }
            }
            ThreadEvent::Error(msg) => {
                self.last_error = Some(msg.clone());
                self.turn_active = false;
                self.compacting = false;
            }
            // The producer already performed the rewind; this replays the same
            // truncation onto a subscriber's fold. `truncate_to_user` no-ops on
            // an ordinal it cannot resolve, which is the right answer for a
            // subscriber that joined after the target turn and never held it.
            ThreadEvent::Rewound { ordinal } => {
                self.truncate_to_user(*ordinal);
            }
        }
    }

    /// Mutable lookup of a background task by its `task_id`.
    fn background_task_mut(&mut self, task_id: &str) -> Option<&mut BackgroundTask> {
        self.background_tasks.iter_mut().find(|t| t.task_id == task_id)
    }

    /// Count of background tasks still running — the header badge value.
    pub fn running_task_count(&self) -> usize {
        self.background_tasks.iter().filter(|t| t.is_running()).count()
    }

    /// Interrupt the current turn (the user pressed Stop). Clears the turn flag
    /// (drops the spinner) but deliberately leaves the current assistant block
    /// OPEN: after SIGINT the agent still flushes a few trailing events (more
    /// deltas, then a finalized `assistant` block) before its stdout closes, and
    /// that finalized block must reconcile into the in-progress block rather than
    /// starting a second one — closing the window here would duplicate the reply.
    /// The turn's own `TurnEnded`/EOF closes the window normally afterward.
    /// Fail-closed: any tool call still `WaitingForConfirmation` is downgraded to
    /// `Rejected` — the process is being killed, so the request can never be
    /// answered, exactly like a live disconnect.
    pub fn interrupt(&mut self) {
        self.touch();
        self.turn_active = false;
        // A tool whose arguments were still streaming (`content_block_start` opened
        // the card, the finalized `tool_use` block never arrived) would otherwise
        // show "streaming…" forever after the turn is killed. Stop tracking it and
        // drop the partial buffer so the preview affordance clears.
        self.streaming_tool_id = None;
        for entry in &mut self.entries {
            if let ThreadEntry::ToolCall(tc) = entry {
                if matches!(
                    tc.status,
                    ToolCallStatus::WaitingForConfirmation(_) | ToolCallStatus::AwaitingAnswer(_)
                ) {
                    tc.status = ToolCallStatus::Rejected;
                }
                // Clear a half-streamed args buffer: no more deltas will arrive.
                tc.partial_input.clear();
            }
        }
    }

    /// Ordinal (0-based, counting only user entries) → entry index.
    pub fn user_entry_index(&self, ordinal: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, ThreadEntry::User { .. }))
            .nth(ordinal)
            .map(|(i, _)| i)
    }

    /// Attach the just-created snapshot to the LAST user entry, if it is still
    /// the one at `expected_index` (the thread may have moved on while the
    /// checkpoint was being created in the background).
    pub fn attach_checkpoint(&mut self, expected_index: usize, sha: String) {
        self.touch();
        if let Some(ThreadEntry::User { checkpoint, .. }) = self.entries.get_mut(expected_index) {
            *checkpoint = Some(CheckpointState { sha, show: false });
        }
    }

    /// Flip the "restore files" affordance on the user entry at `index` once
    /// the turn is known to have changed repo state.
    pub fn set_checkpoint_show(&mut self, index: usize, show: bool) {
        self.touch();
        if let Some(ThreadEntry::User { checkpoint: Some(cp), .. }) = self.entries.get_mut(index) {
            cp.show = show;
        }
    }

    /// Rewind: drop the user entry at `ordinal` (0-based among user entries)
    /// and everything after it, returning the removed user message's payload
    /// (for edit-and-resend prefill). Leaves the thread at rest — no turn in
    /// flight, streaming windows closed, stale footer cleared. Returns `None`
    /// (and mutates nothing) when the ordinal doesn't resolve.
    pub fn truncate_to_user(&mut self, ordinal: usize) -> Option<(String, Vec<ChatImage>)> {
        self.touch();
        let idx = self.user_entry_index(ordinal)?;
        let removed = self.entries.drain(idx..).next();
        self.end_assistant_window();
        self.turn_active = false;
        self.usage = None;
        self.last_summary = None;
        match removed {
            Some(ThreadEntry::User { text, images, .. }) => Some((text, images)),
            _ => unreachable!("user_entry_index always indexes a User entry"),
        }
    }

    /// Transition a tool call's status directly (e.g. to `InProgress` when the
    /// user allows, or `Rejected` when they deny) — called by the connection
    /// layer alongside sending the control response.
    pub fn set_tool_status(&mut self, tool_id: &str, status: ToolCallStatus) {
        self.touch();
        if let Some(tc) = self.tool_call_mut(tool_id) {
            tc.status = status;
        }
    }

    /// Record *why* a tool call was refused, alongside the `Rejected` status the
    /// caller is about to set.
    ///
    /// A refusal the user clicked needs no explanation — they wrote it. One
    /// decided by policy carries a sentence the user has never seen: it goes to
    /// the agent in the control response and, without this, nowhere else, so the
    /// card reads as "refused" with no account of what was wrong.
    ///
    /// Kept on `result` rather than folded into the status so the card keeps its
    /// refused glyph instead of reading as a tool that failed — those are
    /// different things and the transcript should not blur them. Never
    /// overwrites a result the backend already reported.
    pub fn set_tool_refusal(&mut self, tool_id: &str, reason: &str) {
        self.touch();
        if let Some(tc) = self.tool_call_mut(tool_id)
            && tc.result.is_none()
        {
            tc.result = Some(reason.to_string());
        }
    }

    /// Close a turn with its files-changed card, when it changed anything.
    ///
    /// `wire_diff` is the backend's own turn diff (Codex); it is authoritative
    /// because it also sees files written by shell commands. Without one, the
    /// turn's edit cards are summed instead, and no diff is recorded — Review
    /// needs real hunks, and edit cards carry fragments.
    ///
    /// A turn that changed nothing adds no entry, so a conversational turn costs
    /// nothing and looks unchanged.
    fn close_turn_with_diff(&mut self, wire_diff: Option<&str>) {
        let (files, diff) = match wire_diff {
            Some(d) => (turn_diff::stats_from_unified_diff(d), Some(d.to_string())),
            None => (turn_diff::stats_from_turn_entries(&self.entries), None),
        };
        if files.is_empty() {
            return;
        }
        self.entries.push(ThreadEntry::TurnDiff { files, diff });
    }

    /// Mark a tool call as having answered a secret question, so the fold redacts
    /// the result the backend echoes back. Called when the answer is sent —
    /// answering replaces the `AwaitingAnswer` status that carries `is_secret`,
    /// so this is the last moment the secret-ness is knowable.
    pub fn mark_secret_answer(&mut self, tool_id: &str) {
        self.touch();
        if let Some(tc) = self.tool_call_mut(tool_id) {
            tc.redact_result = true;
        }
    }

    /// The pending permission request (if any) awaiting a decision.
    pub fn pending_permission(&self) -> Option<(&str, &PermissionRequest)> {
        self.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) => match &tc.status {
                ToolCallStatus::WaitingForConfirmation(req) => Some((tc.id.as_str(), req)),
                _ => None,
            },
            _ => None,
        })
    }

    /// The pending AskUserQuestion (if any) awaiting the user's answers.
    pub fn pending_question(&self) -> Option<(&str, &QuestionRequest)> {
        self.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) => match &tc.status {
                ToolCallStatus::AwaitingAnswer(req) => Some((tc.id.as_str(), req)),
                _ => None,
            },
            _ => None,
        })
    }

    fn tool_call_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.entries.iter_mut().rev().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(tc),
            _ => None,
        })
    }

    /// Close the current assistant message: subsequent text/thinking starts a
    /// fresh entry, and the streamed-vs-finalized reconciliation flags reset.
    fn end_assistant_window(&mut self) {
        self.current_assistant = None;
        self.text_streaming = false;
        self.thinking_streaming = false;
    }

    /// The assistant message currently being built, creating one if the last
    /// entry isn't an in-progress assistant message.
    fn assistant_mut(&mut self) -> &mut AssistantMessage {
        if self.current_assistant.is_none() {
            self.entries.push(ThreadEntry::Assistant(AssistantMessage::default()));
            self.current_assistant = Some(self.entries.len() - 1);
        }
        let idx = self.current_assistant.expect("just set");
        match &mut self.entries[idx] {
            ThreadEntry::Assistant(m) => m,
            _ => unreachable!("current_assistant always indexes an Assistant entry"),
        }
    }
}

#[cfg(test)]
mod streaming_interrupt_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interrupt_clears_streaming_tool_preview() {
        // A tool whose args were still streaming when Stop is pressed must not
        // stay stuck showing a live preview forever.
        let mut t = ChatThread::new();
        t.push_user_message("write a file");
        t.apply(&ThreadEvent::ToolCallStarted { id: "tw".into(), name: "Write".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolInputDelta {
            tool_call_id: String::new(),
            partial_json: r#"{"file_path": "/tmp/x", "content": "abc"#.into(),
        });
        // Mid-stream: the card previews the growing args.
        let streaming = t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.name == "Write" => Some(tc),
            _ => None,
        }).unwrap();
        assert!(streaming.preview_input().is_some(), "previews while streaming");

        t.interrupt();
        // After Stop: no partial buffer left, nothing to preview.
        let stopped = t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.name == "Write" => Some(tc),
            _ => None,
        }).unwrap();
        assert!(stopped.partial_input.is_empty(), "partial buffer cleared on interrupt");
        assert!(stopped.preview_input().is_none(), "no stuck streaming preview");
    }
}

/// Whether a tool-call `input` value carries no arguments yet — `null` or an
/// empty object, the shape a live `content_block_start` opens a tool card with
/// before any `input_json_delta` fragment has arrived. A finalized `tool_use`
/// block always carries a populated (or at least non-empty) object.
fn is_empty_input(input: &serde_json::Value) -> bool {
    match input {
        serde_json::Value::Null => true,
        serde_json::Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::background_task::BackgroundTaskKind;
    use super::super::tool_call::PermissionKind;
    use super::super::turn_diff::TurnFileChange;
    use serde_json::json;

    fn assistant_text(e: &ThreadEntry) -> &str {
        match e {
            ThreadEntry::Assistant(m) => &m.text,
            _ => panic!("not an assistant entry"),
        }
    }

    /// A remote fold sees the user's half only through this event — no backend
    /// echoes the prompt back — so it must land as a real `User` entry, not be
    /// dropped as an unknown variant.
    #[test]
    fn user_message_event_folds_into_a_user_entry() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::UserMessage { text: "hi".into(), images: vec![] });
        t.apply(&ThreadEvent::AssistantText("hello".into()));
        assert!(
            matches!(&t.entries[0], ThreadEntry::User { text, .. } if text == "hi"),
            "the prompt folds into a User entry, saw {:?}",
            t.entries[0],
        );
        assert_eq!(t.entries.len(), 2, "one user entry + one assistant entry");
    }

    /// The event and the desktop's direct push must agree, or a phone and the
    /// desktop would render the same prompt differently.
    #[test]
    fn user_message_event_matches_a_direct_push() {
        let (mut evented, mut pushed) = (ChatThread::new(), ChatThread::new());
        evented.apply(&ThreadEvent::UserMessage { text: "hi".into(), images: vec![] });
        pushed.push_user_message_with_images("hi", vec![]);
        assert_eq!(evented.entries, pushed.entries);
    }

    #[test]
    fn builds_a_simple_turn() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        t.apply(&ThreadEvent::AssistantTextDelta("Hel".into()));
        t.apply(&ThreadEvent::AssistantTextDelta("lo".into()));
        t.apply(&ThreadEvent::AssistantText("Hello!".into())); // finalize reconciles
        t.apply(&ThreadEvent::TurnEnded { result: Some("Hello!".into()), usage: None, is_error: false, turn_diff: None });

        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[0], ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None });
        assert_eq!(assistant_text(&t.entries[1]), "Hello!"); // not "Hellolo Hello!"
        assert!(!t.turn_active);
    }

    #[test]
    fn multiple_finalized_text_blocks_append_not_overwrite() {
        // A single assistant message carrying two top-level text blocks (no
        // tool_use between) must keep both — not silently drop the first.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantText("First.".into()));
        t.apply(&ThreadEvent::AssistantText("Second.".into()));
        let joined = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.clone()), _ => None }).collect::<Vec<_>>();
        assert_eq!(joined, vec!["First.\nSecond.".to_string()]);
    }

    #[test]
    fn streamed_then_multi_finalized_reconciles_then_appends() {
        // Deltas stream the first block; the first finalized replaces the
        // stream; a second finalized block appends.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantTextDelta("Fir".into()));
        t.apply(&ThreadEvent::AssistantTextDelta("st.".into()));
        t.apply(&ThreadEvent::AssistantText("First.".into())); // reconcile
        t.apply(&ThreadEvent::AssistantText("Second.".into())); // append
        assert_eq!(assistant_text(&t.entries[1]), "First.\nSecond.");
    }

    #[test]
    fn tool_call_flows_to_completed() {
        let mut t = ChatThread::new();
        t.push_user_message("read it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Read".into(), input: json!({"file_path":"a"}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_1".into(), content: "file body".into(), is_error: false,
            structured: None });

        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.name, "Read");
                assert_eq!(tc.status, ToolCallStatus::Completed);
                assert_eq!(tc.result.as_deref(), Some("file body"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn subagent_action_appends_to_parent_card_and_drops_unknown_parent() {
        let mut t = ChatThread::new();
        t.push_user_message("delegate");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_agent".into(), name: "Agent".into(), input: json!({"description":"scout"}) });
        t.apply(&ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_agent".into(), line: "Read src/main.rs".into() });
        t.apply(&ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_agent".into(), line: "Bash cargo test".into() });
        // An action for a parent id with no card is dropped (no panic, no leak).
        t.apply(&ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_missing".into(), line: "orphan".into() });

        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.name, "Agent");
                assert_eq!(tc.subagent_log, vec!["Read src/main.rs".to_string(), "Bash cargo test".to_string()]);
            }
            other => panic!("expected Agent ToolCall, got {other:?}"),
        }
        // No stray entry from the orphaned action.
        assert_eq!(t.entries.len(), 2, "orphan action created no entry");
    }

    #[test]
    fn tool_result_structured_settles_and_survives_bare_followup() {
        // The live wire's top-level `tool_use_result` must land in
        // `tc.structured` so the shared renderers (Bash stderr split, subagent
        // stats, interrupted badge) enrich live chats — not just imported
        // history. A later bare result must not wipe what was captured.
        let mut t = ChatThread::new();
        t.push_user_message("run it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "tb".into(), name: "Bash".into(), input: json!({"command":"x"}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "tb".into(), content: "done".into(), is_error: false,
            structured: Some(json!({"stdout":"","stderr":"boom","interrupted":true})) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "tb".into(), content: "done".into(), is_error: false,
            structured: None });

        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::Completed);
                assert_eq!(
                    tc.structured,
                    Some(json!({"stdout":"","stderr":"boom","interrupted":true})),
                    "structured captured, and not clobbered by the bare follow-up",
                );
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_for_unknown_id_is_ignored() {
        // A result whose id matches no tool call must not panic or corrupt the
        // existing tool call — it is simply dropped.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Read".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_UNKNOWN".into(), content: "stray".into(), is_error: false,
            structured: None });
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::InProgress, "existing tool call untouched");
                assert!(tc.result.is_none());
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn permission_marks_matching_tool_call_and_is_findable() {
        let mut t = ChatThread::new();
        t.push_user_message("edit");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_9".into(), name: "Edit".into(), input: json!({}) });
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "rid-1".into(), tool_use_id: Some("toolu_9".into()),
            tool_name: "Edit".into(), input: json!({}), description: "notes.txt".into(),
            suggestions: vec![], kind: PermissionKind::Tool });

        let (tool_id, req) = t.pending_permission().expect("a pending permission");
        assert_eq!(tool_id, "toolu_9");
        assert_eq!(req.request_id, "rid-1");
        assert_eq!(req.description, "notes.txt");
        // only one tool-call entry was created (permission matched, not duplicated)
        assert_eq!(t.entries.iter().filter(|e| matches!(e, ThreadEntry::ToolCall(_))).count(), 1);

        // deny → rejected, no longer pending
        t.set_tool_status("toolu_9", ToolCallStatus::Rejected);
        assert!(t.pending_permission().is_none());
    }

    #[test]
    fn question_asked_marks_tool_call_and_settles_on_result() {
        use crate::thread::question::parse_questions;
        let mut t = ChatThread::new();
        t.push_user_message("choose");
        let input = json!({"questions":[{"question":"Tabs or spaces?","header":"Indent",
            "options":[{"label":"Tabs","description":""},{"label":"Spaces","description":""}],
            "multiSelect":false}]});
        // The tool_use block lands first, then the AskUserQuestion control_request.
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_q".into(), name: "AskUserQuestion".into(), input: input.clone() });
        t.apply(&ThreadEvent::QuestionAsked {
            request_id: "rid-q".into(), tool_use_id: Some("toolu_q".into()),
            questions: parse_questions(&input) });

        let (tool_id, req) = t.pending_question().expect("a pending question");
        assert_eq!(tool_id, "toolu_q");
        assert_eq!(req.request_id, "rid-q");
        assert_eq!(req.questions.len(), 1);
        assert!(t.pending_permission().is_none(), "a question is not a permission");
        // folded onto the existing tool_use — not duplicated
        assert_eq!(t.entries.iter().filter(|e| matches!(e, ThreadEntry::ToolCall(_))).count(), 1);

        // the real tool_result (after we answer) settles it to Completed
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_q".into(),
            content: "Your questions have been answered: \"Tabs or spaces?\"=\"Tabs\".".into(),
            is_error: false, structured: None });
        assert!(t.pending_question().is_none());
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Completed),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// The `TurnDiff` entry a turn closed with, if any.
    fn turn_card(t: &ChatThread) -> Option<(&Vec<TurnFileChange>, &Option<String>)> {
        t.entries.iter().find_map(|e| match e {
            ThreadEntry::TurnDiff { files, diff } => Some((files, diff)),
            _ => None,
        })
    }

    fn ended(turn_diff: Option<&str>) -> ThreadEvent {
        ThreadEvent::TurnEnded {
            result: None,
            usage: None,
            is_error: false,
            turn_diff: turn_diff.map(str::to_string),
        }
    }

    #[test]
    fn a_codex_turn_closes_with_its_wire_diff() {
        let mut t = ChatThread::new();
        t.push_user_message("edit it");
        t.apply(&ended(Some(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n ctx\n+added\n",
        )));
        let (files, diff) = turn_card(&t).expect("an editing turn closes with a card");
        assert_eq!(files, &vec![TurnFileChange { path: "a.rs".into(), added: 1, removed: 0 }]);
        assert!(diff.is_some(), "the wire diff is kept so Review has real hunks");
    }

    #[test]
    fn a_claude_turn_closes_with_stats_derived_from_its_edit_cards() {
        let mut t = ChatThread::new();
        t.push_user_message("edit it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "e1".into(),
            name: "Edit".into(),
            input: json!({"file_path": "b.rs", "old_string": "x", "new_string": "y\nz"}),
        });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "e1".into(), content: "ok".into(), is_error: false, structured: None });
        // No wire diff — the fold sums the turn's own edit cards instead.
        t.apply(&ended(None));
        let (files, diff) = turn_card(&t).expect("an editing turn closes with a card");
        assert_eq!(files, &vec![TurnFileChange { path: "b.rs".into(), added: 2, removed: 1 }]);
        assert!(diff.is_none(), "no diff is fabricated from fragment-shaped edit cards");
    }

    #[test]
    fn a_turn_that_changed_nothing_closes_with_no_card() {
        let mut t = ChatThread::new();
        t.push_user_message("just talk");
        t.apply(&ThreadEvent::AssistantText("hello".into()));
        t.apply(&ended(None));
        assert!(turn_card(&t).is_none(), "a conversational turn must not add a card");
        // …and neither does a wire diff that is empty or unparseable.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ended(Some("")));
        assert!(turn_card(&t).is_none());
    }

    #[test]
    fn each_turns_card_counts_only_that_turn() {
        let mut t = ChatThread::new();
        t.push_user_message("one");
        t.apply(&ended(Some("diff --git a/a.rs b/a.rs\n+++ b/a.rs\n@@ -0,0 +1 @@\n+one\n")));
        t.push_user_message("two");
        t.apply(&ended(Some("diff --git a/b.rs b/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+two\n")));
        let cards: Vec<_> = t
            .entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::TurnDiff { files, .. } => Some(files),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 2, "one card per editing turn");
        assert_eq!(cards[0][0].path, "a.rs");
        assert_eq!(cards[1][0].path, "b.rs", "the second turn must not restate the first");
    }

    #[test]
    fn a_transcript_written_before_turn_cards_still_loads() {
        // The blob predates `TurnDiff`, so it simply has no such entry — the new
        // variant must not make an old persisted transcript unreadable.
        let old = r#"[{"User":{"text":"hi"}},{"Assistant":{"text":"hey","thinking":""}}]"#;
        let entries: Vec<ThreadEntry> =
            serde_json::from_str(old).expect("an older transcript blob still loads");
        assert_eq!(entries.len(), 2);
        // …and a card round-trips through the same blob format.
        let with_card = vec![ThreadEntry::TurnDiff {
            files: vec![TurnFileChange { path: "a.rs".into(), added: 1, removed: 0 }],
            diff: Some("diff --git a/a.rs b/a.rs\n".into()),
        }];
        let blob = serde_json::to_string(&with_card).expect("serialize");
        let back: Vec<ThreadEntry> = serde_json::from_str(&blob).expect("round-trip");
        assert_eq!(back, with_card);
    }

    #[test]
    fn a_secret_answers_echo_is_redacted_before_it_can_be_persisted() {
        use crate::thread::question::parse_questions;
        let mut t = ChatThread::new();
        t.push_user_message("deploy");
        let input = json!({"questions":[{"question":"API token?","header":"Token",
            "options":[],"multiSelect":false}]});
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_s".into(), name: "AskUserQuestion".into(), input: input.clone() });
        let mut questions = parse_questions(&input);
        questions[0].is_secret = true;
        t.apply(&ThreadEvent::QuestionAsked {
            request_id: "rid-s".into(), tool_use_id: Some("toolu_s".into()), questions });

        // Answering is the last moment `is_secret` is knowable — the awaiting
        // status that carries it is replaced right after.
        t.mark_secret_answer("toolu_s");
        t.set_tool_status("toolu_s", ToolCallStatus::InProgress);

        // The backend echoes the credential back in its result…
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_s".into(),
            content: "Your questions have been answered: \"API token?\"=\"sk-live-abc123\".".into(),
            is_error: false,
            structured: Some(json!({"answers": {"API token?": "sk-live-abc123"}})) });

        let tc = match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => tc,
            other => panic!("expected a tool call, got {other:?}"),
        };
        assert_eq!(tc.status, ToolCallStatus::Completed, "redacting must not break settling");
        assert_eq!(tc.result.as_deref(), Some(SECRET_PLACEHOLDER));
        assert!(tc.structured.is_none(), "the structured echo carries the secret too");

        // The real guarantee: the secret is nowhere in what gets written to disk.
        let blob = serde_json::to_string(&t.entries).expect("entries serialize");
        assert!(!blob.contains("sk-live-abc123"), "the secret must not reach the transcript blob");
        assert!(blob.contains(SECRET_PLACEHOLDER));
    }

    #[test]
    fn a_non_secret_answer_is_still_recorded_verbatim() {
        // The redaction must be scoped to secret questions — a normal answer is
        // transcript content the user wants to keep.
        use crate::thread::question::parse_questions;
        let mut t = ChatThread::new();
        t.push_user_message("choose");
        let input = json!({"questions":[{"question":"Tabs or spaces?","header":"Indent",
            "options":[{"label":"Tabs","description":""}],"multiSelect":false}]});
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_q".into(), name: "AskUserQuestion".into(), input: input.clone() });
        t.apply(&ThreadEvent::QuestionAsked {
            request_id: "rid-q".into(), tool_use_id: Some("toolu_q".into()),
            questions: parse_questions(&input) });
        t.set_tool_status("toolu_q", ToolCallStatus::InProgress);
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_q".into(),
            content: "Your questions have been answered: \"Tabs or spaces?\"=\"Tabs\".".into(),
            is_error: false, structured: None });
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert!(tc.result.as_deref().expect("a result").contains("Tabs"));
                assert!(!tc.redact_result);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn rehydrate_and_interrupt_fail_close_a_pending_question() {
        use crate::thread::question::parse_questions;
        let input = json!({"questions":[{"question":"Q?","header":"H",
            "options":[{"label":"A","description":""}],"multiSelect":false}]});
        let mut tc = ToolCall::new("toolu_q", "AskUserQuestion", input.clone());
        tc.status = ToolCallStatus::AwaitingAnswer(QuestionRequest {
            request_id: "rid".into(), questions: parse_questions(&input) });

        // rehydrate fail-closes (the process that asked is gone)
        let t = ChatThread::rehydrated(None, None, vec![ThreadEntry::ToolCall(tc.clone())], vec![]);
        assert!(t.pending_question().is_none(), "no pending question survives restore");
        match &t.entries[0] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected Rejected, got {other:?}"),
        }

        // interrupt (Stop) also fail-closes
        let mut t2 = ChatThread::new();
        t2.entries.push(ThreadEntry::ToolCall(tc));
        t2.turn_active = true;
        t2.interrupt();
        assert!(t2.pending_question().is_none(), "interrupt fail-closes the question");
    }

    #[test]
    fn empty_question_set_is_ignored_not_stranded() {
        // A malformed/garbled AskUserQuestion with no questions must NOT become
        // an AwaitingAnswer (an empty-indexed card render would panic) — the
        // event is dropped and the tool_use is left untouched.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(),
            name: "AskUserQuestion".into(),
            input: json!({"questions": []}),
        });
        t.apply(&ThreadEvent::QuestionAsked {
            request_id: "rq".into(),
            tool_use_id: Some("t1".into()),
            questions: vec![],
        });
        assert!(t.pending_question().is_none(), "empty questions never becomes pending");
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::InProgress, "tool_use left untouched");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn orphan_permission_synthesizes_a_tool_call() {
        // Permission arrives with no matching tool_use (null id, or the
        // tool_use event was lost) — the card is synthesized so the user can
        // still allow/deny, keyed on the request_id.
        let mut t = ChatThread::new();
        t.push_user_message("do it");
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "rid-orphan".into(), tool_use_id: None,
            tool_name: "Bash".into(), input: json!({"command":"ls"}),
            description: "ls".into(), suggestions: vec![], kind: PermissionKind::Tool });

        let (tool_id, req) = t.pending_permission().expect("synthesized pending permission");
        assert_eq!(tool_id, "rid-orphan", "falls back to request_id as the tool id");
        assert_eq!(req.request_id, "rid-orphan");
        match t.entries.iter().find(|e| matches!(e, ThreadEntry::ToolCall(_))) {
            Some(ThreadEntry::ToolCall(tc)) => assert_eq!(tc.name, "Bash"),
            _ => panic!("a synthesized ToolCall entry should exist"),
        }
    }

    #[test]
    fn rehydrated_seeds_entries_and_session_and_rests_flags() {
        let entries = vec![
            ThreadEntry::User { text: "hi".into(), images: vec![], checkpoint: None },
            ThreadEntry::Assistant(AssistantMessage { text: "hello".into(), thinking: String::new() }),
        ];
        let t = ChatThread::rehydrated(Some("sid-9".into()), Some("opus".into()), entries, vec![]);
        assert_eq!(t.session_id.as_deref(), Some("sid-9"));
        assert_eq!(t.model.as_deref(), Some("opus"));
        assert_eq!(t.entries.len(), 2);
        assert!(!t.turn_active, "a restored thread has no turn in flight");
        assert!(t.pending_permission().is_none());
    }

    #[test]
    fn rehydrated_fail_closes_a_pending_permission() {
        // A tool call left WaitingForConfirmation at quit must restore as
        // Rejected — the process that asked is gone, so it can never be
        // answered; leaving it pending would strand an unanswerable prompt.
        let req = PermissionRequest {
            request_id: "rid".into(),
            kind: PermissionKind::Tool,
            description: "notes.txt".into(),
            suggestions: vec![],
        };
        let mut tc = ToolCall::new("toolu_1", "Edit", serde_json::json!({}));
        tc.status = ToolCallStatus::WaitingForConfirmation(req);
        let t = ChatThread::rehydrated(None, None, vec![ThreadEntry::ToolCall(tc)], vec![]);
        assert!(t.pending_permission().is_none(), "no pending permission survives restore");
        match &t.entries[0] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn turn_ended_stores_usage_for_the_footer() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        assert!(t.usage.is_none(), "no usage before a turn ends");
        t.apply(&ThreadEvent::TurnEnded {
            result: Some("done".into()),
            usage: Some(TurnUsage {
                input_tokens: 714, output_tokens: 7,
                cache_read_tokens: 16681, cache_creation_tokens: 5571,
                context_window: Some(200000), cost_usd: Some(0.33),
            }),
            is_error: false, turn_diff: None });
        let u = t.usage.as_ref().expect("usage stored");
        assert_eq!(u.input_tokens, 714);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cost_usd, Some(0.33));
    }

    #[test]
    fn new_turn_clears_prior_usage_and_summary() {
        // The footer must not carry a prior turn's numbers into the next turn.
        let mut t = ChatThread::new();
        t.push_user_message("q1");
        t.apply(&ThreadEvent::TurnSummary { detail: "did a thing".into(), category: "x".into() });
        t.apply(&ThreadEvent::TurnEnded {
            result: Some("a".into()),
            usage: Some(TurnUsage { input_tokens: 100, ..Default::default() }),
            is_error: false, turn_diff: None,
        });
        assert!(t.usage.is_some() && t.last_summary.is_some());

        // Sending the next prompt wipes the stale footer/summary.
        t.push_user_message("q2");
        assert!(t.usage.is_none(), "prior usage cleared on a new turn");
        assert!(t.last_summary.is_none(), "prior summary cleared on a new turn");
    }

    #[test]
    fn interrupt_ends_turn_and_fail_closes_pending_permission() {
        // Stop mid-turn: the turn flag clears and a pending approval downgrades
        // to Rejected (the process is being killed and can never answer it).
        let mut t = ChatThread::new();
        t.push_user_message("edit it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(), name: "Edit".into(), input: json!({}) });
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "r1".into(), tool_use_id: Some("t1".into()),
            tool_name: "Edit".into(), input: json!({}), description: "x".into(),
            suggestions: vec![], kind: PermissionKind::Tool });
        assert!(t.turn_active);
        assert!(t.pending_permission().is_some());

        t.interrupt();

        assert!(!t.turn_active, "interrupt ends the turn");
        assert!(t.pending_permission().is_none(), "pending approval is fail-closed");
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn interrupt_does_not_duplicate_the_trailing_finalized_block() {
        // After Stop, the agent flushes trailing deltas + a finalized `assistant`
        // block before EOF. That finalized text must reconcile INTO the streamed
        // block, not spawn a second identical one. (Interrupt must not close the
        // assistant window, or the reply doubles.)
        let mut t = ChatThread::new();
        t.push_user_message("essay");
        t.apply(&ThreadEvent::AssistantTextDelta("The History".into()));
        t.interrupt(); // user hits Stop mid-stream
        // trailing events that arrive after SIGINT, before stdout EOF:
        t.apply(&ThreadEvent::AssistantTextDelta(" and Design".into()));
        t.apply(&ThreadEvent::AssistantText("The History and Design of Rust".into()));
        t.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: true, turn_diff: None });

        let assistants: Vec<&str> = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.as_str()), _ => None }).collect();
        assert_eq!(
            assistants,
            vec!["The History and Design of Rust"],
            "exactly one assistant block, reconciled — not doubled"
        );
    }

    #[test]
    fn transcript_survives_a_serde_round_trip() {
        // The persisted-blob path serializes the entry list and reloads it.
        // Prove a representative transcript (user + assistant + completed tool)
        // round-trips byte-for-byte through serde_json.
        let mut src = ChatThread::new();
        src.push_user_message("read it");
        src.apply(&ThreadEvent::AssistantText("On it.".into()));
        src.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(), name: "Read".into(), input: json!({"file_path":"a"}) });
        src.apply(&ThreadEvent::ToolResult {
            tool_use_id: "t1".into(), content: "body".into(), is_error: false,
            structured: None });

        let json = serde_json::to_string(&src.entries).expect("serialize entries");
        let back: Vec<ThreadEntry> = serde_json::from_str(&json).expect("deserialize entries");
        assert_eq!(back, src.entries);
    }

    /// A remote subscriber folds `Rewound` into the same truncation the desktop
    /// performed directly. Without this the phone keeps the dropped tail and
    /// then appends the replacement turns after it.
    #[test]
    fn rewound_truncates_a_subscribers_fold() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::UserMessage { text: "one".into(), images: vec![] });
        t.apply(&ThreadEvent::AssistantText("A1".into()));
        t.apply(&ThreadEvent::UserMessage { text: "two".into(), images: vec![] });
        t.apply(&ThreadEvent::AssistantText("A2".into()));
        assert_eq!(t.entries.len(), 4);

        t.apply(&ThreadEvent::Rewound { ordinal: 1 });

        assert_eq!(t.entries.len(), 2, "'two' and its reply are gone");
        assert!(
            !t.entries.iter().any(|e| matches!(e, ThreadEntry::User { text, .. } if text == "two")),
            "the rewound turn is not still in the transcript",
        );
        // The replacement turn appends onto the truncated thread, not after a
        // stale tail — the whole point of propagating the truncation.
        t.apply(&ThreadEvent::UserMessage { text: "two-redo".into(), images: vec![] });
        assert_eq!(t.entries.len(), 3);
    }

    /// A subscriber that joined mid-session never held the target turn. The
    /// ordinal cannot resolve against its shorter fold, and dropping rows it
    /// *did* legitimately receive would be worse than ignoring the event.
    #[test]
    fn rewound_past_a_late_subscribers_history_changes_nothing() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::UserMessage { text: "only".into(), images: vec![] });
        t.apply(&ThreadEvent::AssistantText("A".into()));

        t.apply(&ThreadEvent::Rewound { ordinal: 7 });

        assert_eq!(t.entries.len(), 2, "an unresolvable ordinal truncates nothing");
    }

    #[test]
    fn checkpoint_attach_show_and_truncate() {
        let mut t = ChatThread::new();
        t.push_user_message("one");
        t.apply(&ThreadEvent::AssistantText("A1".into()));
        t.push_user_message("two");
        t.apply(&ThreadEvent::AssistantText("A2".into()));
        t.push_user_message("three");
        t.apply(&ThreadEvent::AssistantText("A3".into()));

        // Attach a checkpoint to user #1 ("two") and flip its show flag.
        let idx = t.user_entry_index(1).expect("second user entry");
        t.attach_checkpoint(idx, "sha-2".into());
        t.set_checkpoint_show(idx, true);
        match &t.entries[idx] {
            ThreadEntry::User { checkpoint: Some(cp), .. } => {
                assert_eq!(cp.sha, "sha-2");
                assert!(cp.show);
            }
            other => panic!("expected checkpointed user entry, got {other:?}"),
        }

        // Rewind to "two": it and everything after are dropped; payload returned.
        let (text, images) = t.truncate_to_user(1).expect("truncate succeeds");
        assert_eq!(text, "two");
        assert!(images.is_empty());
        assert_eq!(t.entries.len(), 2, "only 'one' + 'A1' remain");
        assert!(!t.turn_active);
        assert!(t.usage.is_none() && t.last_summary.is_none());

        // Out-of-range ordinal mutates nothing.
        assert!(t.truncate_to_user(5).is_none());
        assert_eq!(t.entries.len(), 2);
    }

    #[test]
    fn attach_checkpoint_skips_non_user_index() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        t.apply(&ThreadEvent::AssistantText("yo".into()));
        // Index 1 is the assistant entry — attach must be a no-op, not a panic.
        t.attach_checkpoint(1, "sha-x".into());
        assert!(matches!(&t.entries[1], ThreadEntry::Assistant(_)));
    }

    #[test]
    fn checkpointed_entry_survives_serde_and_old_blobs_default() {
        let mut t = ChatThread::new();
        t.push_user_message("snap");
        t.attach_checkpoint(0, "abc123".into());
        let json = serde_json::to_string(&t.entries).unwrap();
        let back: Vec<ThreadEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t.entries);

        // A pre-checkpoint blob (no field at all) still loads.
        let old = r#"[{"User":{"text":"legacy","images":[]}}]"#;
        let entries: Vec<ThreadEntry> = serde_json::from_str(old).unwrap();
        match &entries[0] {
            ThreadEntry::User { checkpoint, .. } => assert!(checkpoint.is_none()),
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn tool_terminal_binds_terminal_id_to_its_card() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "call-t".into(),
            name: "Run build".into(),
            input: json!({}),
        });
        t.apply(&ThreadEvent::ToolTerminal {
            tool_call_id: "call-t".into(),
            terminal_id: "term-7".into(),
        });
        match t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "call-t" => Some(tc),
            _ => None,
        }) {
            Some(tc) => assert_eq!(tc.terminal_id.as_deref(), Some("term-7")),
            None => panic!("terminal tool card missing"),
        }
        // An orphan bind (no matching card) is dropped, not a panic.
        t.apply(&ThreadEvent::ToolTerminal {
            tool_call_id: "nope".into(),
            terminal_id: "x".into(),
        });
    }

    #[test]
    fn tool_kind_binds_kind_to_its_card() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "call-k".into(),
            name: "Run the build".into(),
            input: json!({}),
        });
        t.apply(&ThreadEvent::ToolKind {
            tool_call_id: "call-k".into(),
            kind: "execute".into(),
        });
        match t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "call-k" => Some(tc),
            _ => None,
        }) {
            Some(tc) => assert_eq!(tc.kind.as_deref(), Some("execute")),
            None => panic!("kind tool card missing"),
        }
        // An orphan kind (no matching card) is dropped, not a panic.
        t.apply(&ThreadEvent::ToolKind { tool_call_id: "nope".into(), kind: "read".into() });
    }

    #[test]
    fn text_then_tool_then_text_makes_two_assistant_messages() {
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantText("Let me check.".into()));
        t.apply(&ThreadEvent::ToolCallStarted { id: "t1".into(), name: "Bash".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult { tool_use_id: "t1".into(), content: "ok".into(), is_error: false, structured: None });
        t.apply(&ThreadEvent::AssistantText("Done.".into()));

        let assistants: Vec<&str> = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.as_str()), _ => None }).collect();
        assert_eq!(assistants, vec!["Let me check.", "Done."]);
        // order: User, Assistant, ToolCall, Assistant
        assert!(matches!(t.entries[0], ThreadEntry::User { .. }));
        assert!(matches!(t.entries[1], ThreadEntry::Assistant(_)));
        assert!(matches!(t.entries[2], ThreadEntry::ToolCall(_)));
        assert!(matches!(t.entries[3], ThreadEntry::Assistant(_)));
    }

    #[test]
    fn background_task_lifecycle_folds_into_state() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::BackgroundTaskStarted {
            task_id: "T1".into(),
            tool_use_id: "toolu_a".into(),
            kind: BackgroundTaskKind::Subagent { subagent_type: "general-purpose".into() },
            description: "read files".into(),
        });
        assert_eq!(t.running_task_count(), 1);
        t.apply(&ThreadEvent::BackgroundTaskProgress {
            task_id: "T1".into(),
            last_tool: Some("Read".into()),
        });
        t.apply(&ThreadEvent::BackgroundTaskProgress {
            task_id: "T1".into(),
            last_tool: Some("Read".into()),
        });
        let task = &t.background_tasks[0];
        assert_eq!(task.tool_uses, 2);
        assert_eq!(task.last_tool.as_deref(), Some("Read"));
        assert!(task.is_running());

        // task_updated (end_time) then task_notification (summary+output_file) —
        // both terminal signals merge, neither clobbers the other's fields.
        t.apply(&ThreadEvent::BackgroundTaskFinished {
            task_id: "T1".into(),
            failed: false,
            ended_at_ms: Some(1000),
            summary: None,
            output_file: None,
        });
        t.apply(&ThreadEvent::BackgroundTaskFinished {
            task_id: "T1".into(),
            failed: false,
            ended_at_ms: None,
            summary: Some("done".into()),
            output_file: Some("/tmp/x.jsonl".into()),
        });
        let task = &t.background_tasks[0];
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.ended_at_ms, Some(1000), "end_time not clobbered by the later None");
        assert_eq!(task.summary.as_deref(), Some("done"));
        assert!(task.has_transcript());
        assert_eq!(t.running_task_count(), 0);
    }

    #[test]
    fn background_task_finished_failed_marks_failed() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::BackgroundTaskStarted {
            task_id: "T1".into(),
            tool_use_id: "toolu_a".into(),
            kind: BackgroundTaskKind::BackgroundBash,
            description: "boom".into(),
        });
        t.apply(&ThreadEvent::BackgroundTaskFinished {
            task_id: "T1".into(),
            failed: true,
            ended_at_ms: None,
            summary: None,
            output_file: None,
        });
        assert_eq!(t.background_tasks[0].status, TaskStatus::Failed);
    }

    /// End-to-end: folding the captured spike stream tracks exactly the two tasks,
    /// both completed, without polluting the transcript with subagent chatter.
    #[test]
    fn spike_fixture_folds_to_two_completed_tasks() {
        let fixture = include_str!("testdata/stream_json_subagent.jsonl");
        let mut t = ChatThread::new();
        for line in fixture.lines() {
            for ev in super::super::stream_json::decode_line(line) {
                t.apply(&ev);
            }
        }
        assert_eq!(t.background_tasks.len(), 2, "one subagent + one bash");
        assert!(t.background_tasks.iter().all(|task| task.status == TaskStatus::Completed));
        assert_eq!(t.running_task_count(), 0);
        // Both carry an output_file for "View transcript".
        assert!(t.background_tasks.iter().all(|task| task.has_transcript()));
        // The subagent ran tools; its activity counter advanced.
        let subagent = t
            .background_tasks
            .iter()
            .find(|task| matches!(task.kind, BackgroundTaskKind::Subagent { .. }))
            .expect("a subagent task");
        assert!(subagent.tool_uses > 0, "subagent progress steps counted");
    }

    #[test]
    fn acp_session_updates_fold_into_thread_state() {
        use crate::thread::event::PlanEntryLite;
        let mut t = ChatThread::new();
        t.push_user_message("go");

        // Plan → pinned checklist; survives the turn boundary.
        t.apply(&ThreadEvent::PlanUpdated {
            entries: vec![
                PlanEntryLite { content: "a".into(), status: "in_progress".into(), priority: "high".into() },
                PlanEntryLite { content: "b".into(), status: "pending".into(), priority: "low".into() },
            ],
        });
        // Slash refresh replaces the palette; mode + title update their fields.
        // `plan` carries a description + an argument hint; `review` is blank →
        // only the described/hinted one lands in each lookup map.
        t.apply(&ThreadEvent::SlashCommandsUpdated {
            commands: vec!["plan".into(), "review".into()],
            descriptions: vec!["Draft a plan".into(), String::new()],
            hints: vec!["what to plan".into(), String::new()],
        });
        t.apply(&ThreadEvent::ModeChanged { mode_id: "acceptEdits".into() });
        t.apply(&ThreadEvent::TitleUpdated { title: "Fix auth".into() });

        assert_eq!(t.plan.as_ref().map(|p| p.len()), Some(2));
        assert_eq!(t.slash_commands, vec!["plan".to_string(), "review".to_string()]);
        assert_eq!(t.slash_command_descriptions.get("plan").map(String::as_str), Some("Draft a plan"));
        assert!(!t.slash_command_descriptions.contains_key("review"), "blank description skipped");
        assert_eq!(t.slash_command_hints.get("plan").map(String::as_str), Some("what to plan"));
        assert!(!t.slash_command_hints.contains_key("review"), "blank hint skipped");
        assert_eq!(t.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(t.title.as_deref(), Some("Fix auth"));

        // A new turn keeps the plan/title pinned (they persist across turns) even
        // as the footer/summary reset.
        t.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
        t.push_user_message("next");
        assert!(t.plan.is_some(), "plan survives a turn boundary");
        assert_eq!(t.title.as_deref(), Some("Fix auth"), "title survives a turn boundary");
    }

    #[test]
    fn empty_plan_update_clears_the_card() {
        use crate::thread::event::PlanEntryLite;
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::PlanUpdated {
            entries: vec![PlanEntryLite { content: "a".into(), status: "pending".into(), priority: "low".into() }],
        });
        assert!(t.plan.is_some());
        // ACP full-replaces with an empty list to clear the plan.
        t.apply(&ThreadEvent::PlanUpdated { entries: vec![] });
        assert!(t.plan.is_none(), "an empty plan hides the card");
    }

    #[test]
    fn clear_blanks_conversation_but_keeps_pickers() {
        let mut t = ChatThread::new();
        t.push_user_message("hello");
        t.apply(&ThreadEvent::SessionInit {
            meta: Default::default(),
            session_id: "sid-1".into(),
            model: "claude-opus".into(),
            permission_mode: "default".into(),
            slash_commands: vec![],
        });
        // The picker-facing config that must survive a reset (set after init to
        // reflect the live state the composer reads at `clear` time).
        t.model = Some("claude-opus".into());
        t.permission_mode = Some("acceptEdits".into());
        t.slash_commands = vec!["clear".into(), "compact".into()];
        t.last_error = Some("boom".into());

        t.clear();

        // Conversation/session state is gone.
        assert!(t.entries.is_empty(), "transcript blanked");
        assert!(t.session_id.is_none(), "session id dropped (fresh, non-resumed)");
        assert!(t.last_error.is_none(), "error cleared");
        assert!(!t.turn_active, "no turn in flight");
        assert!(t.usage.is_none() && t.last_summary.is_none());
        // Picker-facing config the composer reads is preserved.
        assert_eq!(t.model.as_deref(), Some("claude-opus"), "model kept");
        assert_eq!(t.permission_mode.as_deref(), Some("acceptEdits"), "mode kept");
        assert_eq!(t.slash_commands, vec!["clear".to_string(), "compact".to_string()]);
    }

    #[test]
    fn tool_result_images_attach_to_the_matching_card() {
        use crate::thread::ChatImage;
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::ToolCallStarted { id: "ti".into(), name: "Read".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "ti".into(), content: "[image]".into(), is_error: false, structured: None });
        t.apply(&ThreadEvent::ToolResultImages {
            tool_use_id: "ti".into(),
            images: vec![ChatImage { media_type: "image/png".into(), data: "QUJD".into() }] });
        match t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "ti" => Some(tc),
            _ => None,
        }) {
            Some(tc) => {
                assert_eq!(tc.images.len(), 1);
                assert_eq!(tc.images[0].data, "QUJD");
            }
            None => panic!("tool card missing"),
        }
        // Images for an unknown id are dropped (no card to attach to).
        t.apply(&ThreadEvent::ToolResultImages {
            tool_use_id: "nope".into(),
            images: vec![ChatImage { media_type: "image/png".into(), data: "X".into() }] });
    }

    #[test]
    fn tool_output_delta_appends_then_result_replaces() {
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::ToolCallStarted { id: "c1".into(), name: "Bash".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolOutputDelta { id: "c1".into(), chunk: "line1\n".into() });
        t.apply(&ThreadEvent::ToolOutputDelta { id: "c1".into(), chunk: "line2\n".into() });
        // Mid-stream the card shows the accumulated chunks.
        let tc = t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "c1" => Some(tc), _ => None }).unwrap();
        assert_eq!(tc.result.as_deref(), Some("line1\nline2\n"));
        // Completion replaces with the authoritative full output.
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "c1".into(), content: "line1\nline2\n".into(), is_error: false, structured: None });
        let tc = t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "c1" => Some(tc), _ => None }).unwrap();
        assert_eq!(tc.result.as_deref(), Some("line1\nline2\n"));
        assert!(matches!(tc.status, ToolCallStatus::Completed));
    }

    #[test]
    fn blank_completion_does_not_erase_streamed_output() {
        // A partial/cancelled command with empty aggregatedOutput must keep the
        // text the user already saw stream in (still marking the terminal status).
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::ToolCallStarted { id: "c2".into(), name: "Bash".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolOutputDelta { id: "c2".into(), chunk: "partial output".into() });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "c2".into(), content: String::new(), is_error: true, structured: None });
        let tc = t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "c2" => Some(tc), _ => None }).unwrap();
        assert_eq!(tc.result.as_deref(), Some("partial output"), "streamed output preserved");
        assert!(matches!(tc.status, ToolCallStatus::Failed(_)));
        // A tool with NO prior stream still records an empty result (unchanged).
        t.apply(&ThreadEvent::ToolCallStarted { id: "c3".into(), name: "Bash".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "c3".into(), content: String::new(), is_error: false, structured: None });
        let tc = t.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == "c3" => Some(tc), _ => None }).unwrap();
        assert_eq!(tc.result.as_deref(), Some(""), "no prior stream → empty result recorded");
    }

    #[test]
    fn compact_boundary_pushes_a_divider() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        t.apply(&ThreadEvent::AssistantText("working".into()));
        t.apply(&ThreadEvent::CompactBoundary { summary: "Context compacted".into() });
        match t.entries.last() {
            Some(ThreadEntry::ContextCompaction { summary }) => assert_eq!(summary, "Context compacted"),
            other => panic!("expected a compaction divider, got {other:?}"),
        }
    }

    #[test]
    fn live_usage_tracks_and_clears_per_turn_and_caches_window() {
        let mut t = ChatThread::new();
        t.push_user_message("go");
        // A live event mid-turn populates live_usage without touching the footer.
        t.apply(&ThreadEvent::LiveUsage(TurnUsage {
            input_tokens: 100, output_tokens: 5, context_window: Some(200_000), ..Default::default()
        }));
        assert!(t.turn_active, "still mid-turn");
        assert_eq!(t.live_usage.as_ref().unwrap().input_tokens, 100);
        assert_eq!(t.last_known_context_window, Some(200_000), "window cached as denominator");
        assert!(t.usage.is_none(), "settled footer untouched by a live event");
        // A new turn clears the live meter but keeps the cached window.
        t.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
        t.push_user_message("next");
        assert!(t.live_usage.is_none(), "live meter cleared at new turn");
        assert_eq!(t.last_known_context_window, Some(200_000), "window survives the turn boundary");
    }

    #[test]
    fn session_cost_accumulates_only_from_settled_turns() {
        let mut t = ChatThread::new();
        t.push_user_message("a");
        t.apply(&ThreadEvent::TurnEnded {
            result: None, is_error: false, turn_diff: None,
            usage: Some(TurnUsage { cost_usd: Some(0.10), ..Default::default() }),
        });
        t.push_user_message("b");
        t.apply(&ThreadEvent::TurnEnded {
            result: None, is_error: false, turn_diff: None,
            usage: Some(TurnUsage { cost_usd: Some(0.25), ..Default::default() }),
        });
        assert!((t.session_cost_usd - 0.35).abs() < 1e-9, "cost sums across turns: {}", t.session_cost_usd);
        // A turn with no cost (Codex reports None) doesn't change the accumulator.
        t.push_user_message("c");
        t.apply(&ThreadEvent::TurnEnded {
            result: None, is_error: false, turn_diff: None,
            usage: Some(TurnUsage { cost_usd: None, ..Default::default() }),
        });
        assert!((t.session_cost_usd - 0.35).abs() < 1e-9, "None cost is a no-op");
    }

    #[test]
    fn diagnostic_folds_into_error_turn_then_clears() {
        let mut t = ChatThread::new();
        // A diagnostic stashed before an error TurnEnded appends to last_error.
        t.apply(&ThreadEvent::Diagnostic("stderr: connection reset".into()));
        t.apply(&ThreadEvent::TurnEnded { result: Some("api error".into()), usage: None, is_error: true, turn_diff: None });
        let err = t.last_error.clone().expect("error recorded");
        assert!(err.contains("api error"));
        assert!(err.contains("stderr: connection reset"), "diagnostic folded in: {err}");

        // A stashed diagnostic must NOT leak into a LATER turn's error text.
        t.apply(&ThreadEvent::Diagnostic("stale noise".into()));
        // This turn succeeds → the stash is consumed (dropped), not shown.
        t.apply(&ThreadEvent::TurnEnded { result: Some("done".into()), usage: None, is_error: false, turn_diff: None });
        t.push_user_message("again");
        t.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: true, turn_diff: None });
        let err2 = t.last_error.clone().expect("second error");
        assert!(!err2.contains("stale noise"), "old diagnostic leaked into a later turn: {err2}");
        assert_eq!(err2, "turn ended with error", "generic fallback, no leaked stash");
    }

    #[test]
    fn notice_folds_to_a_muted_divider() {
        // A quietly-handled event reads as a divider, never as agent speech.
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::Notice("Auto-declined: attestation/generate".into()));
        match t.entries.last() {
            Some(ThreadEntry::ContextCompaction { summary }) => {
                assert_eq!(summary, "Auto-declined: attestation/generate");
            }
            other => panic!("a notice folds to a muted divider, got {other:?}"),
        }

        // A retried request repeats the notice — one divider, not a wall.
        t.apply(&ThreadEvent::Notice("Auto-declined: attestation/generate".into()));
        t.apply(&ThreadEvent::Notice("Auto-declined: attestation/generate".into()));
        assert_eq!(t.entries.len(), 1, "consecutive identical notices collapse");

        // A different notice is still its own divider.
        t.apply(&ThreadEvent::Notice("Auto-declined: permissions/request".into()));
        assert_eq!(t.entries.len(), 2, "a distinct notice still shows");
    }

    #[test]
    fn session_resume_stale_clears_only_the_attempted_id() {
        // The stored id still equals the attempted one → cleared, notice pushed.
        let mut t = ChatThread::new();
        t.session_id = Some("dead-sid".into());
        t.apply(&ThreadEvent::SessionResumeStale { attempted_id: "dead-sid".into() });
        assert!(t.session_id.is_none(), "dead id cleared so the next send omits --resume");
        assert!(matches!(
            t.entries.last(),
            Some(ThreadEntry::ContextCompaction { summary }) if summary.contains("no longer resumable")
        ), "recovery notice pushed");

        // A fresh id minted by an interleaved SessionInit must SURVIVE a stale
        // event that names the OLD id.
        let mut t2 = ChatThread::new();
        t2.session_id = Some("fresh-sid".into());
        t2.apply(&ThreadEvent::SessionResumeStale { attempted_id: "old-sid".into() });
        assert_eq!(t2.session_id.as_deref(), Some("fresh-sid"), "fresh id preserved");
    }
}
