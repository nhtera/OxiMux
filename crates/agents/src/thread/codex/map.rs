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
            vec![ThreadEvent::TurnEnded { result: None, usage, is_error }]
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
        // thread/started, thread/*, turn/diff/updated, mcpServer/*,
        // skills/changed, remoteControl/*, …
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
            vec![ThreadEvent::ToolResult { tool_use_id: id, content, is_error, structured }]
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
            Some((format!("{server}.{tool}"), item.get("arguments").cloned().unwrap_or(Value::Null)))
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
        // Sub-agent items must always classify as SubAgent. The 0.144.1 schema
        // populates `collabAgentToolCall.tool` with a freeform collab-tool name
        // (`subAgentActivity` has no `tool` field), so the old `tool`-as-name
        // path would fall to a generic card. Pass the item TYPE as the name —
        // the tool-detail classifier recognizes it — and carry the full item as
        // input so the card body still has the collab details.
        "collabAgentToolCall" | "subAgentActivity" => Some((ty.to_string(), item.clone())),
        // Known non-tool items — not tool cards.
        "agentMessage" | "reasoning" | "plan" | "userMessage" | "hookPrompt"
        | "contextCompaction" | "enteredReviewMode" | "exitedReviewMode" | "sleep" => None,
        // Unknown type → a generic card keyed by the raw type, never dropped.
        other => Some((other.to_string(), item.clone())),
    }
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
        "mcpToolCall" | "dynamicToolCall" => {
            let err = item.get("error");
            let is_error = failed || err.map(|e| !e.is_null()).unwrap_or(false);
            let content = err
                .filter(|e| !e.is_null())
                .or_else(|| item.get("result"))
                .map(|v| v.to_string())
                .unwrap_or_default();
            (content, is_error, item.get("result").cloned())
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
        match &done[..] {
            [ThreadEvent::ToolResult { tool_use_id, content, is_error, .. }] => {
                assert_eq!(tool_use_id, "it1");
                assert_eq!(content, "total 0");
                assert!(!is_error);
            }
            other => panic!("expected one ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn nonzero_exit_is_error() {
        let done = map_notification(
            "item/completed",
            &json!({"item": {"type": "commandExecution", "id": "x", "status": "completed",
                             "aggregatedOutput": "boom", "exitCode": 2}}),
            &mut st(),
        );
        assert!(matches!(&done[0], ThreadEvent::ToolResult { is_error: true, .. }));
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
        match &done[..] {
            [ThreadEvent::ToolResult { tool_use_id, content, is_error, .. }] => {
                assert_eq!(tool_use_id, "f1");
                assert!(content.contains("+b"));
                assert!(!is_error);
            }
            other => panic!("expected ToolResult with diff, got {other:?}"),
        }
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

    #[test]
    fn sub_agent_items_classify_via_type_name() {
        // A `collabAgentToolCall` with a populated `tool` field must still be a
        // SubAgent card — the mapper passes the item TYPE as the name so the
        // classifier recognizes it (the `tool` value would otherwise be generic).
        let started = map_notification(
            "item/started",
            &json!({"item":{"type":"collabAgentToolCall","id":"sa1","tool":"spawn","status":"inProgress"}}),
            &mut st(),
        );
        match &started[..] {
            [ThreadEvent::ToolCallStarted { name, .. }] => assert_eq!(name, "collabAgentToolCall"),
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }
}
