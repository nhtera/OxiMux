//! Decoder: Claude Code `stream-json` wire events → `ThreadEvent`.
//!
//! Pure and sync: one raw JSON line in, zero-or-more `ThreadEvent`s out. A
//! single `assistant` line can carry several content blocks (text + tool_use),
//! hence `Vec`. Unknown `type`s, unknown subtypes, and unknown fields are
//! ignored — the wire schema drifts across `claude` versions and the parser
//! must never panic on it (see spike-findings.md for the captured vocabulary).

use serde_json::Value;

use super::background_task::BackgroundTaskKind;
use super::event::{McpServerStatus, SessionMeta, ThreadEvent, TurnUsage};
use super::question::parse_questions;
use super::tool_call::{
    PermissionKind, PermissionSuggestion, extract_tool_result_images, flatten_tool_result_content,
};

/// Decode one newline-delimited stream-json line. Returns the events it
/// yields (possibly empty for noise: `system/hook_*`, `system/status`,
/// `rate_limit_event`, message framing, or malformed input).
pub fn decode_line(line: &str) -> Vec<ThreadEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    decode_value(&v)
}

fn decode_value(v: &Value) -> Vec<ThreadEvent> {
    // A subagent's internal turn streams inline, every event tagged with the
    // spawning tool's id in `parent_tool_use_id`. Those events must NOT fold into
    // the main transcript as the parent agent's own — but a completed child action
    // (a tool call, the child's visible text or thinking, or a failed result) is
    // routed into the spawning tool card's log via `SubagentAction`, so the user
    // sees what the subagent is doing inline. Deltas and stream framing stay
    // dropped (the log holds completed items only, no per-delta churn). The
    // `system/task_*` events still drive the separate Background Tasks panel.
    if let Some(parent) = v.get("parent_tool_use_id").and_then(Value::as_str) {
        return decode_subagent(v, parent);
    }
    match v.get("type").and_then(Value::as_str) {
        Some("system") => decode_system(v),
        Some("stream_event") => decode_stream_event(v),
        Some("assistant") => decode_assistant(v),
        Some("user") => decode_user(v),
        Some("control_request") => decode_control_request(v),
        Some("result") => decode_result(v),
        _ => Vec::new(),
    }
}

/// Classify a `parent_tool_use_id`-tagged (subagent) line into zero-or-more
/// `SubagentAction` log lines for the spawning tool card.
///
/// An `assistant` line's completed blocks route here — a `tool_use` block
/// becomes a `{name} {primary arg}` line, a non-empty visible `text` block a
/// first-line summary, and a `thinking` block a truncated `[thinking] …` line. A
/// `user` line routes only its *failed* `tool_result` blocks; the child's prompt
/// (a `user`/`text` block) and successful results stay dropped — see
/// [`subagent_failure_line`]. Deltas and stream framing stay dropped too: the log
/// holds completed, user-meaningful items only, no per-delta churn. Parent id =
/// the tag.
fn decode_subagent(v: &Value, parent: &str) -> Vec<ThreadEvent> {
    let kind = v.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("assistant" | "user")) {
        return Vec::new();
    }
    let from_agent = kind == Some("assistant");
    let Some(blocks) = v["message"]["content"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for b in blocks {
        let bt = b.get("type").and_then(Value::as_str);
        let line = match bt {
            Some("tool_use" | "server_tool_use" | "mcp_tool_use") if from_agent => {
                subagent_tool_line(str_field(b, "name"), b.get("input"))
            }
            Some("text") if from_agent => b
                .get("text")
                .and_then(Value::as_str)
                .map(subagent_text_summary)
                .filter(|s| !s.is_empty()),
            Some("thinking") if from_agent => b
                .get("thinking")
                .and_then(Value::as_str)
                .map(subagent_text_summary)
                .filter(|s| !s.is_empty())
                .map(|s| format!("[thinking] {s}")),
            _ if is_tool_result_block(bt) && !from_agent => subagent_failure_line(b),
            _ => None,
        };
        if let Some(line) = line {
            out.push(ThreadEvent::SubagentAction {
                parent_tool_call_id: parent.to_string(),
                line,
            });
        }
    }
    out
}

/// A *failed* child tool result → a `✗ {message}` log line; `None` for anything
/// that succeeded.
///
/// Only failures are logged, for two reasons. The child's `tool_use` block
/// already logged the action, so a matching "it worked" line would double the
/// log's length to say nothing the reader didn't assume — whereas a failure is
/// the surprising event, and it is what explains a subagent that went quiet or
/// changed course. And the wire leaves no choice about the wording: a
/// `tool_result` block carries `tool_use_id` but never the tool's *name* (the
/// name appears only on the earlier `tool_use` block), so a per-line decoder
/// cannot title this "✗ Read" without correlating ids across lines. The error
/// text stands on its own instead — "File does not exist. …" needs no title.
fn subagent_failure_line(b: &Value) -> Option<String> {
    if b.get("is_error").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let text = flatten_tool_result_content(b.get("content"));
    let summary = subagent_text_summary(&text);
    (!summary.is_empty()).then(|| format!("✗ {summary}"))
}

/// A one-line subagent action label from a child tool call: `{name} {primary
/// arg}`, where the primary arg is the tool's most salient input (a path, a
/// command, a query). Truncated so a giant Bash command can't blow the row.
fn subagent_tool_line(name: String, input: Option<&Value>) -> Option<String> {
    let arg = input.and_then(subagent_primary_arg).unwrap_or_default();
    let line = if arg.is_empty() { name } else { format!("{name} {arg}") };
    let line = line.replace('\n', " ");
    Some(truncate_chars(&line, 120))
}

