//! Codex app-server (v2) notification → [`ThreadEvent`] mapping.
//!
//! codex 0.144.1's v2 protocol has a **single** event channel (item lifecycle +
//! turn/thread events) — the legacy `codex/event/<snake>` mirror the older Paseo
//! reference warns about does NOT exist here, so there is no dual-channel dedup
//! to do. `item/started` carries the `ThreadItem` at the start of a tool/message,
//! `item/completed` carries the authoritative final item (so text/reasoning are
//! finalized from it, not from a delta buffer). Verified via
//! `codex app-server generate-json-schema`.

use serde_json::{json, Value};

use super::super::event::{PlanEntryLite, ThreadEvent, TurnUsage};
use super::image_items;
use super::CodexState;

/// Map one server → client notification to zero-or-more `ThreadEvent`s,
/// updating the shared `CodexState` (turn id, last usage). Unknown methods and
/// unknown item types degrade gracefully (a generic card / skip) — never panic.
pub fn map_notification(method: &str, params: &Value, st: &mut CodexState) -> Vec<ThreadEvent> {
    // A notification for a SUB-AGENT's own thread must never end the root turn or
    // splice its deltas into the root transcript. The guard fires ONLY when the
    // root id is KNOWN and the notification names a DIFFERENT thread: an unknown
    // root (handshake not yet recorded, `thread_id == None`) passes through, so
    // the session's own first content is never swallowed and the single-thread
    // reality stays a no-op. A matched or absent `threadId` is likewise not
    // foreign. codex-cli 0.144.1 makes `threadId` a required field on the
    // turn/item completion notifications, so this cross-thread splice is a live
    // corruption once a session engages sub-agents — hence a defensive drop.
    let incoming_thread = params.get("threadId").and_then(|v| v.as_str());
    let foreign = match (st.thread_id.as_deref(), incoming_thread) {
        (Some(root), Some(incoming)) => root != incoming,
        _ => false,
    };
    if foreign {
        // A registered sub-agent child's item lifecycle routes into the spawning
        // collab tool card's log rather than being dropped; an unregistered foreign
        // item is buffered (its collab spawn item may still be racing) and replayed
        // on registration. All OTHER foreign methods keep the tested root-state
        // protection below (they never touch the root turn/usage/error/transcript).
        if let Some(inc) = incoming_thread
            && let Some(routed) = route_foreign_subagent(method, params, st, inc)
        {
            return routed;
        }
        if matches!(
            method,
            "turn/started"
                | "turn/completed"
                | "thread/tokenUsage/updated"
                // A sub-agent's own error carries a required `threadId` too — it
                // must NOT fold into the ROOT thread's `last_error`/`turn_active`.
                | "error"
                | "item/agentMessage/delta"
                | "item/reasoning/textDelta"
                | "item/reasoning/summaryTextDelta"
                | "item/started"
                | "item/completed"
                | "item/commandExecution/outputDelta"
                | "item/commandExecution/terminalInteraction"
        ) {
            return Vec::new();
        }
    }
    // Root-thread item lifecycle: after the card is (re)built by the match below,
    // register any sub-agent children it spawns and replay their buffered actions
    // into the just-built parent card's log.
    let mut out = map_root_notification(method, params, st);
    out.extend(register_subagents_and_replay(method, params, st));
    out
}

/// The root-thread notification map — the original per-method dispatch. Split out
/// so `map_notification` can append sub-agent registration/replay after the card
/// exists (fold drops a `SubagentAction` whose parent card isn't built yet).
fn map_root_notification(method: &str, params: &Value, st: &mut CodexState) -> Vec<ThreadEvent> {
    match method {
        // --- streaming deltas ---------------------------------------------
        "item/agentMessage/delta" => delta(params)
            .map(ThreadEvent::AssistantTextDelta)
            .into_iter()
            .collect(),
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => delta(params)
            .map(ThreadEvent::ThinkingDelta)
            .into_iter()
            .collect(),

        // --- plan + live command output --------------------------------------
        // The agent replaced its plan checklist → the same pinned plan panel
        // Claude/ACP feed. A missing `plan` key is skipped; a present-but-empty
        // list clears the card (matches the ACP full-replacement semantics).
        "turn/plan/updated" => match params.get("plan").and_then(|v| v.as_array()) {
            Some(steps) => vec![ThreadEvent::PlanUpdated { entries: plan_entries(steps) }],
            None => Vec::new(),
        },
        // A command's output streams live before completion; append each chunk to
        // the open card keyed by `itemId`. `item/completed` later replaces the
        // accumulated text with the authoritative `aggregatedOutput`.
        "item/commandExecution/outputDelta" => output_delta(params),
        // The agent typed into an interactive command (a sibling top-level
        // notification method, NOT an item type). Surface the `stdin` as a
        // labeled line on the command's open card so the interaction is visible.
        "item/commandExecution/terminalInteraction" => terminal_interaction(params),

        // --- item lifecycle -----------------------------------------------
        "item/started" => match params.get("item") {
            Some(item) => {
                // Cache tool items so a later approval request (which carries
                // only the itemId) can render the command / changes.
                if let Some(id) = item.get("id").and_then(|v| v.as_str())
                    && matches!(
                        item.get("type").and_then(|v| v.as_str()),
                        Some("commandExecution" | "fileChange")
                    )
                {
                    st.cmd_items.insert(id.to_string(), item.clone());
                }
                item_started(item)
            }
            None => Vec::new(),
        },
        "item/completed" => match params.get("item") {
            Some(item) => {
                // Drop the cached tool item now its lifecycle is over (any
                // approval already resolved), so `cmd_items` can't grow unbounded
                // over a long chat.
                if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                    st.cmd_items.remove(id);
                }
                item_completed(item, &st.cwd)
            }
            None => Vec::new(),
        },

        // --- turn lifecycle -----------------------------------------------
        "turn/started" => {
            if let Some(tid) = params.get("turn").and_then(|t| t.get("id")).and_then(|v| v.as_str()) {
                st.current_turn_id = Some(tid.to_string());
                // Append to the rewind ledger (one turn per user message), so a
                // later conversation-rewind can map a user-message ordinal to the
                // `thread/fork` `lastTurnId`.
                st.user_turn_ids.push(tid.to_string());
            }
            // Start each turn's usage fresh, so a turn that never reports its own
            // usage doesn't inherit the previous turn's counts on TurnEnded.
            st.last_usage = None;
            Vec::new()
        }
        "turn/completed" => {
            let is_error = params
                .get("turn")
                .and_then(|t| t.get("status"))
                .and_then(|s| s.as_str())
                .map(|s| !s.eq_ignore_ascii_case("completed"))
                .unwrap_or(false);
            st.current_turn_id = None;
            st.cancel_requested = false; // turn over — drop any stale Stop request
            // The root turn can't complete while its spawned sub-agents run (the
            // collab `wait` keeps the turn open), so by here every child this turn
            // registered is done — clear the routing map and any un-replayed buffer
            // so neither grows across a long session (mirrors the `cmd_items`
            // insert/remove discipline). The parent tool cards keep their settled
            // logs in the transcript; only the live routing state is dropped.
            st.subagent_parent_by_thread.clear();
            st.subagent_buffer.clear();
            let usage = st.last_usage.take();
            // Take, don't clone: the diff belongs to the turn that just ended, and
            // a turn that changes nothing must not inherit the previous turn's.
            let turn_diff = st.turn_diff.take();
            vec![ThreadEvent::TurnEnded { result: None, usage, is_error, turn_diff }]
        }

        // The turn's accumulated file changes. Each notification restates the
        // WHOLE turn's diff (verified on captured 0.144.3 wire), so the newest
        // simply replaces the last; `turn/completed` attaches it to `TurnEnded`.
        // Stashed rather than emitted — a card per intermediate diff would churn
        // the transcript for what is one summary.
        super::protocol::N_TURN_DIFF_UPDATED => {
            let diff = params.get("diff").and_then(|v| v.as_str()).unwrap_or_default();
            // An empty diff means the turn's net change is nothing (an edit was
            // reverted) — drop the stash rather than showing a card with no files.
            st.turn_diff = (!diff.trim().is_empty()).then(|| diff.to_string());
            Vec::new()
        }

        // --- usage --------------------------------------------------------
        // Stash for the settled `turn/completed` footer AND emit live so the
        // composer meter updates mid-turn. Codex carries the window size live
        // (`modelContextWindow`), so the live meter has a real % on turn 1.
        "thread/tokenUsage/updated" => {
            st.last_usage = parse_usage(params);
            st.last_usage.clone().map(ThreadEvent::LiveUsage).into_iter().collect()
        }

        // A turn that ran while logged out fails with a 401 on the model
        // websocket (not a JSON-RPC error) — classify it as a sign-in prompt
        // rather than a generic error bubble. This is the defensive fallback to
        // the proactive `account/read` check; the card mount is idempotent, so a
        // retry burst (Reconnecting 2/5 …) just re-shows the same card.
        "error" if super::protocol::error_is_unauthorized(params) => {
            vec![ThreadEvent::AuthRequired {
                methods: vec![super::chatgpt_signin_method()],
                error: None,
            }]
        }
        "error" => vec![ThreadEvent::Error(error_message(params))],

        // A browser OAuth sign-in resolved. Clear the stashed login id and tell
        // the view to drop the card (success) or re-show it with the error.
        super::protocol::N_ACCOUNT_LOGIN_COMPLETED => {
            let success = params.get("success").and_then(Value::as_bool).unwrap_or(false);
            let error = params.get("error").and_then(Value::as_str).map(str::to_string);
            vec![ThreadEvent::AuthOutcome { success, error }]
        }

        // The backend compacted earlier context — mirror Claude's divider.
        "thread/compacted" => vec![ThreadEvent::CompactBoundary {
            summary: "Context compacted".to_string(),
        }],

        // Housekeeping / telemetry / not-yet-surfaced — intentionally ignored:
        // thread/started, thread/*, mcpServer/*, skills/changed, remoteControl/*, …
        //
        // `item/fileChange/outputDelta` is deliberately absent from this list:
        // probing app-server 0.144.3 with 300- and 400-line patches, a
        // `fileChange` item goes started → completed emitting no output deltas
        // at all (its `commandExecution` sibling does stream them). There is no
        // such notification to handle, so a live-progress arm for it would be
        // dead code keyed to a guessed field name.
        _ => Vec::new(),
    }
}

