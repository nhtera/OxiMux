//! Decoder: Claude Code `stream-json` wire events → `ThreadEvent`.
//!
//! Pure and sync: one raw JSON line in, zero-or-more `ThreadEvent`s out. A
//! single `assistant` line can carry several content blocks (text + tool_use),
//! hence `Vec`. Unknown `type`s, unknown subtypes, and unknown fields are
//! ignored — the wire schema drifts across `claude` versions and the parser
//! must never panic on it (see spike-findings.md for the captured vocabulary).

use serde_json::Value;

use super::event::{ThreadEvent, TurnUsage};
use super::question::parse_questions;
use super::tool_call::PermissionSuggestion;

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
    match v.get("type").and_then(Value::as_str) {
        Some("system") => decode_system(v),
        Some("stream_event") => decode_stream_event(v),
        Some("assistant") => decode_assistant(v),
        Some("user") => decode_user(v),
        Some("control_request") => decode_control_request(v),
        Some("result") => vec![decode_result(v)],
        _ => Vec::new(),
    }
}

fn decode_system(v: &Value) -> Vec<ThreadEvent> {
    match v.get("subtype").and_then(Value::as_str) {
        Some("init") => vec![ThreadEvent::SessionInit {
            session_id: str_field(v, "session_id"),
            model: str_field(v, "model"),
            permission_mode: str_field(v, "permissionMode"),
        }],
        Some("post_turn_summary") => vec![ThreadEvent::TurnSummary {
            detail: str_field(v, "status_detail"),
            category: str_field(v, "status_category"),
        }],
        // hook_started / hook_response → noise.
        // TODO(chat-ui polish): `system/status` (spinner text) and init's
        // tools/mcp_servers/slash_commands/agents/cwd are dropped for now;
        // surface them when the spinner + session-detail UI is built.
        _ => Vec::new(),
    }
}

fn decode_stream_event(v: &Value) -> Vec<ThreadEvent> {
    let ev = &v["event"];
    if ev.get("type").and_then(Value::as_str) != Some("content_block_delta") {
        return Vec::new();
    }
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
        // signature_delta / input_json_delta → not rendered.
        _ => Vec::new(),
    }
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
                _ => {}
            }
        }
    }
    out
}

fn decode_user(v: &Value) -> Vec<ThreadEvent> {
    let mut out = Vec::new();
    if let Some(blocks) = v["message"]["content"].as_array() {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                out.push(ThreadEvent::ToolResult {
                    tool_use_id: str_field(b, "tool_use_id"),
                    content: content_to_string(b.get("content")),
                    is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
    }
    out
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
    }]
}

fn decode_result(v: &Value) -> ThreadEvent {
    // Real error subtypes are `error_max_turns`, `error_during_execution`, …
    let is_error = v
        .get("subtype")
        .and_then(Value::as_str)
        .is_some_and(|s| s.starts_with("error"))
        || v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    ThreadEvent::TurnEnded {
        result: v.get("result").and_then(Value::as_str).map(str::to_string),
        usage: decode_usage(v),
        is_error,
    }
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

/// A `tool_result.content` is either a plain string or an array of
/// `{type:"text", text:"..."}` blocks — flatten both to a string.
fn content_to_string(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
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
        let l = json!({"type":"system","subtype":"init","session_id":"sid-1",
            "model":"claude-sonnet-5","permissionMode":"default","extra":"ignored"}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::SessionInit {
            session_id: "sid-1".into(), model: "claude-sonnet-5".into(),
            permission_mode: "default".into() }]);
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
            tool_use_id: "toolu_9".into(), content: "1\tline one".into(), is_error: false }]);
        let arr = json!({"type":"user","message":{"content":[
            {"tool_use_id":"t2","type":"tool_result","is_error":true,
             "content":[{"type":"text","text":"boom"}]}]}}).to_string();
        assert_eq!(decode_line(&arr), vec![ThreadEvent::ToolResult {
            tool_use_id: "t2".into(), content: "boom".into(), is_error: true }]);
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
    fn decodes_result_success_and_error() {
        // Real result shape (from the captured stream): usage tokens under
        // `usage`, window size under `modelUsage`, cost at top level.
        let ok = json!({"type":"result","subtype":"success","is_error":false,
            "result":"Done.","total_cost_usd":0.33,
            "usage":{"input_tokens":714,"output_tokens":7,
                "cache_read_input_tokens":16681,"cache_creation_input_tokens":5571},
            "modelUsage":{"claude-opus-4-8":{"contextWindow":200000}}}).to_string();
        match &decode_line(&ok)[0] {
            ThreadEvent::TurnEnded { result, usage, is_error } => {
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
    fn decodes_post_turn_summary() {
        let l = json!({"type":"system","subtype":"post_turn_summary",
            "status_category":"review_ready","status_detail":"appended 'line four' to notes.txt"}).to_string();
        assert_eq!(decode_line(&l), vec![ThreadEvent::TurnSummary {
            detail: "appended 'line four' to notes.txt".into(), category: "review_ready".into() }]);
    }
}
