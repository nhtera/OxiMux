//! Opening-exchange extraction for the session-history preview pane.
//!
//! Mirrors Claude Code's `/resume` preview (its `SessionPreview` renders the
//! transcript): given a session log path, read a bounded head and pull out the
//! first handful of user/assistant **text** messages so the picker can show
//! what a session was about before resuming it. Tool-call/tool-result and
//! meta/compact-summary turns are skipped — they carry no readable prose.
//!
//! Bounded + best-effort: a giant transcript costs one head read, malformed
//! lines are skipped, and an unreadable file yields an empty preview (never a
//! crash). Both formats are handled: Claude's `type: user`/`assistant` lines
//! and Codex rollout `response_item` message payloads (`~/.codex/sessions/**/
//! rollout-*.jsonl`), with Codex's injected `AGENTS.md`/context turns skipped.

use std::path::Path;

use serde_json::Value;

use super::session_index::{line_value, read_head, unwrap_command_xml, user_message_text};

/// Who authored a previewed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRole {
    User,
    Assistant,
}

/// One previewed turn: a role + its (capped, possibly multi-line) text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewMessage {
    pub role: PreviewRole,
    pub text: String,
}

/// Head window read for a preview. Generous on purpose: the opening user turn
/// is often followed by a large `file-history-snapshot` and several
/// `attachment` lines, and the first assistant reply sits *after* them — a
/// tight window would miss it. Small logs are read whole; a multi-hundred-MB
/// transcript still costs only this bounded head.
const PREVIEW_HEAD_BYTES: u64 = 512 * 1024;
/// Per-message character cap (ellipsis appended past it); keeps newlines.
const PREVIEW_MSG_CHARS: usize = 700;

/// Read the head of a Claude `.jsonl` session log and extract up to
/// `max_messages` opening user/assistant text turns for the preview pane.
pub fn load_session_preview(path: &Path, max_messages: usize) -> Vec<PreviewMessage> {
    let head = read_head(path, PREVIEW_HEAD_BYTES).unwrap_or_default();
    preview_from_content(&head, max_messages)
}

/// Pure core of [`load_session_preview`] — split out so it's testable over a
/// literal transcript body.
pub fn preview_from_content(content: &str, max_messages: usize) -> Vec<PreviewMessage> {
    let mut out: Vec<PreviewMessage> = Vec::new();
    for line in content.lines() {
        if out.len() >= max_messages {
            break;
        }
        let Some(v) = line_value(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                if is_skippable_user(&v) {
                    continue;
                }
                if let Some(text) = user_message_text(&v) {
                    let cleaned = unwrap_command_xml(&text);
                    if !cleaned.trim().is_empty() {
                        out.push(PreviewMessage {
                            role: PreviewRole::User,
                            text: truncate_preview(&cleaned),
                        });
                    }
                }
            }
            Some("assistant") => {
                if let Some(text) = assistant_message_text(&v)
                    && !text.trim().is_empty()
                {
                    out.push(PreviewMessage {
                        role: PreviewRole::Assistant,
                        text: truncate_preview(&text),
                    });
                }
            }
            // Codex rollout turns: newer rollouts wrap them as
            // `response_item.payload`; older ones emit top-level `message`.
            Some("response_item") => {
                if let Some(msg) = v.get("payload").and_then(codex_preview_message) {
                    out.push(msg);
                }
            }
            Some("message") => {
                if let Some(msg) = codex_preview_message(&v) {
                    out.push(msg);
                }
            }
            _ => {}
        }
    }
    out
}

/// Extract a previewable turn from a Codex rollout message object (a
/// `response_item.payload` or an older top-level `message`). User turns carry
/// `input_text` blocks, assistant turns `output_text`; `developer` and other
/// roles are plumbing, skipped. Injected synthetic-user context (the project
/// `AGENTS.md` and `<…>`-wrapped blocks) is skipped so the preview opens on the
/// human's actual first line, matching the row title.
fn codex_preview_message(msg: &Value) -> Option<PreviewMessage> {
    // Guard the message type when present (older top-level lines carry the
    // outer `message` type; newer payloads carry an inner `message`).
    if let Some(t) = msg.get("type").and_then(Value::as_str)
        && t != "message"
    {
        return None;
    }
    let role = msg.get("role").and_then(Value::as_str)?;
    let (block_type, preview_role) = match role {
        "user" => ("input_text", PreviewRole::User),
        "assistant" => ("output_text", PreviewRole::Assistant),
        _ => return None,
    };
    let text = msg
        .get("content")?
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some(block_type))
        .find_map(|b| b.get("text").and_then(Value::as_str))?;
    if preview_role == PreviewRole::User && is_codex_injected_prompt(text) {
        return None;
    }
    if text.trim().is_empty() {
        return None;
    }
    Some(PreviewMessage {
        role: preview_role,
        text: truncate_preview(text),
    })
}