/// Cap on buffered pre-registration child actions per thread — a runaway child
/// must not grow the buffer without bound before its collab spawn item lands.
const MAX_SUBAGENT_BUFFER: usize = 100;

/// Route a FOREIGN (`threadId != root`) notification for a sub-agent child.
/// Returns `Some` when the notification is a child item lifecycle event (so the
/// caller returns immediately): registered child `item/completed` → the log line
/// for its parent card; an `item/started` or an unregistered `item/completed` →
/// `Some(empty)` (kept out of the root, buffered when unregistered so it replays
/// once the collab spawn item registers the mapping). Returns `None` for every
/// other foreign method, which then falls to the root-state-protection drop.
fn route_foreign_subagent(
    method: &str,
    params: &Value,
    st: &mut CodexState,
    incoming: &str,
) -> Option<Vec<ThreadEvent>> {
    if !matches!(method, "item/started" | "item/completed") {
        return None;
    }
    let item = params.get("item")?;
    // Log completed actions only (one clean line per child action, no start/finish
    // double-logging and no per-delta churn) — a start is kept out of root but
    // produces no line.
    if method == "item/started" {
        return Some(Vec::new());
    }
    match st.subagent_parent_by_thread.get(incoming).cloned() {
        Some(parent) => Some(subagent_action_line(item, &parent).into_iter().collect()),
        None => {
            // Registration is still racing — buffer bounded, replay on register.
            let buf = st.subagent_buffer.entry(incoming.to_string()).or_default();
            if buf.len() >= MAX_SUBAGENT_BUFFER {
                buf.pop_front();
            }
            buf.push_back(item.clone());
            Some(Vec::new())
        }
    }
}

/// When a root-thread notification carries a collab spawn item, register its
/// child thread(s) → this collab tool card's item id, and replay any child
/// actions that were buffered before the mapping existed. Runs AFTER the card is
/// built by the main map (so replayed `SubagentAction`s fold onto an existing
/// parent card).
fn register_subagents_and_replay(method: &str, params: &Value, st: &mut CodexState) -> Vec<ThreadEvent> {
    if !matches!(method, "item/started" | "item/completed") {
        return Vec::new();
    }
    let Some(item) = params.get("item") else { return Vec::new() };
    let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    if item_id.is_empty() {
        return Vec::new();
    }
    let children: Vec<String> = match item.get("type").and_then(|v| v.as_str()) {
        // A spawn's `receiverThreadIds` are the newly spawned child thread(s).
        Some("collabAgentToolCall") => item
            .get("receiverThreadIds")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
            .unwrap_or_default(),
        Some("subAgentActivity") => item
            .get("agentThreadId")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for child in children {
        st.subagent_parent_by_thread.insert(child.clone(), item_id.to_string());
        if let Some(buffered) = st.subagent_buffer.remove(&child) {
            for it in buffered {
                out.extend(subagent_action_line(&it, item_id));
            }
        }
    }
    out
}

/// Build the `SubagentAction` log line for a completed child item, addressed to
/// the parent card `parent`. `None` for items with no meaningful one-line summary
/// (e.g. reasoning) so they don't clutter the log.
fn subagent_action_line(item: &Value, parent: &str) -> Option<ThreadEvent> {
    let line = codex_subagent_summary(item)?;
    Some(ThreadEvent::SubagentAction {
        parent_tool_call_id: parent.to_string(),
        line,
    })
}

/// A compact one-line label for a completed child item: `{tool} {primary arg}`
/// for tool items, the first line of an `agentMessage`, `None` for reasoning and
/// other non-actionable items. Truncated so a giant command/message can't blow
/// the row.
fn codex_subagent_summary(item: &Value) -> Option<String> {
    match item.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
        "agentMessage" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or_default();
            let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            (!first.is_empty()).then(|| truncate_chars(first, 120))
        }
        // Reasoning is too noisy for the parent card log.
        "reasoning" => None,
        _ => {
            let (name, input) = tool_call(item)?;
            let arg = codex_primary_arg(&input);
            let line = if arg.is_empty() { name } else { format!("{name} {arg}") };
            Some(truncate_chars(&line.replace('\n', " "), 120))
        }
    }
}

/// The most salient single argument of a child tool call's input (the shape
/// `tool_call` builds): a command, query, or url. Empty when none applies.
fn codex_primary_arg(input: &Value) -> String {
    for key in ["command", "query", "url"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    String::new()
}

/// Truncate to at most `max` chars (char-boundary safe), appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Map Codex `turn/plan/updated` steps into the gpui-free `PlanEntryLite` the
/// plan panel renders. Codex reports no per-step priority, so all rows default
/// to `medium` (the renderer's neutral weight).
fn plan_entries(steps: &[Value]) -> Vec<PlanEntryLite> {
    steps
        .iter()
        .filter_map(|s| {
            let content = s.get("step").and_then(|v| v.as_str())?.to_string();
            let status = plan_status_wire(s.get("status").and_then(|v| v.as_str()).unwrap_or(""));
            Some(PlanEntryLite {
                content,
                status: status.to_string(),
                priority: "medium".to_string(),
            })
        })
        .collect()
}

/// Codex `TurnPlanStepStatus` (camelCase) → the `TodoWrite`-compatible wire
/// string the plan panel maps. Unknown/future values degrade to `pending`.
fn plan_status_wire(status: &str) -> &'static str {
    match status {
        "inProgress" => "in_progress",
        "completed" => "completed",
        _ => "pending",
    }
}

/// `item/commandExecution/outputDelta` → a live output-append event keyed by
/// `itemId`. Empty id or empty delta yields nothing.
fn output_delta(params: &Value) -> Vec<ThreadEvent> {
    let id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or_default();
    let chunk = params.get("delta").and_then(|v| v.as_str()).unwrap_or_default();
    if id.is_empty() || chunk.is_empty() {
        return Vec::new();
    }
    vec![ThreadEvent::ToolOutputDelta { id: id.to_string(), chunk: chunk.to_string() }]
}

/// `.delta` string from a `*/delta` notification.
fn delta(params: &Value) -> Option<String> {
    params.get("delta")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// `item/started` → a `ToolCallStarted` for tool-like items; nothing for
/// message/reasoning (their text streams via deltas + finalizes on completion).
fn item_started(item: &Value) -> Vec<ThreadEvent> {
    let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    match tool_call(item) {
        Some((name, input)) => vec![ThreadEvent::ToolCallStarted { id: id.to_string(), name, input }],
        None => Vec::new(),
    }
}

/// `item/completed` → finalized `AssistantText` / `AssistantThinking` for
/// message/reasoning, an inline image for image items, or a `ToolResult` for a
/// tool item. `cwd` contains the image-item file paths.
fn item_completed(item: &Value, cwd: &std::path::Path) -> Vec<ThreadEvent> {
    let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or_default();
    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    match ty {
        "agentMessage" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or_default();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![ThreadEvent::AssistantText(text.to_string())]
            }
        }
        "reasoning" => {
            let text = join_strings(item.get("content")).filter(|s| !s.is_empty())
                .or_else(|| join_strings(item.get("summary")).filter(|s| !s.is_empty()));
            text.map(ThreadEvent::AssistantThinking).into_iter().collect()
        }
        // Image items carry a file path; render an inline thumbnail via the same
        // ToolResultImages pipeline Claude uses. The card shell came from
        // `item/started` (these classify as tools now); fill it with a caption
        // plus, when the path is readable + contained, the pixels.
        "imageView" | "imageGeneration" => {
            let failed = matches!(
                item.get("status").and_then(|v| v.as_str()).unwrap_or_default(),
                "failed" | "declined"
            );
            let mut out = vec![ThreadEvent::ToolResult {
                tool_use_id: id.clone(),
                content: image_caption(item),
                is_error: failed,
                structured: None,
            }];
            if let Some(images) = image_items::image_result(item, cwd) {
                out.push(ThreadEvent::ToolResultImages { tool_use_id: id, images });
            }
            out
        }
        _ if tool_call(item).is_some() => {
            let (content, is_error, structured) = tool_result(ty, item);
            let mut out = Vec::new();
            // A `subAgentActivity` is delivered ONLY as `item/completed` — codex
            // never announces its start (verified on captured 0.144.3 wire). A
            // result for an id the fold has never seen is dropped, so its card
            // never existed, and the child-activity log addressed to that card id
            // silently went nowhere. Open the card here so completion has
            // something to settle.
            //
            // Deliberately NOT done for every tool item: an item's completed
            // payload does not have to repeat its start fields (a completed
            // `commandExecution` carries no `command`), so re-asserting a card
            // from it would overwrite good input with empty values. This type
            // carries its whole payload on completion, so it is safe here.
            if ty == "subAgentActivity" {
                out.extend(item_started(item));
            }
            out.push(ThreadEvent::ToolResult { tool_use_id: id, content, is_error, structured });
            out
        }
        // userMessage/plan/hookPrompt/contextCompaction/review-modes/sleep: not
        // surfaced in the transcript here (sub-agent + image items route through
        // the arms above).
        _ => Vec::new(),
    }
}