/// The most salient single argument of a tool input, for the subagent log —
/// the first of a small ordered set of common keys (path, command, pattern,
/// query, url, description). `None` when none is a non-empty string.
fn subagent_primary_arg(input: &Value) -> Option<String> {
    for key in ["file_path", "path", "command", "pattern", "query", "url", "description", "prompt"] {
        if let Some(s) = input.get(key).and_then(Value::as_str)
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

/// First non-empty line of a child assistant text block, truncated — a compact
/// "the subagent said…" log row (never the full multi-paragraph body).
fn subagent_text_summary(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    truncate_chars(first, 120)
}

/// Truncate a string to at most `max` chars (char-boundary safe), appending `…`
/// when it was cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn decode_system(v: &Value) -> Vec<ThreadEvent> {
    match v.get("subtype").and_then(Value::as_str) {
        Some("init") => vec![ThreadEvent::SessionInit {
            session_id: str_field(v, "session_id"),
            model: str_field(v, "model"),
            permission_mode: str_field(v, "permissionMode"),
            slash_commands: str_list_field(v, "slash_commands"),
            meta: SessionMeta {
                cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
                tools: str_list_field(v, "tools"),
                mcp_servers: mcp_servers(v),
                agents: str_list_field(v, "agents"),
            },
        }],
        Some("post_turn_summary") => vec![ThreadEvent::TurnSummary {
            detail: str_field(v, "status_detail"),
            category: str_field(v, "status_category"),
        }],
        // Background-task lifecycle (subagents + background bash) → the Background
        // Tasks panel. The task's own internal stream is dropped in `decode_value`
        // via `parent_tool_use_id`; these summaries carry the panel's data.
        Some("task_started") => vec![ThreadEvent::BackgroundTaskStarted {
            task_id: str_field(v, "task_id"),
            tool_use_id: str_field(v, "tool_use_id"),
            kind: BackgroundTaskKind::from_wire(
                v.get("task_type").and_then(Value::as_str).unwrap_or_default(),
                v.get("subagent_type").and_then(Value::as_str),
            ),
            description: str_field(v, "description"),
        }],
        Some("task_progress") => vec![ThreadEvent::BackgroundTaskProgress {
            task_id: str_field(v, "task_id"),
            last_tool: v.get("last_tool_name").and_then(Value::as_str).map(str::to_string),
        }],
        Some("task_updated") => decode_task_updated(v),
        Some("task_notification") => decode_task_notification(v),
        // The CLI summarized earlier history to reclaim context — surface a
        // divider so the gap is visible. `compact_metadata.trigger` ("auto" /
        // "manual") labels why; absent → a plain "Context compacted".
        Some("compact_boundary") => {
            let trigger = v
                .get("compact_metadata")
                .and_then(|m| m.get("trigger"))
                .and_then(Value::as_str);
            let summary = match trigger {
                Some("manual") => "Context compacted (manual)".to_string(),
                Some("auto") => "Context compacted (auto)".to_string(),
                _ => "Context compacted".to_string(),
            };
            vec![ThreadEvent::CompactBoundary { summary }]
        }
        // The CLI signals a long context-compaction with `status: "compacting"`
        // (the end lands separately as `compact_boundary`). Promote just that one
        // status so a "Compacting context…" spinner shows instead of an apparent
        // hang; every other `system/status` (spinner text) stays dropped.
        Some("status")
            if v.get("status").and_then(Value::as_str) == Some("compacting") =>
        {
            vec![ThreadEvent::CompactionStarted]
        }
        // hook_started / hook_response / thinking_tokens / other status → noise.
        // TODO(chat-ui polish): the rest of `system/status` (spinner text) and
        // init's tools/mcp_servers/agents/cwd are dropped for now; surface them
        // when the spinner + session-detail UI is built.
        _ => Vec::new(),
    }
}

/// A `system/task_updated` carries a `patch` — the panel only cares about the
/// terminal transition, which brings the completion `end_time`.
fn decode_task_updated(v: &Value) -> Vec<ThreadEvent> {
    let patch = v.get("patch");
    let status = patch.and_then(|p| p.get("status")).and_then(Value::as_str);
    match status {
        Some(s) if is_terminal_status(s) => vec![ThreadEvent::BackgroundTaskFinished {
            task_id: str_field(v, "task_id"),
            failed: s != "completed",
            ended_at_ms: patch.and_then(|p| p.get("end_time")).and_then(Value::as_u64),
            summary: None,
            output_file: None,
        }],
        _ => Vec::new(),
    }
}

/// A `system/task_notification` at terminal status carries the completion summary
/// + the task's output file (for "View transcript").
fn decode_task_notification(v: &Value) -> Vec<ThreadEvent> {
    match v.get("status").and_then(Value::as_str) {
        Some(s) if is_terminal_status(s) => vec![ThreadEvent::BackgroundTaskFinished {
            task_id: str_field(v, "task_id"),
            failed: s != "completed",
            ended_at_ms: None,
            summary: v.get("summary").and_then(Value::as_str).map(str::to_string),
            output_file: v.get("output_file").and_then(Value::as_str).map(str::to_string),
        }],
        _ => Vec::new(),
    }
}

/// Whether a task status string is terminal (completion or a failure/cancel).
fn is_terminal_status(s: &str) -> bool {
    matches!(s, "completed" | "failed" | "cancelled" | "canceled") || s.starts_with("error")
}

fn decode_stream_event(v: &Value) -> Vec<ThreadEvent> {
    let ev = &v["event"];
    match ev.get("type").and_then(Value::as_str) {
        Some("content_block_start") => decode_content_block_start(ev),
        Some("content_block_delta") => decode_content_block_delta(ev),
        // Live usage: `message_start.message.usage` seeds the turn's input/cache
        // counts; `message_delta.usage` carries the running totals (output grows).
        // Neither carries a context-window size (verified live) — the state-side
        // denominator cache fills that gap.
        Some("message_start") => live_usage(ev.get("message").and_then(|m| m.get("usage"))),
        Some("message_delta") => live_usage(ev.get("usage")),
        _ => Vec::new(),
    }
}

/// A `content_block_start` for a `tool_use` block → open the tool card early
/// (id + name are known here; input is still `{}`), so the args can stream in
/// live via `input_json_delta` before the finalized `tool_use` block. Any other
/// block type (text/thinking) starts nothing — those stream via their deltas.
fn decode_content_block_start(ev: &Value) -> Vec<ThreadEvent> {
    let cb = &ev["content_block"];
    if cb.get("type").and_then(Value::as_str) != Some("tool_use") {
        return Vec::new();
    }
    vec![ThreadEvent::ToolCallStarted {
        id: str_field(cb, "id"),
        name: str_field(cb, "name"),
        // `content_block_start` carries `input: {}`; the real args stream as
        // `input_json_delta` fragments and finalize in the `assistant` block.
        input: cb.get("input").cloned().unwrap_or(Value::Null),
    }]
}

/// A `content_block_delta` → the streamed text/thinking/tool-input delta it
/// carries.
fn decode_content_block_delta(ev: &Value) -> Vec<ThreadEvent> {
    let delta = &ev["delta"];
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => match delta.get("text").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => vec![ThreadEvent::AssistantTextDelta(t.to_string())],
            _ => Vec::new(),
        },
        Some("thinking_delta") => match delta.get("thinking").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => vec![ThreadEvent::ThinkingDelta(t.to_string())],
            _ => Vec::new(),
        },
        // A tool's arguments streaming in as partial JSON. The block index maps to
        // the open tool card via its id — but the delta itself carries no id, so
        // the fold correlates by the most-recently-started tool call (blocks
        // stream one at a time). The first fragment is `""`; empty ones are noise.
        Some("input_json_delta") => match delta.get("partial_json").and_then(Value::as_str) {
            Some(pj) if !pj.is_empty() => {
                vec![ThreadEvent::ToolInputDelta { tool_call_id: String::new(), partial_json: pj.to_string() }]
            }
            _ => Vec::new(),
        },
        // signature_delta → not rendered.
        _ => Vec::new(),
    }
}

/// Build a `LiveUsage` from a Claude streaming `usage` object. `None`/absent →
/// no event. `context_window` is always `None` here (Claude's live events omit
/// it); `cost_usd` too (cost is known only at turn-end).
fn live_usage(usage: Option<&Value>) -> Vec<ThreadEvent> {
    let Some(u) = usage else { return Vec::new() };
    let count = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    vec![ThreadEvent::LiveUsage(TurnUsage {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_read_tokens: count("cache_read_input_tokens"),
        cache_creation_tokens: count("cache_creation_input_tokens"),
        context_window: None,
        cost_usd: None,
    })]
}

fn decode_assistant(v: &Value) -> Vec<ThreadEvent> {
    let mut out = Vec::new();
    if let Some(blocks) = v["message"]["content"].as_array() {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str)
                        && !t.is_empty()
                    {
                        out.push(ThreadEvent::AssistantText(t.to_string()));
                    }
                }
                Some("thinking") => {
                    if let Some(t) = b.get("thinking").and_then(Value::as_str)
                        && !t.is_empty()
                    {
                        out.push(ThreadEvent::AssistantThinking(t.to_string()));
                    }
                }
                Some("tool_use") => out.push(ThreadEvent::ToolCallStarted {
                    id: str_field(b, "id"),
                    name: str_field(b, "name"),
                    input: b.get("input").cloned().unwrap_or(Value::Null),
                }),
                // Server-side tools (Anthropic Messages-API `server_tool_use`,
                // e.g. web_search/web_fetch/code_execution) and MCP tools
                // (`mcp_tool_use`) are the same id/name/input shape as `tool_use`
                // — route them to the same card. The `claude` CLI currently
                // wraps these as plain `tool_use`, so these arms are forward-
                // compatible coverage (the Messages API and future CLIs emit the
                // distinct block types) rather than a hot path today.
                Some("server_tool_use") => out.push(ThreadEvent::ToolCallStarted {
                    id: str_field(b, "id"),
                    name: str_field(b, "name"),
                    input: b.get("input").cloned().unwrap_or(Value::Null),
                }),
                Some("mcp_tool_use") => {
                    // Qualify with the server so two servers' same-named tools
                    // stay distinct: `<server>.<tool>` (falls back to the bare
                    // tool name when no server is present).
                    let tool = str_field(b, "name");
                    let name = match b.get("server_name").and_then(Value::as_str) {
                        Some(s) if !s.is_empty() => format!("{s}.{tool}"),
                        _ => tool,
                    };
                    out.push(ThreadEvent::ToolCallStarted {
                        id: str_field(b, "id"),
                        name,
                        input: b.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
                // Extended-thinking that the API redacted: render a muted marker,
                // never the ciphertext `data`.
                Some("redacted_thinking") => {
                    out.push(ThreadEvent::AssistantThinking("[redacted reasoning]".to_string()));
                }
                other => log_unhandled_block("assistant", other),
            }
        }
    }
    out
}

