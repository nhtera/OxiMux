//! Codex app-server (v2) notification → [`ThreadEvent`] mapping.
//!
//! codex 0.141.0's v2 protocol has a **single** event channel (item lifecycle +
//! turn/thread events) — the legacy `codex/event/<snake>` mirror the older Paseo
//! reference warns about does NOT exist here, so there is no dual-channel dedup
//! to do. `item/started` carries the `ThreadItem` at the start of a tool/message,
//! `item/completed` carries the authoritative final item (so text/reasoning are
//! finalized from it, not from a delta buffer). Verified via
//! `codex app-server generate-ts`.

use serde_json::{json, Value};

use super::super::event::{PlanEntryLite, ThreadEvent, TurnUsage};
use super::CodexState;

/// Map one server → client notification to zero-or-more `ThreadEvent`s,
/// updating the shared `CodexState` (turn id, last usage). Unknown methods and
/// unknown item types degrade gracefully (a generic card / skip) — never panic.
pub fn map_notification(method: &str, params: &Value, st: &mut CodexState) -> Vec<ThreadEvent> {
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
                item_completed(item)
            }
            None => Vec::new(),
        },

        // --- turn lifecycle -----------------------------------------------
        "turn/started" => {
            if let Some(tid) = params.get("turn").and_then(|t| t.get("id")).and_then(|v| v.as_str()) {
                st.current_turn_id = Some(tid.to_string());
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
            let usage = st.last_usage.take();
            vec![ThreadEvent::TurnEnded { result: None, usage, is_error }]
        }

        // --- usage (footer wired by a later phase's emits_usage cap) -------
        "thread/tokenUsage/updated" => {
            st.last_usage = parse_usage(params);
            Vec::new()
        }

        "error" => vec![ThreadEvent::Error(error_message(params))],

        // The backend compacted earlier context — mirror Claude's divider.
        "thread/compacted" => vec![ThreadEvent::CompactBoundary {
            summary: "Context compacted".to_string(),
        }],

        // Housekeeping / telemetry / not-yet-surfaced — intentionally ignored:
        // thread/started, thread/*, turn/diff/updated, item/fileChange/outputDelta,
        // mcpServer/*, skills/changed, remoteControl/*, …
        _ => Vec::new(),
    }
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
/// message/reasoning, or a `ToolResult` for a tool item.
fn item_completed(item: &Value) -> Vec<ThreadEvent> {
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
        _ if tool_call(item).is_some() => {
            let (content, is_error, structured) = tool_result(ty, item);
            vec![ThreadEvent::ToolResult { tool_use_id: id, content, is_error, structured }]
        }
        // userMessage/plan/hookPrompt/contextCompaction/review-modes/image*/
        // sleep/subAgentActivity: not surfaced in the transcript for P1/P2.
        _ => Vec::new(),
    }
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
            Some(("Bash".to_string(), json!({ "command": command, "cwd": cwd })))
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
        "collabAgentToolCall" | "subAgentActivity" => {
            let tool = item.get("tool").and_then(|v| v.as_str()).unwrap_or("sub_agent");
            Some((tool.to_string(), item.clone()))
        }
        // Known non-tool items — not tool cards.
        "agentMessage" | "reasoning" | "plan" | "userMessage" | "hookPrompt"
        | "contextCompaction" | "enteredReviewMode" | "exitedReviewMode" | "imageGeneration"
        | "imageView" | "sleep" => None,
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
        _ => (String::new(), failed, None),
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
}