/// A human caption for an image item's tool-card body: the revised prompt or
/// result string for a generation, else a plain marker. The pixels ride in a
/// follow-up `ToolResultImages`.
fn image_caption(item: &Value) -> String {
    item.get("revisedPrompt")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("result").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "[image]".to_string())
}

/// `item/commandExecution/terminalInteraction` → a labeled append on the
/// command's open card showing the `stdin` the agent typed. Empty id/stdin → no
/// event. Keyed by `itemId` (the command-execution item).
fn terminal_interaction(params: &Value) -> Vec<ThreadEvent> {
    let id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or_default();
    let stdin = params.get("stdin").and_then(|v| v.as_str()).unwrap_or_default();
    if id.is_empty() || stdin.is_empty() {
        return Vec::new();
    }
    vec![ThreadEvent::ToolOutputDelta {
        id: id.to_string(),
        chunk: format!("[terminal input] {stdin}\n"),
    }]
}

/// Classify a `ThreadItem` as a tool call → `(display name, input Value)`, or
/// `None` for non-tool items. An unknown type that isn't a known non-tool falls
/// through to a generic card so nothing is silently dropped.
fn tool_call(item: &Value) -> Option<(String, Value)> {
    let ty = item.get("type").and_then(|v| v.as_str())?;
    match ty {
        "commandExecution" => {
            let command = item.get("command").and_then(|v| v.as_str()).unwrap_or_default();
            let cwd = item.get("cwd").and_then(|v| v.as_str()).unwrap_or_default();
            // `command` is the login-shell wrapper Codex runs everything through
            // (`/bin/zsh -lc '<script>'`); `commandActions` reports the script
            // itself. Carry both: the wrapper stays authoritative for copy/raw
            // views, while the renderer can show the script the agent meant.
            let actions = item.get("commandActions").cloned().unwrap_or(Value::Null);
            Some((
                "Bash".to_string(),
                json!({ "command": command, "cwd": cwd, "commandActions": actions }),
            ))
        }
        "fileChange" => Some(("apply_patch".to_string(), json!({ "changes": item.get("changes").cloned().unwrap_or(Value::Null) }))),
        "mcpToolCall" => {
            let server = item.get("server").and_then(|v| v.as_str()).unwrap_or_default();
            let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
            Some((mcp_tool_name(server, tool), item.get("arguments").cloned().unwrap_or(Value::Null)))
        }
        "dynamicToolCall" => {
            let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("tool");
            Some((tool.to_string(), item.get("arguments").cloned().unwrap_or(Value::Null)))
        }
        "webSearch" => {
            let query = item.get("query").and_then(|v| v.as_str()).unwrap_or_default();
            Some(("web_search".to_string(), json!({ "query": query })))
        }
        // Image items get a tool-card shell (from `item/started`) so the
        // `item/completed` follow-up can fill it with an inline thumbnail.
        "imageView" => Some(("view_image".to_string(), item.clone())),
        "imageGeneration" => Some(("generate_image".to_string(), item.clone())),
        // Sub-agent items. `collabAgentToolCall.tool` is a closed enum naming the
        // collab ACTION (spawnAgent/sendInput/…), not the delegated tool, so it
        // can't serve as the card name; `subAgentActivity` has no `tool` at all.
        // Emit a display name here — the rest of the Codex normalization (shell
        // unwrap, apply_patch) names at the mapper too — and carry the full item
        // as input so the card body keeps the collab details. Both names classify
        // as SubAgent; the raw types stay in the classifier for transcripts
        // persisted before this rename.
        "collabAgentToolCall" => Some((COLLAB_TOOL_NAME.to_string(), item.clone())),
        "subAgentActivity" => Some((SUBAGENT_ACTIVITY_TOOL_NAME.to_string(), item.clone())),
        // Known non-tool items — not tool cards.
        "agentMessage" | "reasoning" | "plan" | "userMessage" | "hookPrompt"
        | "contextCompaction" | "enteredReviewMode" | "exitedReviewMode" | "sleep" => None,
        // Unknown type → a generic card keyed by the raw type, never dropped.
        other => Some((other.to_string(), item.clone())),
    }
}

/// The canonical `mcp__<server>__<tool>` name for a Codex MCP call, matching how
/// Claude journals the same call. The mapper KNOWS this item is an MCP call, so
/// naming it here lights up every existing `mcp__` path for free — the classifier
/// (`ToolDetail::Mcp` → the `render_mcp` body) and the `<server> · <tool>` header
/// humanizer — with no name heuristics anywhere else and no false positives on a
/// genuinely dotted tool name.
///
/// The humanizer splits at the FIRST `__` after the prefix, so a server name
/// containing `__` would be split in the wrong place; that case keeps the old
/// `<server>.<tool>` name and its generic card. A `__` in the TOOL is harmless —
/// it lands after the split — and result unwrapping is keyed on the item type,
/// not the name, so even a fallback-named call still gets a legible body.
fn mcp_tool_name(server: &str, tool: &str) -> String {
    if server.is_empty() || tool.is_empty() || server.contains("__") {
        return format!("{server}.{tool}");
    }
    format!("mcp__{server}__{tool}")
}

/// Flatten an `McpToolCallResult` envelope (`{content[], structuredContent?,
/// _meta?}`) into the text a card body should show, so a card reads as the tool's
/// output instead of the transport envelope.
///
/// Returns `(text, is_error)`. Precedence: the `content[]` text parts joined by
/// newlines; else `structuredContent` pretty-printed; else the compact envelope
/// JSON as a last resort, so an unrecognized shape still shows SOMETHING rather
/// than an empty card. Non-text content parts (images, resource links) name their
/// type — the body says "[image]" rather than dumping base64.
///
/// `isError` is not part of the codex envelope (a failed call is reported by the
/// item's own `error`/`status`), but the underlying MCP `CallToolResult` defines
/// it, so it is honoured if a server passes one through.
fn unwrap_mcp_result(result: &Value) -> (String, bool) {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let parts: Vec<String> = result
        .get("content")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(mcp_content_part).collect())
        .unwrap_or_default();
    if !parts.is_empty() {
        return (parts.join("\n"), is_error);
    }
    match result.get("structuredContent") {
        Some(sc) if !sc.is_null() => {
            (serde_json::to_string_pretty(sc).unwrap_or_else(|_| sc.to_string()), is_error)
        }
        _ => (result.to_string(), is_error),
    }
}

/// One MCP content block → its display text. Text blocks yield their text; other
/// block types yield a bracketed marker naming the kind. `None` for a block with
/// nothing to show, so it doesn't contribute a blank line.
fn mcp_content_part(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") | None => {
            let t = block.get("text").and_then(Value::as_str)?;
            (!t.is_empty()).then(|| t.to_string())
        }
        Some(other) => Some(format!("[{other}]")),
    }
}

/// Display name for a Codex `collabAgentToolCall` card. Its own `tool` field
/// names the collab action and rides the card's target line instead.
const COLLAB_TOOL_NAME: &str = "Agent";
/// Display name for a Codex `subAgentActivity` card.
const SUBAGENT_ACTIVITY_TOOL_NAME: &str = "Agent activity";

/// One entry of a collab call's `agentsStates` map: the child thread id and the
/// backend's last-known `CollabAgentState` for it.
struct CollabAgent {
    id: String,
    status: String,
    message: Option<String>,
}

/// Read `collabAgentToolCall.agentsStates` — a map of child thread id →
/// `CollabAgentState { status, message? }`. Entries are sorted by id so a card
/// body doesn't reshuffle between repaints (serde_json maps preserve insertion
/// order, but the backend's is not promised to be stable). A non-object or
/// absent map yields no agents rather than an error: the field is required by
/// the schema, but a thin/older payload must still render a card.
fn collab_agents(item: &Value) -> Vec<CollabAgent> {
    let Some(map) = item.get("agentsStates").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut agents: Vec<CollabAgent> = map
        .iter()
        .map(|(id, state)| CollabAgent {
            id: id.clone(),
            // A state that isn't the documented object shape still names the
            // agent — its status just reads as unknown.
            status: state.get("status").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            message: state
                .get("message")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string),
        })
        .collect();
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    agents
}