/// Debug-only breadcrumb for an assistant/user content-block `type` the decoder
/// doesn't map — so SDK drift surfaces in dev without ever panicking or logging
/// in release. A `None` type (missing key) is noise and skipped.
#[inline]
fn log_unhandled_block(channel: &str, block_type: Option<&str>) {
    #[cfg(debug_assertions)]
    if let Some(t) = block_type {
        tracing::debug!(channel, block_type = t, "stream-json: unhandled content block type");
    }
    #[cfg(not(debug_assertions))]
    let _ = (channel, block_type);
}

fn decode_user(v: &Value) -> Vec<ThreadEvent> {
    let mut out = Vec::new();
    if let Some(blocks) = v["message"]["content"].as_array() {
        for b in blocks {
            let bt = b.get("type").and_then(Value::as_str);
            if is_tool_result_block(bt) {
                let tool_use_id = str_field(b, "tool_use_id");
                let content = b.get("content");
                // The structured result sits at the line's top level (a sibling
                // of `message`), not inside the content block — same shape as
                // the on-disk `toolUseResult`, just snake_cased on the wire.
                out.push(ThreadEvent::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: flatten_tool_result_content(content),
                    is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    structured: v.get("tool_use_result").cloned(),
                });
                // Carry any inline image pixels so the card renders a thumbnail
                // rather than the flattened `[image]` placeholder alone.
                let images = extract_tool_result_images(content);
                if !images.is_empty() {
                    out.push(ThreadEvent::ToolResultImages { tool_use_id, images });
                }
            } else {
                log_unhandled_block("user", bt);
            }
        }
    }
    out
}

/// Whether a `user`-message content block is a tool result. Beyond the common
/// `tool_result`, the Messages API has a family of server-/MCP-tool result block
/// types (web_search, web_fetch, code execution, MCP) that carry the same
/// `tool_use_id` + `content` shape and route identically. The `claude` CLI wraps
/// these as plain `tool_result` today, so the extra names are forward-compatible
/// coverage rather than a hot path.
fn is_tool_result_block(block_type: Option<&str>) -> bool {
    matches!(
        block_type,
        Some(
            "tool_result"
                | "mcp_tool_result"
                | "web_search_tool_result"
                | "web_fetch_tool_result"
                | "code_execution_tool_result"
                | "bash_code_execution_tool_result"
                | "text_editor_code_execution_tool_result"
        )
    )
}

fn decode_control_request(v: &Value) -> Vec<ThreadEvent> {
    let req = &v["request"];
    if req.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return Vec::new();
    }
    // AskUserQuestion rides the same `can_use_tool` channel but is answered with
    // selections, not Allow/Reject — surface it as a distinct event so the UI
    // renders the interactive question card instead of a permission card.
    if req.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") {
        return vec![ThreadEvent::QuestionAsked {
            request_id: str_field(v, "request_id"),
            tool_use_id: req.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
            questions: parse_questions(req.get("input").unwrap_or(&Value::Null)),
        }];
    }
    // `ExitPlanMode` arrives on the same channel when the session is in plan mode:
    // its `input.plan` is the plan markdown the user approves (exit plan mode) or
    // rejects (keep planning). Tag it `Plan` so the view renders a dedicated
    // plan-approval card, and carry the plan text as the description. A missing
    // `plan` field falls through to the generic card rather than dropping the
    // request. Wire shape locked by `testdata/stream_json_exit_plan_mode.jsonl`.
    if req.get("tool_name").and_then(Value::as_str) == Some("ExitPlanMode")
        && let Some(plan) = req.get("input").and_then(|i| i.get("plan")).and_then(Value::as_str)
        && !plan.is_empty()
    {
        return vec![ThreadEvent::PermissionRequested {
            request_id: str_field(v, "request_id"),
            tool_use_id: req.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
            tool_name: "ExitPlanMode".to_string(),
            input: req.get("input").cloned().unwrap_or(Value::Null),
            description: plan.to_string(),
            suggestions: Vec::new(),
            kind: PermissionKind::Plan,
        }];
    }
    let suggestions = req["permission_suggestions"]
        .as_array()
        .map(|arr| arr.iter().map(parse_suggestion).collect())
        .unwrap_or_default();
    vec![ThreadEvent::PermissionRequested {
        request_id: str_field(v, "request_id"),
        tool_use_id: req.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
        tool_name: str_field(req, "tool_name"),
        input: req.get("input").cloned().unwrap_or(Value::Null),
        description: str_field(req, "description"),
        suggestions,
        kind: PermissionKind::Tool,
    }]
}

fn decode_result(v: &Value) -> Vec<ThreadEvent> {
    // When a turn spawns a subagent or a background bash, the CLI wakes the parent
    // once per completed task to react — and each wake emits its OWN `result`,
    // marked `origin.kind == "task-notification"`. Those are NOT the user's turn
    // ending: honoring them would flip `turn_active` mid-flight and overwrite the
    // real turn's usage footer with a continuation's tiny total. Only the
    // origin-less `result` (the user's own turn) ends the turn.
    if v.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str)
        == Some("task-notification")
    {
        return Vec::new();
    }
    // Real error subtypes are `error_max_turns`, `error_during_execution`, …
    let is_error = v
        .get("subtype")
        .and_then(Value::as_str)
        .is_some_and(|s| s.starts_with("error"))
        || v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    // Prefer the `errors[]` array (the API/CLI error carrier) over the plain
    // `result` string. A real turn error — e.g. a stale `--resume` — leaves
    // `result` absent and puts the message under `errors`; without this arm that
    // shape falls through to the generic "turn ended with error" string in the
    // state machine. A populated `result` (e.g. a bad-model 404) is the fallback.
    let result = decode_errors(v).or_else(|| v.get("result").and_then(Value::as_str).map(str::to_string));
    let mut out = Vec::new();
    // A stale `--resume` (session gone) needs recovery, not just a message: emit
    // an additive event so the fold clears the dead session id and the next send
    // starts fresh instead of looping the same error. The attempted id is the
    // one the error result echoes back.
    if is_error
        && let Some(text) = result.as_deref()
        && is_stale_resume_error(text)
    {
        out.push(ThreadEvent::SessionResumeStale {
            attempted_id: str_field(v, "session_id"),
        });
    }
    out.push(ThreadEvent::TurnEnded { result, usage: decode_usage(v), is_error, turn_diff: None });
    out
}

/// Join a result's `errors[]` string entries (the CLI's primary error-text
/// carrier) with newlines. `None` when the key is absent or the array holds no
/// strings — the decoder then falls back to the plain `result` field.
fn decode_errors(v: &Value) -> Option<String> {
    let arr = v.get("errors")?.as_array()?;
    let joined: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
    (!joined.is_empty()).then(|| joined.join("\n"))
}

