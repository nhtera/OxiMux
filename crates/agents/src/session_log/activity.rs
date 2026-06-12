//! Live-activity extraction from a session log tail.
//!
//! For a Running agent the UI wants one dim line: what tool is it on right
//! now ("Bash: cargo test…"). The CLI journals every assistant message —
//! including `tool_use` content blocks with the tool name + input — so the
//! newest such block in the log tail IS the current activity. Pure parsing
//! here; the caller does the (bounded) file read on a background executor.

use std::path::Path;

use serde_json::Value;

use super::{parse_timestamp_ms, read_tail};

/// How much of the log tail to scan. Tool-use entries are small; 64 KiB
/// comfortably covers the last few messages even with large tool results
/// interleaved.
pub const TAIL_BYTES: u64 = 64 * 1024;

/// Ignore activity older than this — a stale tool line on a row that has
/// long moved on (or an old log matched by mtime) is worse than no line.
pub const FRESH_WITHIN_MS: i64 = 5 * 60 * 1000;

/// One in-flight tool call: tool name + a one-line input preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentActivity {
    pub tool: String,
    pub preview: String,
}

impl AgentActivity {
    /// Row label: `"Bash: cargo test…"` / `"Read: src/main.rs"`. The
    /// preview is already truncated by the parser.
    pub fn label(&self) -> String {
        if self.preview.is_empty() {
            self.tool.clone()
        } else {
            format!("{}: {}", self.tool, self.preview)
        }
    }
}

/// Max preview length in characters (ellipsis appended past it).
const PREVIEW_MAX_CHARS: usize = 56;

/// Input keys most likely to summarize a tool call, in preference order.
/// Falls back to the first string value when none match.
const PREVIEW_KEYS: &[&str] = &[
    "command",
    "file_path",
    "path",
    "pattern",
    "query",
    "url",
    "description",
    "prompt",
];

/// Read the current activity from a session log: bounded tail read + scan
/// newest-first for an assistant `tool_use` entry fresher than
/// [`FRESH_WITHIN_MS`]. Does file IO — background executor only.
pub fn read_current_activity(log_path: &Path, now_ms: i64) -> Option<AgentActivity> {
    let tail = read_tail(log_path, TAIL_BYTES)?;
    parse_activity_from_tail(&tail, now_ms)
}

/// Pure tail scan. Lines are parsed newest-first; the first parseable
/// assistant entry holding a `tool_use` block wins. Unparseable lines
/// (including the likely-truncated first line of the tail) are skipped.
pub fn parse_activity_from_tail(tail: &str, now_ms: i64) -> Option<AgentActivity> {
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Freshness gate. Entries without a parseable timestamp are
        // treated as fresh (format drift hides the line elsewhere, not
        // here — better one possibly-stale label than none forever).
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str)
            && let Some(ms) = parse_timestamp_ms(ts)
            && now_ms.saturating_sub(ms) > FRESH_WITHIN_MS
        {
            return None;
        }
        let content = v.pointer("/message/content").and_then(Value::as_array)?;
        for block in content.iter().rev() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let tool = block.get("name").and_then(Value::as_str)?.to_string();
            let preview = block
                .get("input")
                .map(preview_for_input)
                .unwrap_or_default();
            return Some(AgentActivity { tool, preview });
        }
        // Newest assistant entry is text-only (the agent is writing prose,
        // not running a tool) — no activity line beats a stale one.
        return None;
    }
    None
}

/// One-line preview of a tool-use `input` object: preferred key first,
/// else the first string value, flattened + truncated.
fn preview_for_input(input: &Value) -> String {
    let Some(map) = input.as_object() else {
        return String::new();
    };
    let picked = PREVIEW_KEYS
        .iter()
        .find_map(|k| map.get(*k).and_then(Value::as_str))
        .or_else(|| map.values().find_map(Value::as_str));
    match picked {
        Some(s) => flatten_truncate(s),
        None => String::new(),
    }
}