/// Roll the per-agent states of a collab call up into the one status the card
/// shows, by severity: any errored/notFound agent makes the group errored, any
/// still-starting or running agent makes it running, an interrupted agent
/// outranks a clean finish. `shutdown` counts as done (the agent was closed on
/// purpose — `closeAgent` is a normal collab action).
///
/// A status outside the documented `CollabAgentStatus` enum is returned verbatim
/// rather than being folded into "completed": a future backend state must read
/// as itself on the card, not as a wrong success.
fn rollup_agent_status(agents: &[CollabAgent]) -> Option<String> {
    if agents.is_empty() {
        return None;
    }
    let any = |s: &str| agents.iter().any(|a| a.status == s);
    if any("errored") || any("notFound") {
        return Some("errored".to_string());
    }
    if any("running") || any("pendingInit") {
        return Some("running".to_string());
    }
    if any("interrupted") {
        return Some("interrupted".to_string());
    }
    if let Some(unknown) = agents.iter().find(|a| !matches!(a.status.as_str(), "completed" | "shutdown")) {
        return Some(unknown.status.clone());
    }
    Some("completed".to_string())
}

/// Build the `ToolResult` payload for a completed tool item.
fn tool_result(ty: &str, item: &Value) -> (String, bool, Option<Value>) {
    let status = item.get("status").and_then(|v| v.as_str()).unwrap_or_default();
    let failed = matches!(status, "failed" | "declined");
    match ty {
        "commandExecution" => {
            let out = item.get("aggregatedOutput").and_then(|v| v.as_str()).unwrap_or_default();
            let exit = item.get("exitCode").and_then(|v| v.as_i64());
            let is_error = failed || exit.map(|c| c != 0).unwrap_or(false);
            (out.to_string(), is_error, Some(json!({ "exitCode": exit, "status": status })))
        }
        "fileChange" => {
            let diffs = item
                .get("changes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.get("diff").and_then(|d| d.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            (diffs, failed, item.get("changes").cloned())
        }
        // An MCP call's `result` is a `McpToolCallResult` envelope — unwrap it so
        // the body reads as the tool's output. An `error` (`{message}`) wins over
        // the result and reads as its message; the full envelope stays on
        // `structured` for the raw/sheet views.
        "mcpToolCall" => {
            let err = item.get("error").filter(|e| !e.is_null());
            if let Some(e) = err {
                let msg = e
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string());
                return (msg, true, item.get("result").cloned());
            }
            match item.get("result").filter(|r| !r.is_null()) {
                Some(result) => {
                    let (content, envelope_error) = unwrap_mcp_result(result);
                    (content, failed || envelope_error, Some(result.clone()))
                }
                None => (String::new(), failed, None),
            }
        }
        // A dynamic tool's `result` has no envelope contract — render it verbatim.
        "dynamicToolCall" => {
            let err = item.get("error");
            let is_error = failed || err.map(|e| !e.is_null()).unwrap_or(false);
            let content = err
                .filter(|e| !e.is_null())
                .or_else(|| item.get("result"))
                .map(|v| v.to_string())
                .unwrap_or_default();
            (content, is_error, item.get("result").cloned())
        }
        // A completed collab call: its own `status` decides whether the CALL
        // failed, while `agentsStates` describes the agents it targeted. Both are
        // surfaced — the rolled-up agent status is what the body leads with, and
        // the per-agent rows carry any message the backend attached.
        "collabAgentToolCall" => {
            let agents = collab_agents(item);
            let rolled = rollup_agent_status(&agents);
            let n = agents.len();
            let mut bits = Vec::new();
            if n > 0 {
                bits.push(format!("{n} agent{}", if n == 1 { "" } else { "s" }));
            }
            if let Some(r) = rolled.clone() {
                bits.push(r);
            } else if !status.is_empty() {
                // No agent states on the wire — the call's own status is all the
                // body can honestly report, which still beats an empty card.
                bits.push(status.to_string());
            }
            let structured = json!({
                "type": ty,
                "tool": item.get("tool").cloned().unwrap_or(Value::Null),
                "status": rolled.unwrap_or_else(|| status.to_string()),
                "callStatus": status,
                "agents": agents
                    .iter()
                    .map(|a| json!({ "id": a.id, "status": a.status, "message": a.message }))
                    .collect::<Vec<_>>(),
            });
            (bits.join(" · "), failed, Some(structured))
        }
        // `subAgentActivity` reports one child agent's lifecycle beat. It carries
        // no `status` field at all (so it never reads as failed) — `kind` and the
        // agent's identity are the whole payload.
        "subAgentActivity" => {
            let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or_default();
            let path = item.get("agentPath").and_then(|v| v.as_str()).unwrap_or_default();
            let body = [kind, path].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            let structured = json!({
                "type": ty,
                "status": kind,
                "agentPath": path,
                "agentThreadId": item.get("agentThreadId").cloned().unwrap_or(Value::Null),
            });
            (body, false, Some(structured))
        }
        // A completed web search carries the query + the action taken (search /
        // openPage / find). It has no separate results field on the wire, so the
        // body renders what was searched — no longer an empty card.
        "webSearch" => {
            let query = item.get("query").and_then(|v| v.as_str()).unwrap_or_default();
            let action = web_search_action(item.get("action"));
            let body = [query, action.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (body, failed, item.get("action").cloned())
        }
        _ => (String::new(), failed, None),
    }
}

/// Human summary of a `WebSearchAction` (`search`/`openPage`/`find`), used for a
/// web-search result body. Empty when the action is absent or shapeless.
fn web_search_action(action: Option<&Value>) -> String {
    let Some(a) = action else { return String::new() };
    match a.get("type").and_then(|v| v.as_str()) {
        Some("search") => {
            let q = a
                .get("query")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| join_strings(a.get("queries")))
                .unwrap_or_default();
            if q.is_empty() { "Searched the web".to_string() } else { format!("Searched: {q}") }
        }
        Some("openPage") => match a.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => format!("Opened: {u}"),
            _ => "Opened a page".to_string(),
        },
        Some("find") => match a.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => format!("Found on page: {p}"),
            _ => "Searched within the page".to_string(),
        },
        _ => String::new(),
    }
}

/// Parse `thread/tokenUsage/updated` → `TurnUsage` from the `.tokenUsage.last`
/// breakdown (the most recent turn's counts).
fn parse_usage(params: &Value) -> Option<TurnUsage> {
    let usage = params.get("tokenUsage")?;
    let last = usage.get("last")?;
    let get = |k: &str| last.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Some(TurnUsage {
        input_tokens: get("inputTokens"),
        output_tokens: get("outputTokens"),
        cache_read_tokens: get("cachedInputTokens"),
        cache_creation_tokens: 0, // Codex reports no separate cache-creation count.
        context_window: usage.get("modelContextWindow").and_then(|v| v.as_u64()),
        cost_usd: None, // Codex doesn't report per-turn cost here.
    })
}

fn error_message(params: &Value) -> String {
    params
        .get("message")
        .and_then(|m| m.as_str())
        .map(String::from)
        .unwrap_or_else(|| params.to_string())
}