/// Codex prepends context blocks as synthetic user turns — `<…>`-tag-wrapped or
/// the `AGENTS.md` instructions. They're noise in a transcript preview.
fn is_codex_injected_prompt(text: &str) -> bool {
    let s = text.trim_start();
    s.starts_with('<') || s.starts_with("# AGENTS.md")
}

/// User turns that carry no readable prose: meta/compact-summary markers and
/// tool-result echoes (the array form with a `tool_result` block).
fn is_skippable_user(v: &Value) -> bool {
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if let Some(arr) = v.pointer("/message/content").and_then(Value::as_array)
        && arr
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return true;
    }
    false
}

/// Assistant `message.content` as text: a bare string, or every `text` block
/// of a content-block array joined with newlines (tool_use blocks dropped).
fn assistant_message_text(v: &Value) -> Option<String> {
    let content = v.pointer("/message/content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    let parts: Vec<String> = arr
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Trim + cap a message body to [`PREVIEW_MSG_CHARS`] chars, preserving
/// newlines (the pane wraps); appends an ellipsis when cut.
fn truncate_preview(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= PREVIEW_MSG_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(PREVIEW_MSG_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_opening_user_and_assistant_text() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi there"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi! Ready to help."}]}}"#,
            "\n",
        );
        let msgs = preview_from_content(content, 8);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, PreviewRole::User);
        assert_eq!(msgs[0].text, "hi there");
        assert_eq!(msgs[1].role, PreviewRole::Assistant);
        assert_eq!(msgs[1].text, "Hi! Ready to help.");
    }

    #[test]
    fn unwraps_slash_command_user_turn() {
        let xml = "<command-message>cook</command-message><command-name>/cook</command-name><command-args>plan.md</command-args>";
        let content = format!(r#"{{"type":"user","message":{{"content":"{xml}"}}}}"#);
        let msgs = preview_from_content(&content, 8);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "/cook plan.md");
    }

    #[test]
    fn skips_meta_tool_result_and_tool_use() {
        let content = concat!(
            r#"{"type":"user","isMeta":true,"message":{"content":"<ide-context/>"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"the real first prompt"}}"#,
            "\n",
        );
        let msgs = preview_from_content(content, 8);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "the real first prompt");
    }

    #[test]
    fn respects_max_messages_cap() {
        let mut lines = String::new();
        for i in 0..20 {
            lines.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"msg {i}\"}}}}\n"
            ));
        }
        let msgs = preview_from_content(&lines, 4);
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn long_message_is_truncated_with_ellipsis() {
        let big = "x".repeat(PREVIEW_MSG_CHARS + 50);
        let content = format!(r#"{{"type":"user","message":{{"content":"{big}"}}}}"#);
        let msgs = preview_from_content(&content, 8);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text.ends_with('…'));
        assert_eq!(msgs[0].text.chars().count(), PREVIEW_MSG_CHARS + 1);
    }

    #[test]
    fn malformed_and_empty_never_panic() {
        assert!(preview_from_content("not json\n{bad", 8).is_empty());
        assert!(preview_from_content("", 8).is_empty());
    }

    #[test]
    fn extracts_codex_rollout_turns_skipping_injected_preamble() {
        let content = concat!(
            r#"{"type":"session_meta","payload":{"session_id":"cx","cwd":"/p"}}"#,
            "\n",
            // Injected AGENTS.md + environment_context synthetic-user turns.
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}"##,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context><cwd>/p</cwd></environment_context>"}]}}"#,
            "\n",
            // developer-role plumbing is skipped.
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions/>"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the parser"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"On it."}]}}"#,
            "\n",
        );
        let msgs = preview_from_content(content, 8);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, PreviewRole::User);
        assert_eq!(msgs[0].text, "fix the parser");
        assert_eq!(msgs[1].role, PreviewRole::Assistant);
        assert_eq!(msgs[1].text, "On it.");
    }
}