/// Whether an error's text is the CLI's "session not found" shape from a
/// `--resume` against a session it no longer has. Matched against the captured
/// wire wording (`testdata/stream_json_error_stale_resume.jsonl`), not a guess.
fn is_stale_resume_error(text: &str) -> bool {
    text.contains("No conversation found with session ID")
}

/// Decode the token/cost breakdown from a `result` event. Present when the
/// result carries a `usage` object; `cost_usd` comes from the top-level
/// `total_cost_usd` and `context_window` from the first `modelUsage` entry
/// (the model's window size, for a "% of Nk" readout). Absent `usage` → `None`,
/// which the UI reads as "no footer".
fn decode_usage(v: &Value) -> Option<TurnUsage> {
    let u = v.get("usage")?;
    let count = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let context_window = v
        .get("modelUsage")
        .and_then(Value::as_object)
        .and_then(|models| models.values().next())
        .and_then(|mu| mu.get("contextWindow"))
        .and_then(Value::as_u64);
    Some(TurnUsage {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_read_tokens: count("cache_read_input_tokens"),
        cache_creation_tokens: count("cache_creation_input_tokens"),
        context_window,
        cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
    })
}

fn parse_suggestion(s: &Value) -> PermissionSuggestion {
    let kind = str_field(s, "type");
    let label = match kind.as_str() {
        "setMode" => format!("Always ({})", str_field(s, "mode")),
        "addRules" => "Always allow this pattern".to_string(),
        other => other.to_string(),
    };
    PermissionSuggestion { kind, label, raw: s.clone() }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Decode a JSON array of strings into `Vec<String>`. Missing key or non-array
/// → empty; non-string array entries are skipped (defensive against a wire
/// shape drift). Used for `init.slash_commands`.
/// `mcp_servers: [{name, status}]` from an init line. A malformed entry is
/// skipped rather than failing the whole init — the popover is informational,
/// and a missing row beats a lost session.
fn mcp_servers(v: &Value) -> Vec<McpServerStatus> {
    v.get("mcp_servers")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(McpServerStatus {
                        name: s.get("name").and_then(Value::as_str)?.to_string(),
                        status: s
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn str_list_field(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignores_noise_and_malformed() {
        assert!(decode_line("").is_empty());
        assert!(decode_line("not json").is_empty());
        assert!(decode_line(&json!({"type":"system","subtype":"status","status":"requesting"}).to_string()).is_empty());
        assert!(decode_line(&json!({"type":"system","subtype":"hook_response"}).to_string()).is_empty());
        assert!(decode_line(&json!({"type":"rate_limit_event"}).to_string()).is_empty());
        // unknown type + unknown fields must not panic
        assert!(decode_line(&json!({"type":"brand_new","x":{"y":[1,2]}}).to_string()).is_empty());
    }

    #[test]
    fn decodes_session_init() {
        // slash_commands round-trips (names only); non-string entries skipped;
        // unrelated keys ignored.
        let l = json!({"type":"system","subtype":"init","session_id":"sid-1",
            "model":"claude-sonnet-5","permissionMode":"default","extra":"ignored",
            "slash_commands":["compact","research",42,"codex:rescue"]}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::SessionInit {
            session_id: "sid-1".into(), model: "claude-sonnet-5".into(),
            permission_mode: "default".into(),
            slash_commands: vec!["compact".into(), "research".into(), "codex:rescue".into()],
            meta: SessionMeta::default() }]);
    }

    #[test]
    fn session_init_captures_the_advertised_metadata() {
        // Field names/shapes captured from a live `claude -p --output-format
        // stream-json --verbose` init line (Claude Code 2.x).
        let l = json!({"type":"system","subtype":"init","session_id":"s","model":"m",
            "permissionMode":"default","cwd":"/repo",
            "tools":["Bash","Read",7],
            "mcp_servers":[{"name":"ctx7","status":"connected"},
                           {"name":"tg","status":"pending"},
                           {"status":"nameless — skipped"}],
            "agents":["code-reviewer","planner"]}).to_string();
        match &decode_line(&l)[..] {
            [ThreadEvent::SessionInit { meta, .. }] => {
                assert_eq!(meta.cwd.as_deref(), Some("/repo"));
                // Non-string entries are skipped, not fatal.
                assert_eq!(meta.tools, vec!["Bash".to_string(), "Read".to_string()]);
                assert_eq!(meta.agents, vec!["code-reviewer".to_string(), "planner".to_string()]);
                // A server without a name is dropped; the rest still land, and an
                // unknown status string is displayed verbatim rather than matched.
                assert_eq!(meta.mcp_servers.len(), 2);
                assert_eq!(meta.mcp_servers[0].name, "ctx7");
                assert_eq!(meta.mcp_servers[0].status, "connected");
                assert_eq!(meta.mcp_servers[1].status, "pending");
            }
            other => panic!("expected SessionInit, got {other:?}"),
        }
    }

    #[test]
    fn session_init_without_slash_commands_is_empty() {
        // Absent key → empty vec (older CLI / other backends), no panic.
        let l = json!({"type":"system","subtype":"init","session_id":"s",
            "model":"m","permissionMode":"default"}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::SessionInit {
            session_id: "s".into(), model: "m".into(),
            permission_mode: "default".into(), slash_commands: vec![],
            meta: SessionMeta::default() }]);
    }

    #[test]
    fn decodes_text_and_thinking_deltas() {
        let t = json!({"type":"stream_event","event":{"type":"content_block_delta",
            "index":0,"delta":{"type":"text_delta","text":"Hel"}}}).to_string();
        assert_eq!(decode_line(&t), vec![ThreadEvent::AssistantTextDelta("Hel".into())]);
        let th = json!({"type":"stream_event","event":{"type":"content_block_delta",
            "index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}}).to_string();
        assert_eq!(decode_line(&th), vec![ThreadEvent::ThinkingDelta("hmm".into())]);
        // signature_delta and message framing → nothing
        let sig = json!({"type":"stream_event","event":{"type":"content_block_delta",
            "index":0,"delta":{"type":"signature_delta","signature":"x"}}}).to_string();
        assert!(decode_line(&sig).is_empty());
        let ms = json!({"type":"stream_event","event":{"type":"message_stop"}}).to_string();
        assert!(decode_line(&ms).is_empty());
    }

    #[test]
    fn decodes_assistant_multi_block() {
        // A finalized assistant message with thinking + text + a tool_use.
        let l = json!({"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"plan"},
            {"type":"text","text":"On it."},
            {"type":"tool_use","id":"toolu_9","name":"Edit",
             "input":{"file_path":"a.rs","old_string":"x","new_string":"y"}}
        ]}}).to_string();
        assert_eq!(decode_line(&l), vec![
            ThreadEvent::AssistantThinking("plan".into()),
            ThreadEvent::AssistantText("On it.".into()),
            ThreadEvent::ToolCallStarted {
                id: "toolu_9".into(), name: "Edit".into(),
                input: json!({"file_path":"a.rs","old_string":"x","new_string":"y"}) },
        ]);
    }

    #[test]
    fn decodes_tool_result_string_and_array() {
        let s = json!({"type":"user","message":{"content":[
            {"tool_use_id":"toolu_9","type":"tool_result","content":"1\tline one"}]}}).to_string();
        assert_eq!(decode_line(&s), vec![ThreadEvent::ToolResult {
            tool_use_id: "toolu_9".into(), content: "1\tline one".into(), is_error: false,
            structured: None }]);
        let arr = json!({"type":"user","message":{"content":[
            {"tool_use_id":"t2","type":"tool_result","is_error":true,
             "content":[{"type":"text","text":"boom"}]}]}}).to_string();
        assert_eq!(decode_line(&arr), vec![ThreadEvent::ToolResult {
            tool_use_id: "t2".into(), content: "boom".into(), is_error: true,
            structured: None }]);
    }

    #[test]
    fn decodes_tool_result_captures_top_level_structured() {
        // The rich `tool_use_result` rides at the line's top level (snake_case),
        // a sibling of `message` — not inside the content block. It must reach
        // `ThreadEvent::ToolResult.structured` so live cards enrich like history.
        let l = json!({"type":"user","message":{"content":[
            {"tool_use_id":"tb","type":"tool_result","content":"done"}]},
            "tool_use_result":{"stdout":"","stderr":"boom","interrupted":true}}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::ToolResult {
            tool_use_id: "tb".into(), content: "done".into(), is_error: false,
            structured: Some(json!({"stdout":"","stderr":"boom","interrupted":true})) }]);
    }

    #[test]
    fn decodes_permission_request_with_suggestions() {
        // Exact shape captured from the spike (probe D/E).
        let l = json!({"type":"control_request","request_id":"rid-7","request":{
            "subtype":"can_use_tool","tool_name":"Edit","display_name":"Edit",
            "input":{"file_path":"notes.txt","old_string":"a","new_string":"b","replace_all":false},
            "description":"notes.txt",
            "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],
            "tool_use_id":"toolu_5"}}).to_string();
        let evs = decode_line(&l);
        match &evs[0] {
            ThreadEvent::PermissionRequested { request_id, tool_use_id, tool_name, description, suggestions, .. } => {
                assert_eq!(request_id, "rid-7");
                assert_eq!(tool_use_id.as_deref(), Some("toolu_5"));
                assert_eq!(tool_name, "Edit");
                assert_eq!(description, "notes.txt");
                assert_eq!(suggestions.len(), 1);
                assert_eq!(suggestions[0].kind, "setMode");
                assert_eq!(suggestions[0].label, "Always (acceptEdits)");
            }
            other => panic!("expected PermissionRequested, got {other:?}"),
        }
    }

    #[test]
    fn decodes_ask_user_question_as_question_event_not_permission() {
        // AskUserQuestion arrives on the can_use_tool channel but must decode to
        // QuestionAsked (the interactive card), NOT PermissionRequested.
        let l = json!({"type":"control_request","request_id":"rid-q","request":{
            "subtype":"can_use_tool","tool_name":"AskUserQuestion","display_name":"AskUserQuestion",
            "input":{"questions":[{"question":"Tabs or spaces?","header":"Indent",
                "options":[{"label":"Tabs","description":"tab chars"},{"label":"Spaces","description":"space chars"}],
                "multiSelect":false}]},
            "tool_use_id":"toolu_q"}}).to_string();
        match &decode_line(&l)[0] {
            ThreadEvent::QuestionAsked { request_id, tool_use_id, questions } => {
                assert_eq!(request_id, "rid-q");
                assert_eq!(tool_use_id.as_deref(), Some("toolu_q"));
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].question, "Tabs or spaces?");
                assert_eq!(questions[0].options.len(), 2);
                assert!(!questions[0].multi_select());
            }
            other => panic!("expected QuestionAsked, got {other:?}"),
        }
        // A real gated tool on the same channel still decodes to a permission.
        let edit = json!({"type":"control_request","request_id":"rid-e","request":{
            "subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"a"},"description":"a",
            "tool_use_id":"toolu_e"}}).to_string();
        assert!(matches!(decode_line(&edit)[0], ThreadEvent::PermissionRequested { .. }));
    }

    #[test]
    fn decodes_exit_plan_mode_as_plan_permission_with_plan_markdown() {
        // ExitPlanMode rides the can_use_tool channel; it must decode to a
        // `Plan`-kind permission whose description carries the plan markdown, so
        // the view routes it to the dedicated plan-approval card. Wire shape from
        // testdata/stream_json_exit_plan_mode.jsonl.
        let line = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/thread/testdata/stream_json_exit_plan_mode.jsonl"),
        )
        .unwrap();
        let evs = decode_line(line.trim());
        match &evs[0] {
            ThreadEvent::PermissionRequested { tool_name, description, kind, .. } => {
                assert_eq!(tool_name, "ExitPlanMode");
                assert_eq!(*kind, PermissionKind::Plan);
                assert!(description.contains("hello()"), "plan markdown is the description");
            }
            other => panic!("expected Plan PermissionRequested, got {other:?}"),
        }
        // Defensive: an ExitPlanMode with no `plan` falls back to the generic
        // Tool card rather than dropping the request.
        let bare = json!({"type":"control_request","request_id":"r","request":{
            "subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{},
            "tool_use_id":"t"}}).to_string();
        match &decode_line(&bare)[0] {
            ThreadEvent::PermissionRequested { kind, .. } => assert_eq!(*kind, PermissionKind::Tool),
            other => panic!("expected fallback Tool permission, got {other:?}"),
        }
    }

    #[test]
    fn decodes_content_block_start_tool_use_as_early_tool_call() {
        // The card opens here — id + name known, input still empty.
        let l = json!({"type":"stream_event","event":{"type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"toolu_9","name":"Write","input":{}}}}).to_string();
        match &decode_line(&l)[0] {
            ThreadEvent::ToolCallStarted { id, name, input } => {
                assert_eq!(id, "toolu_9");
                assert_eq!(name, "Write");
                assert_eq!(*input, json!({}));
            }
            other => panic!("expected early ToolCallStarted, got {other:?}"),
        }
        // A text content_block_start starts nothing (text streams via deltas).
        let text = json!({"type":"stream_event","event":{"type":"content_block_start","index":0,
            "content_block":{"type":"text"}}}).to_string();
        assert!(decode_line(&text).is_empty());
    }

    #[test]
    fn decodes_input_json_delta_as_tool_input_delta() {
        let l = json!({"type":"stream_event","event":{"type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":"{\"file_path\": \"/tmp/x"}}}).to_string();
        match &decode_line(&l)[0] {
            ThreadEvent::ToolInputDelta { tool_call_id, partial_json } => {
                assert!(tool_call_id.is_empty(), "delta carries no id; fold correlates it");
                assert_eq!(partial_json, "{\"file_path\": \"/tmp/x");
            }
            other => panic!("expected ToolInputDelta, got {other:?}"),
        }
        // An empty first fragment is noise.
        let empty = json!({"type":"stream_event","event":{"type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":""}}}).to_string();
        assert!(decode_line(&empty).is_empty());
    }

    #[test]
    fn tool_input_delta_fixture_replays_into_growing_then_final_card() {
        use crate::thread::state::ChatThread;
        use crate::thread::tool_call::ToolCallStatus;
        // Replay the captured Write fixture through the decoder + fold: the card
        // opens early, its preview grows as fragments arrive, and the finalized
        // tool_use block supersedes the preview with authoritative input.
        let fixture = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/thread/testdata/stream_json_tool_input_delta.jsonl"),
        )
        .unwrap();
        let mut thread = ChatThread::new();
        let mut saw_growing_preview = false;
        for line in fixture.lines() {
            for ev in decode_line(line) {
                thread.apply(&ev);
            }
            // Mid-stream, the open Write card should preview partial args.
            if let Some(tc) = thread.entries.iter().rev().find_map(|e| match e {
                crate::thread::ThreadEntry::ToolCall(tc) if tc.name == "Write" => Some(tc),
                _ => None,
            }) && tc.input == serde_json::Value::from(serde_json::Map::new())
                && let Some(preview) = tc.preview_input()
                && preview.get("file_path").is_some()
            {
                saw_growing_preview = true;
            }
        }
        assert!(saw_growing_preview, "a partial preview rendered before finalize");
        // Exactly one Write card, with the authoritative finalized input.
        let writes: Vec<_> = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                crate::thread::ThreadEntry::ToolCall(tc) if tc.name == "Write" => Some(tc),
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 1, "no duplicate tool card for the same id");
        let tc = writes[0];
        assert!(tc.partial_input.is_empty(), "preview buffer cleared at finalize");
        assert!(tc.input.get("file_path").and_then(|v| v.as_str()).is_some());
        assert!(tc.input.get("content").and_then(|v| v.as_str()).unwrap().contains("ocean"));
        assert!(matches!(tc.status, ToolCallStatus::InProgress | ToolCallStatus::Completed));
    }

    #[test]
    fn decodes_result_success_and_error() {
        // Real result shape (from the captured stream): usage tokens under
        // `usage`, window size under `modelUsage`, cost at top level.
        let ok = json!({"type":"result","subtype":"success","is_error":false,
            "result":"Done.","total_cost_usd":0.33,
            "usage":{"input_tokens":714,"output_tokens":7,
                "cache_read_input_tokens":16681,"cache_creation_input_tokens":5571},
            "modelUsage":{"claude-opus-4-8":{"contextWindow":200000}}}).to_string();
        match &decode_line(&ok)[0] {
            ThreadEvent::TurnEnded { result, usage, is_error, .. } => {
                assert_eq!(result.as_deref(), Some("Done."));
                assert!(!is_error);
                let u = usage.as_ref().expect("usage decoded");
                assert_eq!(u.input_tokens, 714);
                assert_eq!(u.output_tokens, 7);
                assert_eq!(u.cache_read_tokens, 16681);
                assert_eq!(u.cache_creation_tokens, 5571);
                assert_eq!(u.context_window, Some(200000));
                assert_eq!(u.cost_usd, Some(0.33));
            }
            other => panic!("expected TurnEnded, got {other:?}"),
        }
        // No `usage` object → no footer material.
        let bare = json!({"type":"result","subtype":"success","result":"ok","total_cost_usd":0.1}).to_string();
        match &decode_line(&bare)[0] {
            ThreadEvent::TurnEnded { usage, .. } => assert!(usage.is_none(), "no usage key → None"),
            other => panic!("expected TurnEnded, got {other:?}"),
        }
        let err = json!({"type":"result","subtype":"error_max_turns","result":"stopped"}).to_string();
        match &decode_line(&err)[0] {
            ThreadEvent::TurnEnded { is_error, .. } => assert!(*is_error),
            other => panic!("expected TurnEnded error, got {other:?}"),
        }
    }

    #[test]
    fn message_start_and_delta_emit_live_usage() {
        // message_start seeds input/cache; message_delta carries the running
        // totals (output grows). Neither carries a context window.
        let start = json!({"type":"stream_event","event":{"type":"message_start",
            "message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":500,
                "cache_read_input_tokens":20,"output_tokens":1}}}}).to_string();
        match &decode_line(&start)[..] {
            [ThreadEvent::LiveUsage(u)] => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.cache_creation_tokens, 500);
                assert_eq!(u.cache_read_tokens, 20);
                assert_eq!(u.output_tokens, 1);
                assert_eq!(u.context_window, None, "Claude live events omit the window");
                assert_eq!(u.cost_usd, None);
            }
            other => panic!("expected LiveUsage, got {other:?}"),
        }
        let delta = json!({"type":"stream_event","event":{"type":"message_delta",
            "usage":{"input_tokens":10,"cache_read_input_tokens":20,
                "cache_creation_input_tokens":500,"output_tokens":160}}}).to_string();
        match &decode_line(&delta)[..] {
            [ThreadEvent::LiveUsage(u)] => assert_eq!(u.output_tokens, 160),
            other => panic!("expected LiveUsage, got {other:?}"),
        }
        // A message_delta with no usage object → nothing.
        let bare = json!({"type":"stream_event","event":{"type":"message_delta","delta":{}}}).to_string();
        assert!(decode_line(&bare).is_empty());
    }

    #[test]
    fn decode_result_prefers_errors_array_over_result_field() {
        // The `errors[]` array is the primary error carrier; a `result` string,
        // when both are present, is the fallback. Here only `errors[]` exists.
        let l = json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "session_id":"sid-x",
            "errors":["boom one","boom two"]}).to_string();
        let evs = decode_line(&l);
        let te = evs.iter().find(|e| matches!(e, ThreadEvent::TurnEnded { .. })).expect("TurnEnded");
        match te {
            ThreadEvent::TurnEnded { result, is_error, .. } => {
                assert!(is_error);
                assert_eq!(result.as_deref(), Some("boom one\nboom two"), "errors[] joined with newline");
            }
            _ => unreachable!(),
        }
        // A `result` field with no `errors[]` still decodes as before.
        let plain = json!({"type":"result","subtype":"success","is_error":true,
            "result":"model not found"}).to_string();
        match decode_line(&plain).into_iter().next().unwrap() {
            ThreadEvent::TurnEnded { result, .. } => assert_eq!(result.as_deref(), Some("model not found")),
            other => panic!("expected TurnEnded, got {other:?}"),
        }
    }

    #[test]
    fn stale_resume_fixture_emits_recovery_event_before_turn_end() {
        // The captured real stale-`--resume` error: no `result` field, the text
        // rides in `errors[]`, and `session_id` echoes the attempted id. Decode
        // must surface both a SessionResumeStale (carrying that id) AND the
        // error TurnEnded, in that order.
        let fixture = include_str!("testdata/stream_json_error_stale_resume.jsonl");
        let events: Vec<ThreadEvent> = fixture.lines().flat_map(decode_line).collect();
        assert!(matches!(
            events.as_slice(),
            [
                ThreadEvent::SessionResumeStale { .. },
                ThreadEvent::TurnEnded { is_error: true, .. },
            ]
        ), "got {events:?}");
        match &events[0] {
            ThreadEvent::SessionResumeStale { attempted_id } => {
                assert_eq!(attempted_id, "00000000-0000-0000-0000-000000000000");
            }
            _ => unreachable!(),
        }
        match &events[1] {
            ThreadEvent::TurnEnded { result, .. } => {
                assert_eq!(
                    result.as_deref(),
                    Some("No conversation found with session ID: 00000000-0000-0000-0000-000000000000"),
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn api_error_fixture_surfaces_result_text_not_generic() {
        // A generic API error (bad model) carries its message in `result`, no
        // `errors[]`, and must NOT be mistaken for a stale-resume.
        let fixture = include_str!("testdata/stream_json_error_api.jsonl");
        let events: Vec<ThreadEvent> = fixture.lines().flat_map(decode_line).collect();
        assert!(!events.iter().any(|e| matches!(e, ThreadEvent::SessionResumeStale { .. })));
        let te = events.iter().find(|e| matches!(e, ThreadEvent::TurnEnded { .. })).expect("TurnEnded");
        match te {
            ThreadEvent::TurnEnded { result, is_error, .. } => {
                assert!(is_error);
                assert!(result.as_deref().unwrap().contains("selected model"), "actual error text surfaced");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn is_stale_resume_error_matches_captured_wording() {
        assert!(is_stale_resume_error("No conversation found with session ID: abc-123"));
        assert!(!is_stale_resume_error("There's an issue with the selected model"));
        assert!(!is_stale_resume_error("turn ended with error"));
    }

    #[test]
    fn decodes_server_and_mcp_tool_use() {
        // server_tool_use keeps its name; mcp_tool_use qualifies with the server.
        let srv = json!({"type":"assistant","message":{"content":[
            {"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"x"}}]}}).to_string();
        assert_eq!(decode_line(&srv), vec![ThreadEvent::ToolCallStarted {
            id: "srv_1".into(), name: "web_search".into(), input: json!({"query":"x"}) }]);
        let mcp = json!({"type":"assistant","message":{"content":[
            {"type":"mcp_tool_use","id":"mcp_1","name":"query-docs","server_name":"context7","input":{"q":"tokio"}}]}}).to_string();
        assert_eq!(decode_line(&mcp), vec![ThreadEvent::ToolCallStarted {
            id: "mcp_1".into(), name: "context7.query-docs".into(), input: json!({"q":"tokio"}) }]);
        // mcp_tool_use without a server_name falls back to the bare tool name.
        let mcp_bare = json!({"type":"assistant","message":{"content":[
            {"type":"mcp_tool_use","id":"mcp_2","name":"solo","input":{}}]}}).to_string();
        match &decode_line(&mcp_bare)[0] {
            ThreadEvent::ToolCallStarted { name, .. } => assert_eq!(name, "solo"),
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn decodes_all_tool_result_variants_as_tool_result() {
        // The server-/MCP-tool result block types route identically to the plain
        // `tool_result` (same tool_use_id + content shape).
        for ty in [
            "tool_result", "mcp_tool_result", "web_search_tool_result", "web_fetch_tool_result",
            "code_execution_tool_result", "bash_code_execution_tool_result",
            "text_editor_code_execution_tool_result",
        ] {
            let l = json!({"type":"user","message":{"content":[
                {"type":ty,"tool_use_id":"tu","content":[{"type":"text","text":"out"}]}]}}).to_string();
            assert_eq!(
                decode_line(&l),
                vec![ThreadEvent::ToolResult {
                    tool_use_id: "tu".into(), content: "out".into(), is_error: false, structured: None }],
                "variant {ty} must decode to ToolResult",
            );
        }
    }

    #[test]
    fn decodes_tool_result_image_emits_images_event() {
        use crate::thread::ChatImage;
        // A tool_result carrying an inline base64 image → the flattened
        // `[image]` placeholder PLUS a follow-up ToolResultImages with the pixels.
        let l = json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"ti","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}]}]}}).to_string();
        assert_eq!(decode_line(&l), vec![
            ThreadEvent::ToolResult {
                tool_use_id: "ti".into(), content: "[image]".into(), is_error: false, structured: None },
            ThreadEvent::ToolResultImages {
                tool_use_id: "ti".into(),
                images: vec![ChatImage { media_type: "image/png".into(), data: "QUJD".into() }] },
        ]);
        // A URL-sourced image is NOT inlined (no base64) — only the placeholder.
        let url = json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"tu2","content":[
                {"type":"image","source":{"type":"url","url":"http://x/y.png"}}]}]}}).to_string();
        assert!(matches!(decode_line(&url).as_slice(), [ThreadEvent::ToolResult { .. }]));
    }

    #[test]
    fn decodes_redacted_thinking_as_muted_marker() {
        // Never render the ciphertext `data`; emit a fixed muted marker instead.
        let l = json!({"type":"assistant","message":{"content":[
            {"type":"redacted_thinking","data":"SECRET_CIPHERTEXT=="}]}}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::AssistantThinking("[redacted reasoning]".into())]);
    }

    #[test]
    fn decodes_compacting_status_but_drops_other_status() {
        // `status: "compacting"` promotes to CompactionStarted; other statuses drop.
        let compacting = json!({"type":"system","subtype":"status","status":"compacting"}).to_string();
        assert_eq!(decode_line(&compacting), vec![ThreadEvent::CompactionStarted]);
        let requesting = json!({"type":"system","subtype":"status","status":"requesting"}).to_string();
        assert!(decode_line(&requesting).is_empty());
        // No status field → dropped.
        assert!(decode_line(&json!({"type":"system","subtype":"status"}).to_string()).is_empty());
    }

    #[test]
    fn compacting_flag_set_then_cleared_by_boundary_and_turn_end() {
        use crate::thread::state::ChatThread;
        let mut t = ChatThread::new();
        t.apply(&ThreadEvent::CompactionStarted);
        assert!(t.compacting, "spinner shows during compaction");
        t.apply(&ThreadEvent::CompactBoundary { summary: "Context compacted".into() });
        assert!(!t.compacting, "boundary supersedes the spinner");
        // A compaction that never reaches a boundary clears on turn end.
        t.apply(&ThreadEvent::CompactionStarted);
        assert!(t.compacting);
        t.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
        assert!(!t.compacting, "turn end clears a dangling spinner");
    }

    #[test]
    fn decodes_compact_boundary_divider() {
        let auto = json!({"type":"system","subtype":"compact_boundary",
            "compact_metadata":{"trigger":"auto","pre_tokens":150000}}).to_string();
        assert_eq!(decode_line(&auto), vec![ThreadEvent::CompactBoundary {
            summary: "Context compacted (auto)".into() }]);
        // Absent metadata → the plain label, no panic.
        let bare = json!({"type":"system","subtype":"compact_boundary"}).to_string();
        assert_eq!(decode_line(&bare), vec![ThreadEvent::CompactBoundary {
            summary: "Context compacted".into() }]);
    }

    /// Fixture replay: a turn exercising the full new taxonomy (server/mcp tool
    /// use + results, a real image Read, redacted thinking, compaction) decodes
    /// to the expected event sequence with the image pixels carried through.
    #[test]
    fn richtools_fixture_decodes_full_taxonomy() {
        use crate::thread::ChatImage;
        let fixture = include_str!("testdata/stream_json_richtools.jsonl");
        let events: Vec<ThreadEvent> = fixture.lines().flat_map(decode_line).collect();

        // Two tool calls surface (web_search, context7.query-docs, Read).
        let names: Vec<&str> = events.iter().filter_map(|e| match e {
            ThreadEvent::ToolCallStarted { name, .. } => Some(name.as_str()),
            _ => None,
        }).collect();
        assert_eq!(names, ["web_search", "context7.query-docs", "Read"]);

        // The image Read produced a ToolResultImages with the real PNG.
        let imgs: Vec<&ChatImage> = events.iter().filter_map(|e| match e {
            ThreadEvent::ToolResultImages { images, .. } => images.first(),
            _ => None,
        }).collect();
        assert_eq!(imgs.len(), 1, "exactly one inline tool-result image");
        assert_eq!(imgs[0].media_type, "image/png");
        assert!(imgs[0].data.starts_with("iVBOR"), "carries the base64 PNG pixels");

        // Redacted thinking became the muted marker; a compaction divider fired.
        assert!(events.contains(&ThreadEvent::AssistantThinking("[redacted reasoning]".into())));
        assert!(events.iter().any(|e| matches!(e, ThreadEvent::CompactBoundary { .. })));
        // Exactly one turn end.
        assert_eq!(events.iter().filter(|e| matches!(e, ThreadEvent::TurnEnded { .. })).count(), 1);
    }

    #[test]
    fn decodes_post_turn_summary() {
        let l = json!({"type":"system","subtype":"post_turn_summary",
            "status_category":"review_ready","status_detail":"appended 'line four' to notes.txt"}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::TurnSummary {
            detail: "appended 'line four' to notes.txt".into(), category: "review_ready".into() }]);
    }

    // --- Background-task stream hygiene (subagents + background bash) ---

    /// A subagent's thinking becomes one truncated `[thinking] …` log line
    /// (first line only) rather than a wall of reasoning in a card.
    ///
    /// The block shape (`{"type":"thinking","thinking":"…"}`) is the one the main
    /// agent's own path already decodes and is verified there. Subagents were not
    /// observed emitting thinking in live probing — the main agent thinks under
    /// identical flags, but two subagent probes produced none, one of which asked
    /// for step-by-step reasoning explicitly. So this arm is coverage for a shape
    /// that is known-correct but currently unobserved on this path: dormant if
    /// subagents never think, right if they start.
    #[test]
    fn subagent_thinking_becomes_one_truncated_line() {
        let long = "x".repeat(200);
        let think = json!({"type":"assistant","parent_tool_use_id":"toolu_ABC","message":{"content":[
            {"type":"thinking","thinking":format!("First I should check the config.\n{long}")}]}})
        .to_string();
        assert_eq!(decode_line(&think), vec![ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_ABC".into(),
            line: "[thinking] First I should check the config.".into() }]);
        // Empty thinking contributes no line.
        let empty = json!({"type":"assistant","parent_tool_use_id":"toolu_ABC",
            "message":{"content":[{"type":"thinking","thinking":"   "}]}}).to_string();
        assert!(decode_line(&empty).is_empty(), "empty thinking logs nothing");
    }

    /// A subagent's internal `tool_use` streams inline tagged with the spawning
    /// tool's id; it must NOT fold as the parent agent's own tool card — instead
    /// it routes to that card's log as a `SubagentAction` line.
    #[test]
    fn parent_tool_use_id_tool_use_routes_to_subagent_log() {
        let sub = json!({
            "type":"assistant","parent_tool_use_id":"toolu_ABC",
            "message":{"content":[{"type":"tool_use","id":"toolu_x","name":"Read","input":{"file_path":"src/main.rs"}}]}
        }).to_string();
        assert_eq!(decode_line(&sub), vec![ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_ABC".into(), line: "Read src/main.rs".into() }]);
        // A child text block becomes a first-line summary action.
        let txt = json!({
            "type":"assistant","parent_tool_use_id":"toolu_ABC",
            "message":{"content":[{"type":"text","text":"Looking at the auth module.\nmore detail"}]}
        }).to_string();
        assert_eq!(decode_line(&txt), vec![ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_ABC".into(), line: "Looking at the auth module.".into() }]);
        // Child deltas are still dropped (no per-delta churn in the log).
        let delta = json!({"type":"stream_event","parent_tool_use_id":"toolu_ABC",
            "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}}).to_string();
        assert!(decode_line(&delta).is_empty(), "child delta stays dropped");
        // A SUCCESSFUL child result stays dropped: the `tool_use` line above
        // already logged the action, so a second line saying it worked is noise.
        let result = json!({"type":"user","parent_tool_use_id":"toolu_ABC",
            "message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"out"}]}}).to_string();
        assert!(decode_line(&result).is_empty(), "successful child result stays dropped");
        // A FAILED child result is logged — it is the surprising event, and the
        // one that explains a subagent going quiet or changing course. Payload is
        // a real captured `Read /nonexistent` failure.
        let failed = json!({"type":"user","parent_tool_use_id":"toolu_ABC","message":{"content":[
            {"tool_use_id":"toolu_01Tpfm","type":"tool_result","is_error":true,
             "content":"File does not exist. Note: your current working directory is /tmp/p."}]}}).to_string();
        assert_eq!(decode_line(&failed), vec![ThreadEvent::SubagentAction {
            parent_tool_call_id: "toolu_ABC".into(),
            line: "✗ File does not exist. Note: your current working directory is /tmp/p.".into() }]);
        // The child's PROMPT echoes back as a tagged `user`/`text` line. It is not
        // an action the subagent took, and the card already shows the Task's own
        // description — so it must not open the log with a duplicate of it.
        let prompt = json!({"type":"user","parent_tool_use_id":"toolu_ABC","message":{"content":[
            {"type":"text","text":"read sample.rs in the cwd and reply with the word it prints"}]}}).to_string();
        assert!(decode_line(&prompt).is_empty(), "child prompt echo is not an action");

        // The same tool_use WITHOUT the tag decodes as a real tool card.
        let main = json!({
            "type":"assistant",
            "message":{"content":[{"type":"tool_use","id":"toolu_x","name":"Read","input":{}}]}
        }).to_string();
        assert!(matches!(decode_line(&main).as_slice(), [ThreadEvent::ToolCallStarted { .. }]));
    }

    /// A `result` marked `origin.kind == "task-notification"` is a background-task
    /// continuation, not the user's turn ending — it must not emit `TurnEnded`.
    #[test]
    fn task_notification_result_does_not_end_the_turn() {
        let cont = json!({
            "type":"result","subtype":"success","origin":{"kind":"task-notification"},
            "usage":{"output_tokens":30}
        }).to_string();
        assert!(decode_line(&cont).is_empty(), "task-notification result must not end the turn");
        // An origin-less result still ends the turn.
        let real = json!({"type":"result","subtype":"success","usage":{"output_tokens":381}}).to_string();
        assert!(matches!(decode_line(&real).as_slice(), [ThreadEvent::TurnEnded { .. }]));
    }

    /// End-to-end against the captured spike fixture (one turn spawning a subagent
    /// + a background bash): the subagent's own tool calls stay out of the main
    /// stream, and exactly one — the primary — `TurnEnded` is emitted with the
    /// real turn's usage (not a continuation's tiny total).
    #[test]
    fn spike_fixture_keeps_main_stream_clean() {
        let fixture = include_str!("testdata/stream_json_subagent.jsonl");
        let events: Vec<ThreadEvent> = fixture.lines().flat_map(decode_line).collect();

        let tool_names: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::ToolCallStarted { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        // Only the parent's two tool calls survive as real cards; the subagent's
        // Read/Bash calls (`parent_tool_use_id`-tagged) route to the log, not a card.
        assert_eq!(tool_names, ["Agent", "Bash"], "subagent tool calls leaked: {tool_names:?}");

        // The subagent's child tool calls surface as SubagentAction lines addressed
        // to the spawning Agent card's id — folded, they populate that card's log
        // while the root transcript stays free of child bubbles.
        use crate::thread::state::ChatThread;
        let mut thread = ChatThread::new();
        for ev in &events {
            thread.apply(ev);
        }
        let agent_card = thread.entries.iter().find_map(|e| match e {
            crate::thread::ThreadEntry::ToolCall(tc) if tc.name == "Agent" => Some(tc),
            _ => None,
        }).expect("Agent card exists");
        assert!(!agent_card.subagent_log.is_empty(), "subagent log populated from child actions");
        assert!(
            agent_card.subagent_log.iter().any(|l| l.starts_with("Read ") || l.starts_with("Bash ")),
            "child tool actions logged: {:?}", agent_card.subagent_log,
        );

        let turn_ends: Vec<&TurnUsage> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::TurnEnded { usage: Some(u), .. } => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(turn_ends.len(), 1, "exactly one turn-end (not the task-notification continuations)");
        assert_eq!(turn_ends[0].output_tokens, 381, "usage footer must be the primary turn's, not a continuation's");
    }

    /// The fixture's `system/task_*` events decode into one subagent + one
    /// background-bash task, each with a start and a terminal finish.
    #[test]
    fn spike_fixture_emits_background_task_events() {
        use super::super::background_task::BackgroundTaskKind;
        let fixture = include_str!("testdata/stream_json_subagent.jsonl");
        let events: Vec<ThreadEvent> = fixture.lines().flat_map(decode_line).collect();

        let started: Vec<&ThreadEvent> = events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::BackgroundTaskStarted { .. }))
            .collect();
        assert_eq!(started.len(), 2, "one subagent + one background bash started");
        let kinds: Vec<&BackgroundTaskKind> = started
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::BackgroundTaskStarted { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert!(kinds.iter().any(|k| matches!(k, BackgroundTaskKind::BackgroundBash)));
        assert!(kinds.iter().any(|k| matches!(
            k,
            BackgroundTaskKind::Subagent { subagent_type } if subagent_type == "general-purpose"
        )));

        let finished = events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::BackgroundTaskFinished { .. }))
            .count();
        assert!(finished >= 2, "both tasks reach a terminal finish, got {finished}");
    }
}