fn join_strings(v: Option<&Value>) -> Option<String> {
    let arr = v?.as_array()?;
    let joined = arr
        .iter()
        .filter_map(|x| x.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::state::ChatThread;
    use crate::thread::{ThreadEntry, ToolCallStatus, ToolDetail};

    /// Replay a captured `codex app-server` session, mapping every line exactly
    /// as the live worker does, and fold the result into the REAL `ChatThread` —
    /// so the assertions are about the transcript a user actually sees rather
    /// than a re-implementation of the fold rules.
    ///
    /// The fixtures are real server→client bytes from codex-cli **0.144.3**,
    /// captured over the same handshake `protocol.rs` drives (initialize →
    /// initialized → thread/start → turn/start), then trimmed of the operator's
    /// personal channels (hooks, account/rate limits, MCP server list) and
    /// redacted of home paths. JSON-RPC responses are excluded: the transport
    /// correlates those and the mapper never sees them.
    ///
    /// To regenerate after a codex upgrade: drive that handshake against
    /// `codex app-server`, tee every server→client line, and re-run the same
    /// prompts — "Use the node_repl MCP tool to evaluate 6*7" and "Spawn a
    /// collaborator subagent to verify <file>, then wait for it".
    fn replay(fixture: &str) -> (Vec<ThreadEvent>, CodexState) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/thread/testdata/");
        let raw = std::fs::read_to_string(format!("{dir}{fixture}")).expect("fixture");
        let mut st = CodexState::default();
        let mut out = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).expect("captured line must parse");
            let method = v.get("method").and_then(Value::as_str).expect("a notification");
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            // The worker records the root thread id from the thread/start
            // response; the fixture holds only notifications, so seed it from
            // `thread/started` the same way — this is what makes a sub-agent's
            // thread read as foreign.
            if method == "thread/started"
                && let Some(id) = params.pointer("/thread/id").and_then(Value::as_str)
            {
                st.thread_id = Some(id.to_string());
            }
            out.extend(map_notification(method, &params, &mut st));
        }
        (out, st)
    }

    /// Fold replayed events into the transcript the user sees.
    fn render(events: &[ThreadEvent]) -> ChatThread {
        let mut t = ChatThread::default();
        for e in events {
            t.apply(e);
        }
        t
    }

    /// Every tool card in the folded transcript, in order.
    fn tool_cards(t: &ChatThread) -> Vec<&crate::thread::tool_call::ToolCall> {
        t.entries
            .iter()
            .filter_map(|e| match e {
                ThreadEntry::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replays_a_captured_mcp_turn_into_a_humanized_unwrapped_card() {
        let (events, _) = replay("codex-mcp-call-turn.jsonl");
        let thread = render(&events);
        let cards = tool_cards(&thread);
        let mcp = cards
            .iter()
            .find(|c| c.name.starts_with("mcp__"))
            .expect("the captured turn made an MCP call");

        // Named like Claude journals the same call, so it classifies + humanizes.
        assert_eq!(mcp.name, "mcp__node_repl__js");
        assert_eq!(ToolDetail::classify(&mcp.name, None, &mcp.input), ToolDetail::Mcp);
        // The regression this fixture locks: the body was the raw envelope
        // (`{"content":[{"type":"text","text":""}],...}`), not the tool's answer.
        assert_eq!(mcp.result.as_deref(), Some("42"), "body reads the tool's text");
        assert!(matches!(mcp.status, ToolCallStatus::Completed));
        // The envelope survives for the raw/sheet view.
        assert!(mcp.structured.as_ref().expect("envelope kept")["content"].is_array());
    }

    #[test]
    fn replays_a_captured_collab_turn_into_a_humanized_card_with_child_activity() {
        let (events, _) = replay("codex-collab-turn.jsonl");
        let thread = render(&events);
        let cards = tool_cards(&thread);

        // The real spawn surfaces as a `subAgentActivity` naming the child, and a
        // separate `wait` collab call — NOT as a `spawnAgent` collab call. Both
        // read as humanized SubAgent cards rather than raw camelCase types.
        for c in &cards {
            assert_eq!(
                ToolDetail::classify(&c.name, None, &c.input),
                ToolDetail::SubAgent,
                "every card in this turn is a sub-agent card, got {}",
                c.name
            );
        }
        let activity = cards.iter().find(|c| c.name == "Agent activity").expect("an activity card");
        assert_eq!(activity.structured.as_ref().unwrap()["agentPath"], "/root/verify_scratch_file");

        let wait = cards.iter().find(|c| c.name == "Agent").expect("a collab card");
        assert_eq!(wait.input["tool"], "wait");
        // This capture's `agentsStates` is EMPTY — a real spawn+wait reports no
        // per-agent states at all, so the body must degrade to the call's own
        // status instead of rendering blank (the live-verified round-9 bug).
        assert_eq!(wait.input["agentsStates"], json!({}));
        assert_eq!(wait.result.as_deref(), Some("completed"), "thin payload still gets a body");

        // The child thread's work is attached to its parent card rather than
        // leaking into the root transcript.
        let log: Vec<&String> = cards.iter().flat_map(|c| c.subagent_log.iter()).collect();
        assert!(!log.is_empty(), "the child's actions attach to a parent card");
        assert!(
            log.iter().any(|l| l.contains("scratch-verify.txt")),
            "the child's command shows in the parent's activity log, got {log:?}"
        );
        // …and the root transcript never grows a card for the child's own tools.
        assert!(
            !cards.iter().any(|c| c.name == "Bash"),
            "the sub-agent's command must not surface as a root card"
        );
    }

    fn st() -> CodexState {
        CodexState::default()
    }

    #[test]
    fn agent_message_delta_streams_text() {
        let evs = map_notification("item/agentMessage/delta", &json!({"delta": "hel"}), &mut st());
        assert_eq!(evs, vec![ThreadEvent::AssistantTextDelta("hel".into())]);
    }

    #[test]
    fn reasoning_deltas_stream_thinking() {
        let a = map_notification("item/reasoning/textDelta", &json!({"delta": "think"}), &mut st());
        let b = map_notification("item/reasoning/summaryTextDelta", &json!({"delta": "sum"}), &mut st());
        assert_eq!(a, vec![ThreadEvent::ThinkingDelta("think".into())]);
        assert_eq!(b, vec![ThreadEvent::ThinkingDelta("sum".into())]);
    }

    #[test]
    fn command_execution_starts_and_completes_one_card() {
        let mut s = st();
        let started = map_notification(
            "item/started",
            &json!({"item": {"type": "commandExecution", "id": "it1", "command": "ls -la", "cwd": "/tmp"}}),
            &mut s,
        );
        match &started[..] {
            [ThreadEvent::ToolCallStarted { id, name, input }] => {
                assert_eq!(id, "it1");
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "ls -la");
            }
            other => panic!("expected one ToolCallStarted, got {other:?}"),
        }
        let done = map_notification(
            "item/completed",
            &json!({"item": {"type": "commandExecution", "id": "it1", "status": "completed",
                             "aggregatedOutput": "total 0", "exitCode": 0}}),
            &mut s,
        );
        let (content, is_error, _) = result_of(&done);
        assert_eq!(content, "total 0");
        assert!(!is_error);
        // One started + one completed settle ONE card, and completion must not
        // clobber the input the start established — a completed command item
        // does not repeat its `command`.
        let thread = render(&[started, done].concat());
        let cards = tool_cards(&thread);
        assert_eq!(cards.len(), 1, "one card for one command, got {cards:?}");
        assert_eq!(cards[0].id, "it1");
        assert_eq!(cards[0].input["command"], "ls -la", "the started input survives completion");
        assert!(matches!(cards[0].status, ToolCallStatus::Completed));
    }

    #[test]
    fn nonzero_exit_is_error() {
        let done = map_notification(
            "item/completed",
            &json!({"item": {"type": "commandExecution", "id": "x", "status": "completed",
                             "aggregatedOutput": "boom", "exitCode": 2}}),
            &mut st(),
        );
        assert!(result_of(&done).1, "a nonzero exit fails the card");
    }

    #[test]
    fn command_execution_carries_the_wrapper_and_the_script() {
        // Codex runs every exec through the login shell and reports the script
        // it wrapped in `commandActions`. Both reach the card: the wrapper stays
        // authoritative for copy/raw views, the script is what gets displayed.
        let started = map_notification(
            "item/started",
            &json!({"item": {"type": "commandExecution", "id": "c1",
                             "command": "/bin/zsh -lc 'echo hello'",
                             "cwd": "/tmp/x",
                             "commandActions": [{"type": "unknown", "command": "echo hello"}]}}),
            &mut st(),
        );
        match &started[..] {
            [ThreadEvent::ToolCallStarted { name, input, .. }] => {
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "/bin/zsh -lc 'echo hello'");
                assert_eq!(input["commandActions"][0]["command"], "echo hello");
            }
            other => panic!("expected a Bash ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn file_change_completed_surfaces_diff() {
        let done = map_notification(
            "item/completed",
            &json!({"item": {"type": "fileChange", "id": "f1", "status": "completed",
                             "changes": [{"path": "a.txt", "kind": "update", "diff": "@@ -1 +1 @@\n-a\n+b"}]}}),
            &mut st(),
        );
        let (content, is_error, _) = result_of(&done);
        assert!(content.contains("+b"));
        assert!(!is_error);
    }

    #[test]
    fn agent_message_and_reasoning_finalize() {
        let m = map_notification(
            "item/completed",
            &json!({"item": {"type": "agentMessage", "id": "m", "text": "done"}}),
            &mut st(),
        );
        assert_eq!(m, vec![ThreadEvent::AssistantText("done".into())]);
        let r = map_notification(
            "item/completed",
            &json!({"item": {"type": "reasoning", "id": "r", "content": ["a", "b"], "summary": []}}),
            &mut st(),
        );
        assert_eq!(r, vec![ThreadEvent::AssistantThinking("a\nb".into())]);
    }

    #[test]
    fn turn_started_appends_to_rewind_ledger() {
        // One turn id recorded per user prompt, in order — so a later
        // conversation-rewind can map a user-message ordinal to `thread/fork`'s
        // `lastTurnId`.
        let mut s = st();
        map_notification("turn/started", &json!({"turn": {"id": "turn_a"}}), &mut s);
        map_notification("turn/completed", &json!({"turn": {"status": "completed"}}), &mut s);
        map_notification("turn/started", &json!({"turn": {"id": "turn_b"}}), &mut s);
        assert_eq!(s.user_turn_ids, vec!["turn_a".to_string(), "turn_b".to_string()]);
        assert_eq!(s.current_turn_id.as_deref(), Some("turn_b"));
    }

    #[test]
    fn turn_completed_carries_usage_and_status() {
        let mut s = st();
        s.current_turn_id = Some("t".into());
        map_notification(
            "thread/tokenUsage/updated",
            &json!({"tokenUsage": {"last": {"inputTokens": 10, "outputTokens": 5, "cachedInputTokens": 3},
                                    "modelContextWindow": 200000}}),
            &mut s,
        );
        let evs = map_notification("turn/completed", &json!({"turn": {"status": "completed"}}), &mut s);
        match &evs[..] {
            [ThreadEvent::TurnEnded { usage: Some(u), is_error, .. }] => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
                assert_eq!(u.cache_read_tokens, 3);
                assert_eq!(u.context_window, Some(200000));
                assert!(!is_error);
            }
            other => panic!("expected TurnEnded with usage, got {other:?}"),
        }
        assert!(s.current_turn_id.is_none());
    }

    #[test]
    fn failed_turn_is_error() {
        let evs = map_notification("turn/completed", &json!({"turn": {"status": "failed"}}), &mut st());
        assert!(matches!(&evs[0], ThreadEvent::TurnEnded { is_error: true, .. }));
    }

    #[test]
    fn token_usage_emits_live_and_stashes_for_footer() {
        let mut s = st();
        // Emits LiveUsage (with the live window size) AND stashes for the footer.
        let evs = map_notification(
            "thread/tokenUsage/updated",
            &json!({"tokenUsage": {"last": {"inputTokens": 40, "outputTokens": 8, "cachedInputTokens": 2},
                                    "modelContextWindow": 272000}}),
            &mut s,
        );
        match &evs[..] {
            [ThreadEvent::LiveUsage(u)] => {
                assert_eq!(u.input_tokens, 40);
                assert_eq!(u.context_window, Some(272000), "Codex carries the window live");
            }
            other => panic!("expected LiveUsage, got {other:?}"),
        }
        // The stash is still present for turn/completed to fold into the footer.
        let ended = map_notification("turn/completed", &json!({"turn": {"status": "completed"}}), &mut s);
        assert!(matches!(&ended[..], [ThreadEvent::TurnEnded { usage: Some(_), .. }]), "footer still fed");
    }

    #[test]
    fn unknown_item_type_is_a_generic_card_not_dropped() {
        let started = map_notification(
            "item/started",
            &json!({"item": {"type": "someFutureTool", "id": "u1"}}),
            &mut st(),
        );
        match &started[..] {
            [ThreadEvent::ToolCallStarted { name, .. }] => assert_eq!(name, "someFutureTool"),
            other => panic!("expected a generic ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_is_ignored() {
        assert!(map_notification("thread/settings/updated", &json!({}), &mut st()).is_empty());
    }

    #[test]
    fn plan_updated_maps_steps_and_status() {
        // Codex `inProgress` (camelCase) maps to the panel's `in_progress`; each
        // row defaults to `medium` priority (Codex reports none).
        let evs = map_notification(
            "turn/plan/updated",
            &json!({"threadId":"t","turnId":"u","plan":[
                {"step":"scaffold","status":"completed"},
                {"step":"wire api","status":"inProgress"},
                {"step":"tests","status":"pending"}]}),
            &mut st(),
        );
        match &evs[..] {
            [ThreadEvent::PlanUpdated { entries }] => {
                assert_eq!(entries.len(), 3);
                assert_eq!(entries[0].status, "completed");
                assert_eq!(entries[1].status, "in_progress");
                assert_eq!(entries[1].content, "wire api");
                assert_eq!(entries[2].status, "pending");
                assert!(entries.iter().all(|e| e.priority == "medium"));
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
        // An empty plan clears the card; a missing plan key emits nothing.
        assert_eq!(
            map_notification("turn/plan/updated", &json!({"plan":[]}), &mut st()),
            vec![ThreadEvent::PlanUpdated { entries: vec![] }],
        );
        assert!(map_notification("turn/plan/updated", &json!({}), &mut st()).is_empty());
    }

    #[test]
    fn command_output_delta_appends_live() {
        let evs = map_notification(
            "item/commandExecution/outputDelta",
            &json!({"itemId":"it1","threadId":"t","turnId":"u","delta":"line1\n"}),
            &mut st(),
        );
        assert_eq!(evs, vec![ThreadEvent::ToolOutputDelta { id: "it1".into(), chunk: "line1\n".into() }]);
        // Empty delta or missing id → nothing.
        assert!(map_notification("item/commandExecution/outputDelta",
            &json!({"itemId":"it1","delta":""}), &mut st()).is_empty());
        assert!(map_notification("item/commandExecution/outputDelta",
            &json!({"delta":"x"}), &mut st()).is_empty());
    }

    #[test]
    fn thread_compacted_emits_divider() {
        let evs = map_notification("thread/compacted", &json!({"threadId":"t"}), &mut st());
        assert!(matches!(&evs[..], [ThreadEvent::CompactBoundary { .. }]));
    }

    #[test]
    fn foreign_thread_notification_never_touches_root_state() {
        // Root id known; a notification naming a DIFFERENT thread is a sub-agent's
        // own event — it must neither end the root turn nor splice its deltas.
        let mut s = st();
        s.thread_id = Some("root".into());
        s.current_turn_id = Some("turn-1".into());
        let ended = map_notification(
            "turn/completed",
            &json!({"threadId":"child","turn":{"status":"completed"}}),
            &mut s,
        );
        assert!(ended.is_empty(), "foreign turn/completed dropped");
        assert_eq!(s.current_turn_id.as_deref(), Some("turn-1"), "root turn still active");
        let delta = map_notification(
            "item/agentMessage/delta",
            &json!({"threadId":"child","delta":"sub text"}),
            &mut s,
        );
        assert!(delta.is_empty(), "foreign delta not spliced into root transcript");
        // A sub-agent's OWN error must not surface as the root thread's error
        // (it carries a required threadId, so it's caught by the same guard).
        let err = map_notification(
            "error",
            &json!({"threadId":"child","turnId":"t","willRetry":false,
                "error":{"message":"child boom"}}),
            &mut s,
        );
        assert!(err.is_empty(), "foreign error not folded into root last_error");
        // A ROOT-thread error still surfaces normally.
        let root_err = map_notification(
            "error",
            &json!({"threadId":"root","turnId":"t","willRetry":false,
                "error":{"message":"root boom"}}),
            &mut s,
        );
        assert!(matches!(&root_err[..], [ThreadEvent::Error(_)]), "root error still surfaces");
    }

    #[test]
    fn collab_spawn_registers_child_and_routes_its_actions_to_parent_card() {
        // The root issues a `spawnAgent` collab tool call; its `receiverThreadIds`
        // name the child. The item itself becomes the parent card, and the child's
        // later completed actions route into that card's log — never the root turn.
        let mut s = st();
        s.thread_id = Some("root".into());
        let spawn = map_notification(
            "item/started",
            &json!({"threadId":"root","item":{"type":"collabAgentToolCall","id":"collab_1",
                "tool":"spawnAgent","senderThreadId":"root","receiverThreadIds":["child-A"],
                "status":"inProgress","agentsStates":{}}}),
            &mut s,
        );
        // The collab item created the parent tool card.
        assert!(matches!(&spawn[..], [ThreadEvent::ToolCallStarted { id, .. }] if id == "collab_1"));
        assert_eq!(s.subagent_parent_by_thread.get("child-A").map(String::as_str), Some("collab_1"));

        // A foreign child command completion → a routed log line for `collab_1`.
        let child_cmd = map_notification(
            "item/completed",
            &json!({"threadId":"child-A","item":{"type":"commandExecution","id":"c1",
                "status":"completed","command":"cargo test","cwd":"/tmp","aggregatedOutput":"ok"}}),
            &mut s,
        );
        assert_eq!(child_cmd, vec![ThreadEvent::SubagentAction {
            parent_tool_call_id: "collab_1".into(), line: "Bash cargo test".into() }]);

        // A foreign child agent message → its first line as an action.
        let child_msg = map_notification(
            "item/completed",
            &json!({"threadId":"child-A","item":{"type":"agentMessage","id":"m1",
                "text":"Found the bug.\nmore detail"}}),
            &mut s,
        );
        assert_eq!(child_msg, vec![ThreadEvent::SubagentAction {
            parent_tool_call_id: "collab_1".into(), line: "Found the bug.".into() }]);

        // A foreign child `item/started` produces no line (completed-only logging).
        let child_start = map_notification(
            "item/started",
            &json!({"threadId":"child-A","item":{"type":"commandExecution","id":"c2","command":"ls"}}),
            &mut s,
        );
        assert!(child_start.is_empty());
    }

    #[test]
    fn child_actions_before_registration_are_buffered_and_replayed() {
        // The child's completion races ahead of the collab spawn item. The early
        // action is buffered, then replayed into the parent card log on register.
        let mut s = st();
        s.thread_id = Some("root".into());
        let early = map_notification(
            "item/completed",
            &json!({"threadId":"child-B","item":{"type":"commandExecution","id":"e1",
                "status":"completed","command":"rustc --version","cwd":"/tmp"}}),
            &mut s,
        );
        assert!(early.is_empty(), "unregistered child action buffered, not emitted");
        assert_eq!(s.subagent_buffer.get("child-B").map(|b| b.len()), Some(1));

        // The collab spawn item lands → replay the buffered action after the card.
        let spawn = map_notification(
            "item/started",
            &json!({"threadId":"root","item":{"type":"collabAgentToolCall","id":"collab_2",
                "tool":"spawnAgent","senderThreadId":"root","receiverThreadIds":["child-B"],
                "status":"inProgress","agentsStates":{}}}),
            &mut s,
        );
        // Card first, then the replayed buffered action addressed to it.
        assert!(matches!(&spawn[0], ThreadEvent::ToolCallStarted { id, .. } if id == "collab_2"));
        assert!(spawn.contains(&ThreadEvent::SubagentAction {
            parent_tool_call_id: "collab_2".into(), line: "Bash rustc --version".into() }));
        assert!(s.subagent_buffer.get("child-B").is_none(), "buffer drained on register");
    }

    #[test]
    fn root_turn_completion_clears_subagent_routing_state() {
        // Registering + buffering state must not survive the root turn that owns it
        // (the turn can't complete while its sub-agents run), so it stays bounded.
        let mut s = st();
        s.thread_id = Some("root".into());
        s.subagent_parent_by_thread.insert("child-A".into(), "collab_1".into());
        s.subagent_buffer.entry("child-B".into()).or_default().push_back(json!({}));
        let ended = map_notification(
            "turn/completed",
            &json!({"threadId":"root","turn":{"status":"completed"}}),
            &mut s,
        );
        assert!(matches!(&ended[..], [ThreadEvent::TurnEnded { .. }]));
        assert!(s.subagent_parent_by_thread.is_empty(), "routing map cleared at turn end");
        assert!(s.subagent_buffer.is_empty(), "buffer cleared at turn end");
    }

    #[test]
    fn unauthorized_error_maps_to_signin_card() {
        let mut s = st();
        s.thread_id = Some("root".into());
        // A logged-out 401 on the root thread → an AuthRequired sign-in card, not
        // a generic error bubble.
        let evs = map_notification("error", &json!({
            "threadId":"root",
            "error":{"message":"Reconnecting... 2/5",
                "codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":401}}},
            "willRetry":true}), &mut s);
        match &evs[..] {
            [ThreadEvent::AuthRequired { methods, error }] => {
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].id, "chatgpt");
                assert!(matches!(methods[0].kind, crate::thread::AuthMethodKind::BrowserOauth));
                assert!(error.is_none());
            }
            other => panic!("expected AuthRequired, got {other:?}"),
        }
        // A non-401 error still surfaces as a generic Error.
        let boom = map_notification("error", &json!({
            "threadId":"root","error":{"message":"boom"}}), &mut s);
        assert!(matches!(&boom[..], [ThreadEvent::Error(_)]));
    }

    #[test]
    fn login_completed_maps_to_auth_outcome() {
        let mut s = st();
        let ok = map_notification("account/login/completed",
            &json!({"loginId":"L1","success":true}), &mut s);
        assert_eq!(ok, vec![ThreadEvent::AuthOutcome { success: true, error: None }]);
        let fail = map_notification("account/login/completed",
            &json!({"loginId":"L1","success":false,"error":"Login was not completed"}), &mut s);
        assert_eq!(fail, vec![ThreadEvent::AuthOutcome {
            success: false, error: Some("Login was not completed".into()) }]);
    }

    #[test]
    fn subagent_activity_item_registers_agent_thread() {
        // `subAgentActivity` names its child via `agentThreadId`.
        let mut s = st();
        s.thread_id = Some("root".into());
        map_notification(
            "item/started",
            &json!({"threadId":"root","item":{"type":"subAgentActivity","id":"act_1",
                "agentThreadId":"child-C","agentPath":"/x","kind":"started"}}),
            &mut s,
        );
        assert_eq!(s.subagent_parent_by_thread.get("child-C").map(String::as_str), Some("act_1"));
    }

    #[test]
    fn unregistered_foreign_non_item_still_dropped() {
        // The root-state protection is unchanged for non-item foreign methods even
        // with sub-agent routing in front of it.
        let mut s = st();
        s.thread_id = Some("root".into());
        s.current_turn_id = Some("turn-1".into());
        let ended = map_notification(
            "turn/completed",
            &json!({"threadId":"child-Z","turn":{"status":"completed"}}),
            &mut s,
        );
        assert!(ended.is_empty(), "foreign turn/completed still dropped");
        assert_eq!(s.current_turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn matching_or_absent_thread_id_passes_through() {
        let mut s = st();
        s.thread_id = Some("root".into());
        // Matching threadId → normal behavior.
        assert_eq!(
            map_notification("item/agentMessage/delta", &json!({"threadId":"root","delta":"hi"}), &mut s),
            vec![ThreadEvent::AssistantTextDelta("hi".into())],
        );
        // Absent threadId → normal behavior (never treated as foreign).
        assert_eq!(
            map_notification("item/agentMessage/delta", &json!({"delta":"ho"}), &mut s),
            vec![ThreadEvent::AssistantTextDelta("ho".into())],
        );
    }

    #[test]
    fn unknown_root_passes_everything_through() {
        // thread_id == None (handshake not yet recorded): a threadId-bearing
        // notification must STILL surface (fail-open) — otherwise the session's
        // own first content is swallowed and the existing fixture tests break.
        let mut s = st(); // thread_id: None
        assert_eq!(
            map_notification(
                "item/agentMessage/delta",
                &json!({"threadId":"anything","delta":"first content"}),
                &mut s,
            ),
            vec![ThreadEvent::AssistantTextDelta("first content".into())],
        );
    }

    #[test]
    fn web_search_result_has_non_empty_body() {
        // Previously fell to the catch-all empty body; now renders query + action.
        let done = map_notification(
            "item/completed",
            &json!({"item":{"type":"webSearch","id":"ws1","status":"completed",
                "query":"rust async","action":{"type":"search","query":"rust async streams"}}}),
            &mut st(),
        );
        match &done[..] {
            [ThreadEvent::ToolResult { tool_use_id, content, is_error, .. }] => {
                assert_eq!(tool_use_id, "ws1");
                assert!(!is_error);
                assert!(content.contains("rust async"), "query in body");
                assert!(content.contains("Searched:"), "action summary in body: {content}");
            }
            other => panic!("expected ToolResult with a body, got {other:?}"),
        }
    }

    #[test]
    fn image_item_emits_tool_result_and_inline_images() {
        // A contained image path renders both a ToolResult card body AND a
        // follow-up ToolResultImages with the pixels.
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("out.png");
        std::fs::write(&img, b"\x89PNG\r\n\x1a\npixels").unwrap();
        let mut s = st();
        s.cwd = dir.path().to_path_buf();
        // item/started gives the card its shell (image items classify as tools).
        let started = map_notification(
            "item/started",
            &json!({"item":{"type":"imageGeneration","id":"im1"}}),
            &mut s,
        );
        assert!(matches!(&started[..], [ThreadEvent::ToolCallStarted { .. }]), "image card shell created");
        let done = map_notification(
            "item/completed",
            &json!({"item":{"type":"imageGeneration","id":"im1","status":"completed",
                "revisedPrompt":"a red square","savedPath": img.to_str().unwrap()}}),
            &mut s,
        );
        match &done[..] {
            [ThreadEvent::ToolResult { tool_use_id, content, .. },
             ThreadEvent::ToolResultImages { tool_use_id: img_id, images }] => {
                assert_eq!(tool_use_id, "im1");
                assert_eq!(content, "a red square");
                assert_eq!(img_id, "im1");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].media_type, "image/png");
            }
            other => panic!("expected ToolResult + ToolResultImages, got {other:?}"),
        }
    }

    #[test]
    fn image_item_outside_cwd_emits_result_without_pixels() {
        // A path escaping cwd is dropped: the card still appears (a caption), but
        // no ToolResultImages leaks arbitrary on-disk bytes.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.png");
        std::fs::write(&secret, b"\x89PNGsecret").unwrap();
        let mut s = st();
        s.cwd = dir.path().to_path_buf();
        let done = map_notification(
            "item/completed",
            &json!({"item":{"type":"imageView","id":"iv1","status":"completed",
                "path": secret.to_str().unwrap()}}),
            &mut s,
        );
        assert!(matches!(&done[..], [ThreadEvent::ToolResult { .. }]), "no images event: {done:?}");
    }

    #[test]
    fn terminal_interaction_appends_stdin_to_card() {
        let evs = map_notification(
            "item/commandExecution/terminalInteraction",
            &json!({"itemId":"c1","threadId":"t","turnId":"u","processId":"p","stdin":"yes\n"}),
            &mut st(),
        );
        match &evs[..] {
            [ThreadEvent::ToolOutputDelta { id, chunk }] => {
                assert_eq!(id, "c1");
                assert!(chunk.contains("yes"), "stdin surfaced: {chunk}");
            }
            other => panic!("expected ToolOutputDelta, got {other:?}"),
        }
        // Empty stdin/id → nothing.
        assert!(map_notification("item/commandExecution/terminalInteraction",
            &json!({"itemId":"c1","stdin":""}), &mut st()).is_empty());
    }

    /// Complete an `mcpToolCall` carrying `result`/`error`, and read back its
    /// `(content, is_error, structured)`.
    fn mcp_completed(payload: Value) -> (String, bool, Value) {
        let mut item = json!({
            "type": "mcpToolCall", "id": "m1", "server": "node_repl", "tool": "js",
            "arguments": {"code": "6*7"}, "status": "completed",
        });
        for (k, v) in payload.as_object().unwrap() {
            item[k] = v.clone();
        }
        result_of(&map_notification("item/completed", &json!({ "item": item }), &mut st()))
    }

    #[test]
    fn mcp_call_is_named_like_claude_and_classifies_as_mcp() {
        let started = map_notification(
            "item/started",
            &json!({"item":{"type":"mcpToolCall","id":"m1","server":"node_repl","tool":"js",
                    "arguments":{"code":"6*7"},"status":"inProgress"}}),
            &mut st(),
        );
        match &started[..] {
            [ThreadEvent::ToolCallStarted { name, input, .. }] => {
                assert_eq!(name, "mcp__node_repl__js", "canonical mcp__server__tool name");
                assert_eq!(ToolDetail::classify(name, None, input), ToolDetail::Mcp);
                assert_eq!(input["code"], "6*7", "arguments stay the card input");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn mcp_tool_name_falls_back_when_the_server_would_break_the_split() {
        // The header humanizer splits at the first `__`, so a server containing
        // `__` keeps the old name rather than rendering a mis-split header.
        assert_eq!(mcp_tool_name("my__srv", "x"), "my__srv.x");
        // A `__` in the TOOL lands after the split — still canonical.
        assert_eq!(mcp_tool_name("srv", "my__tool"), "mcp__srv__my__tool");
        // Absent server/tool must not produce a `mcp____x` name.
        assert_eq!(mcp_tool_name("", "x"), ".x");
        assert_eq!(mcp_tool_name("srv", ""), "srv.");
    }

    #[test]
    fn mcp_result_envelope_unwraps_to_its_text() {
        // The live-verified regression: the envelope itself was the body.
        let (content, is_error, structured) = mcp_completed(json!({
            "result": {"content": [{"type": "text", "text": "42"}],
                       "structuredContent": null, "_meta": null}
        }));
        assert_eq!(content, "42", "the tool's text, not the envelope JSON");
        assert!(!is_error);
        assert!(structured["content"].is_array(), "the full envelope stays for the sheet");
    }

    #[test]
    fn mcp_result_covers_the_envelope_variants() {
        // Several text parts join by newline.
        let (content, ..) = mcp_completed(json!({
            "result": {"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}
        }));
        assert_eq!(content, "a\nb");
        // Non-text parts name their kind instead of dumping bytes.
        let (content, ..) = mcp_completed(json!({
            "result": {"content": [{"type": "image", "data": "iVBORw0KGgo="}]}
        }));
        assert_eq!(content, "[image]");
        // Empty text + structuredContent → the structured payload, pretty-printed.
        let (content, ..) = mcp_completed(json!({
            "result": {"content": [{"type": "text", "text": ""}],
                       "structuredContent": {"answer": 42}}
        }));
        assert!(content.contains("\"answer\": 42"), "structuredContent rendered, got {content:?}");
        // Neither → the compact envelope, so the card is never blank.
        let (content, ..) = mcp_completed(json!({"result": {"content": []}}));
        assert_eq!(content, r#"{"content":[]}"#);
        // A non-object result must not panic.
        let (content, ..) = mcp_completed(json!({"result": "plain"}));
        assert_eq!(content, "\"plain\"");
        // No result at all → an empty body, not a crash.
        let (content, is_error, _) = mcp_completed(json!({"result": Value::Null}));
        assert_eq!(content, "");
        assert!(!is_error);
    }

    #[test]
    fn mcp_errors_mark_the_card_failed() {
        // An item-level error reads as its message.
        let (content, is_error, _) =
            mcp_completed(json!({"status": "failed", "error": {"message": "server crashed"}}));
        assert_eq!(content, "server crashed");
        assert!(is_error);
        // A failed status alone still fails the card.
        let (_, is_error, _) = mcp_completed(json!({
            "status": "failed", "result": {"content": [{"type": "text", "text": "nope"}]}
        }));
        assert!(is_error);
        // An MCP-level `isError` envelope fails the card too.
        let (content, is_error, _) = mcp_completed(json!({
            "result": {"content": [{"type": "text", "text": "bad input"}], "isError": true}
        }));
        assert_eq!(content, "bad input");
        assert!(is_error, "an isError envelope must fail the card");
    }

    #[test]
    fn sub_agent_items_get_humanized_names_that_still_classify() {
        // The mapper names collab items for the header; both names must classify
        // as SubAgent, and the raw item (incl. its `type`) must stay on the input
        // so the card can render the collab detail.
        let started = map_notification(
            "item/started",
            &json!({"item":{"type":"collabAgentToolCall","id":"sa1","tool":"spawnAgent","status":"inProgress"}}),
            &mut st(),
        );
        match &started[..] {
            [ThreadEvent::ToolCallStarted { name, input, .. }] => {
                assert_eq!(name, "Agent");
                assert_eq!(input["type"], "collabAgentToolCall");
                assert_eq!(ToolDetail::classify(name, None, input), ToolDetail::SubAgent);
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
        let activity = map_notification(
            "item/started",
            &json!({"item":{"type":"subAgentActivity","id":"act1","agentPath":"reviewer","agentThreadId":"t9","kind":"started"}}),
            &mut st(),
        );
        match &activity[..] {
            [ThreadEvent::ToolCallStarted { name, input, .. }] => {
                assert_eq!(name, "Agent activity");
                assert_eq!(ToolDetail::classify(name, None, input), ToolDetail::SubAgent);
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    /// Build a completed `collabAgentToolCall` with the given `agentsStates` map.
    fn collab_completed(states: Value, call_status: &str) -> Vec<ThreadEvent> {
        map_notification(
            "item/completed",
            &json!({"item":{
                "type":"collabAgentToolCall","id":"c1","tool":"spawnAgent",
                "senderThreadId":"root","receiverThreadIds":["t1"],
                "status":call_status,"agentsStates":states,
            }}),
            &mut st(),
        )
    }

    /// The `(content, is_error, structured)` of the ToolResult in `evs`.
    fn result_of(evs: &[ThreadEvent]) -> (String, bool, Value) {
        match evs.iter().find(|e| matches!(e, ThreadEvent::ToolResult { .. })) {
            Some(ThreadEvent::ToolResult { content, is_error, structured, .. }) => {
                (content.clone(), *is_error, structured.clone().unwrap_or(Value::Null))
            }
            _ => panic!("expected a ToolResult in {evs:?}"),
        }
    }

    #[test]
    fn collab_completion_body_reports_agents_and_rolled_up_status() {
        let (content, is_error, structured) = result_of(&collab_completed(
            json!({"t1": {"status": "completed"}, "t2": {"status": "completed"}}),
            "completed",
        ));
        assert_eq!(content, "2 agents · completed");
        assert!(!is_error);
        assert_eq!(structured["status"], "completed");
        assert_eq!(structured["tool"], "spawnAgent");
        assert_eq!(structured["agents"].as_array().unwrap().len(), 2);
        assert_eq!(structured["agents"][0]["id"], "t1");
    }

    #[test]
    fn collab_status_rolls_up_by_severity() {
        // Any errored agent makes the group errored, even alongside a runner.
        let (content, _, s) = result_of(&collab_completed(
            json!({"a": {"status": "completed"}, "b": {"status": "errored"}, "c": {"status": "running"}}),
            "completed",
        ));
        assert_eq!(s["status"], "errored");
        assert_eq!(content, "3 agents · errored");
        // …then running outranks a clean finish,
        let (_, _, s) = result_of(&collab_completed(
            json!({"a": {"status": "completed"}, "b": {"status": "running"}}),
            "completed",
        ));
        assert_eq!(s["status"], "running");
        // …a pending agent counts as running,
        let (_, _, s) =
            result_of(&collab_completed(json!({"a": {"status": "pendingInit"}}), "inProgress"));
        assert_eq!(s["status"], "running");
        // …interrupted outranks completed,
        let (_, _, s) = result_of(&collab_completed(
            json!({"a": {"status": "interrupted"}, "b": {"status": "completed"}}),
            "completed",
        ));
        assert_eq!(s["status"], "interrupted");
        // …a deliberately closed agent is done, not a failure,
        let (_, _, s) = result_of(&collab_completed(
            json!({"a": {"status": "shutdown"}, "b": {"status": "completed"}}),
            "completed",
        ));
        assert_eq!(s["status"], "completed");
        // …and a missing agent is a failure.
        let (_, _, s) = result_of(&collab_completed(json!({"a": {"status": "notFound"}}), "completed"));
        assert_eq!(s["status"], "errored");
    }

    #[test]
    fn collab_result_is_lenient_about_thin_or_unknown_payloads() {
        // No agentsStates at all → the call's own status carries the body.
        let (content, _, s) = result_of(&collab_completed(json!({}), "completed"));
        assert_eq!(content, "completed");
        assert_eq!(s["agents"].as_array().unwrap().len(), 0);
        // An unknown future state renders verbatim rather than as a wrong success.
        let (_, _, s) = result_of(&collab_completed(json!({"a": {"status": "teleporting"}}), "completed"));
        assert_eq!(s["status"], "teleporting");
        // A failed CALL marks the card failed regardless of agent states.
        let (_, is_error, _) =
            result_of(&collab_completed(json!({"a": {"status": "completed"}}), "failed"));
        assert!(is_error, "a failed collab call must mark its card failed");
        // The backend's per-agent message rides along for the card row.
        let (_, _, s) = result_of(&collab_completed(
            json!({"a": {"status": "errored", "message": "boom"}}),
            "completed",
        ));
        assert_eq!(s["agents"][0]["message"], "boom");
    }

    #[test]
    fn sub_agent_activity_completion_reports_kind_and_path() {
        // `subAgentActivity` has no `status` field on the wire — it must never
        // read as a failure just because status is absent.
        let evs = map_notification(
            "item/completed",
            &json!({"item":{"type":"subAgentActivity","id":"a1","agentPath":"reviewer",
                    "agentThreadId":"t7","kind":"started"}}),
            &mut st(),
        );
        let (content, is_error, s) = result_of(&evs);
        assert_eq!(content, "started · reviewer");
        assert!(!is_error);
        assert_eq!(s["status"], "started");
        assert_eq!(s["agentPath"], "reviewer");
    }
}