fn flatten_truncate(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= PREVIEW_MAX_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1781222400000; // 2026-06-12T00:00:00Z

    fn assistant_line(ts: &str, content: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"role":"assistant","content":[{content}]}}}}"#
        )
    }

    #[test]
    fn newest_tool_use_wins() {
        let old = assistant_line(
            "2026-06-11T23:59:00Z",
            r#"{"type":"tool_use","name":"Read","input":{"file_path":"a.rs"}}"#,
        );
        let new = assistant_line(
            "2026-06-11T23:59:50Z",
            r#"{"type":"tool_use","name":"Bash","input":{"command":"cargo test -p oximux-agents"}}"#,
        );
        let tail = format!("{old}\n{new}\n");
        let a = parse_activity_from_tail(&tail, NOW).unwrap();
        assert_eq!(a.tool, "Bash");
        assert_eq!(a.label(), "Bash: cargo test -p oximux-agents");
    }

    #[test]
    fn text_only_newest_assistant_yields_none() {
        let line = assistant_line(
            "2026-06-11T23:59:50Z",
            r#"{"type":"text","text":"Here is what I found."}"#,
        );
        assert!(parse_activity_from_tail(&line, NOW).is_none());
    }

    #[test]
    fn stale_entries_are_ignored() {
        let line = assistant_line(
            "2026-06-11T20:00:00Z", // 4 h before NOW
            r#"{"type":"tool_use","name":"Bash","input":{"command":"ls"}}"#,
        );
        assert!(parse_activity_from_tail(&line, NOW).is_none());
    }

    #[test]
    fn non_assistant_and_garbage_lines_are_skipped() {
        let user = r#"{"type":"user","message":{"content":"do it"}}"#;
        let garbage = r#"{"type":"assist"#; // truncated first tail line
        let tool = assistant_line(
            "2026-06-11T23:59:50Z",
            r#"{"type":"tool_use","name":"Grep","input":{"pattern":"fn main"}}"#,
        );
        let tail = format!("{garbage}\n{tool}\n{user}\n");
        let a = parse_activity_from_tail(&tail, NOW).unwrap();
        assert_eq!(a.tool, "Grep");
        assert_eq!(a.preview, "fn main");
    }

    #[test]
    fn preview_prefers_command_over_other_keys() {
        let line = assistant_line(
            "2026-06-11T23:59:50Z",
            r#"{"type":"tool_use","name":"Bash","input":{"description":"List files","command":"ls -la"}}"#,
        );
        let a = parse_activity_from_tail(&line, NOW).unwrap();
        assert_eq!(a.preview, "ls -la");
    }

    #[test]
    fn preview_falls_back_to_first_string_value() {
        let line = assistant_line(
            "2026-06-11T23:59:50Z",
            r#"{"type":"tool_use","name":"Custom","input":{"count":3,"target":"src/lib.rs"}}"#,
        );
        let a = parse_activity_from_tail(&line, NOW).unwrap();
        assert_eq!(a.preview, "src/lib.rs");
    }

    #[test]
    fn preview_flattens_newlines_and_truncates() {
        let long = "x".repeat(200);
        let content = format!(
            r#"{{"type":"tool_use","name":"Write","input":{{"command":"a\nb\t{long}"}}}}"#
        );
        let line = assistant_line("2026-06-11T23:59:50Z", &content);
        let a = parse_activity_from_tail(&line, NOW).unwrap();
        assert!(!a.preview.contains('\n'));
        assert!(a.preview.ends_with('…'));
        assert!(a.preview.chars().count() <= PREVIEW_MAX_CHARS + 1);
    }

    #[test]
    fn missing_timestamp_is_treated_as_fresh() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"pwd"}}]}}"#;
        let a = parse_activity_from_tail(line, NOW).unwrap();
        assert_eq!(a.label(), "Bash: pwd");
    }

    #[test]
    fn label_without_preview_is_tool_name_only() {
        let a = AgentActivity {
            tool: "Bash".into(),
            preview: String::new(),
        };
        assert_eq!(a.label(), "Bash");
    }

    #[test]
    fn empty_tail_is_none() {
        assert!(parse_activity_from_tail("", NOW).is_none());
    }
}
