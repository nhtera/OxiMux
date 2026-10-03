//! Codex rollout lines → turns + session metadata.
//!
//! Newer rollouts wrap each item as `{"type":"response_item","payload":{…}}`;
//! older ones put the item at the top level. Event echoes (`event_msg`) repeat
//! response items and are skipped, as are developer/system messages and the
//! injected context turns (`AGENTS.md`, `<environment_context>`, …).

use serde_json::Value;

use super::turn::{MetaDelta, Role, Turn, tool_call_text};
use crate::session_log::parse_timestamp_ms;
use crate::session_log::session_index::{line_value, truncate_prompt};

pub fn parse_line(line: &str, meta: &mut MetaDelta, out: &mut Vec<Turn>) {
    let Some(v) = line_value(line) else { return };
    let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_timestamp_ms);
    meta.note_ts(ts);
    match v.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            if let Some(p) = v.get("payload") {
                session_meta(p, meta);
            }
        }
        Some("response_item") => {
            if let Some(p) = v.get("payload") {
                item(p, ts, meta, out);
            }
        }
        Some("event_msg" | "turn_context" | "compacted") | None => {}
        // Older rollouts: the item itself is the line.
        Some(_) => item(&v, ts, meta, out),
    }
}

fn session_meta(p: &Value, meta: &mut MetaDelta) {
    let s = |key: &str| p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    if meta.session_id.is_none() {
        meta.session_id = s("session_id").or_else(|| s("id"));
    }
    if meta.cwd.is_none() {
        meta.cwd = s("cwd");
    }
    if meta.branch.is_none() {
        meta.branch = p.pointer("/git/branch").and_then(Value::as_str).filter(|b| !b.is_empty()).map(str::to_string);
    }
    let created = p.get("timestamp").and_then(Value::as_str).and_then(parse_timestamp_ms);
    meta.note_ts(created);
}

fn item(p: &Value, ts: Option<i64>, meta: &mut MetaDelta, out: &mut Vec<Turn>) {
    let mut push = |role, text: String| {
        if !text.trim().is_empty() {
            out.push(Turn { role, text, ts_ms: ts });
        }
    };
    match p.get("type").and_then(Value::as_str) {
        Some("message") => match p.get("role").and_then(Value::as_str) {
            Some("user") => {
                let text = blocks_text(p.get("content"), "input_text");
                if is_injected(&text) {
                    return;
                }
                meta.message_count += 1;
                if meta.first_prompt.is_none() {
                    meta.first_prompt = Some(truncate_prompt(&text)).filter(|t| !t.is_empty());
                }
                push(Role::User, text);
            }
            Some("assistant") => {
                meta.message_count += 1;
                push(Role::Assistant, blocks_text(p.get("content"), "output_text"));
            }
            _ => {}
        },
        Some("reasoning") => push(Role::Assistant, blocks_text(p.get("summary"), "summary_text")),
        Some("function_call") => {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
            let args = p
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .unwrap_or(Value::Null);
            push(Role::Tool, tool_call_text(name, &args));
        }
        Some("custom_tool_call") => {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
            let input = p.get("input").and_then(Value::as_str).unwrap_or_default();
            push(Role::Tool, format!("{name}: {input}"));
        }
        Some("function_call_output" | "custom_tool_call_output") => push(Role::Tool, output_text(p.get("output"))),
        Some("web_search_call") => {
            if let Some(q) = p.pointer("/action/query").and_then(Value::as_str) {
                push(Role::Tool, format!("web_search: {q}"));
            }
        }
        _ => {}
    }
}

/// Text of every `kind` block in a content array, newline-joined.
fn blocks_text(content: Option<&Value>, kind: &str) -> String {
    let Some(blocks) = content.and_then(Value::as_array) else { return String::new() };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A tool output: a string (sometimes a JSON envelope `{"output": …}`) or an
/// array of `input_text` blocks.
fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| v.get("output").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| s.clone()),
        Some(v @ Value::Array(_)) => blocks_text(Some(v), "input_text"),
        _ => String::new(),
    }
}

fn is_injected(text: &str) -> bool {
    let s = text.trim_start();
    s.starts_with('<') || s.starts_with("# AGENTS.md")
}
