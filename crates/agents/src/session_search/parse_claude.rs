//! Claude transcript lines → turns + session metadata.
//!
//! One `.jsonl` line at a time, so the indexer can stream a log from any
//! byte offset. Unknown record types and malformed lines yield nothing —
//! format drift costs rows, never a crash.

use serde_json::Value;

use super::turn::{MetaDelta, Role, Turn, tool_call_text};
use crate::session_log::parse_timestamp_ms;
use crate::session_log::session_index::{line_value, truncate_prompt, unwrap_command_xml};

pub fn parse_line(line: &str, meta: &mut MetaDelta, out: &mut Vec<Turn>) {
    let Some(v) = line_value(line) else { return };
    let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_timestamp_ms);
    meta.note_ts(ts);
    if meta.cwd.is_none() {
        meta.cwd = non_empty(&v, "cwd");
    }
    if meta.branch.is_none() {
        meta.branch = non_empty(&v, "gitBranch");
    }
    if let Some(t) = non_empty(&v, "customTitle") {
        meta.custom_title = Some(truncate_prompt(&t));
    }
    if let Some(t) = non_empty(&v, "aiTitle") {
        meta.ai_title = Some(truncate_prompt(&t));
    }
    match v.get("type").and_then(Value::as_str) {
        Some("last-prompt") if meta.last_prompt.is_none() => {
            meta.last_prompt = non_empty(&v, "lastPrompt").map(|p| truncate_prompt(&p)).filter(|p| !p.is_empty());
        }
        Some("user") => user_record(&v, ts, meta, out),
        Some("assistant") => assistant_record(&v, ts, meta, out),
        _ => {}
    }
}

fn user_record(v: &Value, ts: Option<i64>, meta: &mut MetaDelta, out: &mut Vec<Turn>) {
    // Injected context and compaction summaries restate other turns.
    if v.get("isMeta").and_then(Value::as_bool) == Some(true)
        || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    let Some(content) = v.pointer("/message/content") else { return };
    let mut texts = Vec::new();
    let mut tool_only = true;
    if let Some(s) = content.as_str() {
        texts.push(s.to_string());
        tool_only = false;
    } else if let Some(blocks) = content.as_array() {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        texts.push(t.to_string());
                    }
                    tool_only = false;
                }
                Some("tool_result") => {
                    out.push(Turn { role: Role::Tool, text: tool_result_text(b.get("content")), ts_ms: ts });
                }
                _ => {}
            }
        }
    }
    if tool_only {
        return;
    }
    meta.message_count += 1;
    for raw in texts {
        // Harness-injected records (`<task-notification>`, `<bash-stdout>`, …)
        // arrive as user text; they are searchable but are not what the user
        // said. Slash commands are XML too, and are the user's.
        let injected =
            raw.trim_start().starts_with('<') && crate::command_envelope::parse_slash_command(&raw).is_none();
        let text = unwrap_command_xml(&raw);
        if text.trim().is_empty() {
            continue;
        }
        if injected {
            out.push(Turn { role: Role::Tool, text, ts_ms: ts });
            continue;
        }
        if meta.first_prompt.is_none() {
            meta.first_prompt = Some(truncate_prompt(&text)).filter(|p| !p.is_empty());
        }
        out.push(Turn { role: Role::User, text, ts_ms: ts });
    }
}

fn assistant_record(v: &Value, ts: Option<i64>, meta: &mut MetaDelta, out: &mut Vec<Turn>) {
    meta.message_count += 1;
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else { return };
    for b in blocks {
        let turn = match b.get("type").and_then(Value::as_str) {
            Some("text") => b.get("text").and_then(Value::as_str).map(|t| (Role::Assistant, t.to_string())),
            Some("thinking") => b.get("thinking").and_then(Value::as_str).map(|t| (Role::Assistant, t.to_string())),
            Some("tool_use") => {
                let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                Some((Role::Tool, tool_call_text(name, b.get("input").unwrap_or(&Value::Null))))
            }
            _ => None,
        };
        if let Some((role, text)) = turn {
            out.push(Turn { role, text, ts_ms: ts });
        }
    }
}

/// A `tool_result` body: a bare string or an array of `text` blocks.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn non_empty(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}
