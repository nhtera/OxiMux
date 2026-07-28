//! Import a Claude CLI session log (`~/.claude/projects/<slug>/<sid>.jsonl`)
//! into a chat transcript (`Vec<ThreadEntry>`), so a session started outside
//! OxiMux (e.g. in the terminal `claude`) can be reopened and rendered in the
//! chat view, then continued live with `--resume`.
//!
//! The session file is a superset of the `-p --output-format stream-json`
//! wire the live decoder handles: same `assistant`/`user` message shapes, plus
//! journaling lines (`attachment`, `queue-operation`, `mode`, `last-prompt`,
//! `summary`, `system`) that we skip. Rather than fold events through the live
//! state machine (which has no "user message" event and relies on turn
//! boundaries the file doesn't mark), this builds entries directly:
//!
//! - real user lines → [`ThreadEntry::User`] (text + inline base64 images),
//! - assistant lines → one [`ThreadEntry::Assistant`] (text + thinking) plus a
//!   [`ThreadEntry::ToolCall`] per `tool_use` block,
//! - `tool_result` carriers → settle the matching tool call by id (an
//!   id→index map makes this order-independent, tolerating parallel tools),
//! - compaction summaries → a [`ThreadEntry::ContextCompaction`] divider.
//!
//! Everything degrades to "skip": unparseable lines, unknown types, missing
//! fields, and schema drift across CLI versions never panic — they just yield a
//! smaller transcript. Subagent (`isSidechain`) lines are omitted (they belong
//! to a nested transcript, not this one).

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::entry::{AssistantMessage, ChatImage, ThreadEntry};
use super::tool_call::{ToolCall, ToolCallStatus, flatten_tool_result_content};
use crate::command_envelope;

/// Cap on imported entries. A very long session renders the most recent slice;
/// older history is elided behind a single divider so the transcript stays
/// cheap to paint and scroll. `--resume` still carries the full server-side
/// history — only the local render is capped.
///
/// Note: when a session is capped, the displayed user messages are a suffix of
/// the full log, so a later *rewind* targets a UI ordinal that no longer lines
/// up with the file's user-line ordinal. That path fails safe — the fork's
/// expected-text guard rejects the mismatch and disables the rewind rather than
/// cutting at the wrong point — so it's a graceful limitation, not corruption.
pub const MAX_IMPORT_ENTRIES: usize = 500;

/// One-line prompt/summary preview cap (characters).
const LABEL_MAX_CHARS: usize = 160;

/// Read and import a session log file. IO errors propagate; a well-formed-but-
/// empty or all-noise file yields an empty transcript.
pub fn transcript_from_jsonl(path: &Path) -> std::io::Result<Vec<ThreadEntry>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(transcript_from_str(&raw))
}

/// Pure core: map the newline-delimited session body into a transcript.
pub fn transcript_from_str(raw: &str) -> Vec<ThreadEntry> {
    let mut entries: Vec<ThreadEntry> = Vec::new();
    // tool_use_id → index of its ToolCall entry, so a later (or out-of-order)
    // tool_result settles the right card regardless of file order.
    let mut tool_index: HashMap<String, usize> = HashMap::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            // Corrupt/torn line — skip, keep importing the rest.
            continue;
        };
        // Subagent transcript lines are a separate conversation.
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("summary") => {
                if let Some(s) = v.get("summary").and_then(Value::as_str) {
                    push_compaction(&mut entries, s);
                }
            }
            Some("user") => import_user(&v, &mut entries, &tool_index),
            Some("assistant") => import_assistant(&v, &mut entries, &mut tool_index),
            // attachment / queue-operation / mode / last-prompt / system / tag → noise
            _ => {}
        }
    }

    // A tool_use with no recorded result (session interrupted mid-tool at EOF)
    // renders as Canceled rather than a perpetual spinner.
    for entry in &mut entries {
        if let ThreadEntry::ToolCall(tc) = entry
            && tc.status == ToolCallStatus::InProgress
        {
            tc.status = ToolCallStatus::Canceled;
        }
    }

    cap_entries(entries)
}

/// A real user prompt (string or non-tool_result blocks) → a `User` entry; a
/// `tool_result` carrier → settle the matching tool call; a compaction summary
/// → a divider. Slash-command envelopes are unwrapped to `"/name args"` and
/// machine scaffolding (`<system-reminder>`, `<local-command-*>`) is stripped so
/// the bubble shows what the user said, not the plumbing.
fn import_user(v: &Value, entries: &mut Vec<ThreadEntry>, tool_index: &HashMap<String, usize>) {
    // The injected post-compaction summary arrives as a user turn — checked
    // before the meta skip so its divider survives even if it's flagged meta.
    if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        let text = user_text(v);
        let label = if text.trim().is_empty() { "Context compacted" } else { text.trim() };
        push_compaction(entries, label);
        return;
    }
    // Injected/meta turns (IDE context, slash-command skill expansions,
    // reminders) are plumbing the CLI records but never a real prompt — drop
    // them rather than render them as user messages.
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return;
    }
    match v.pointer("/message/content") {
        Some(Value::String(s)) => {
            let text = command_envelope::normalize_user_text(s);
            if !text.is_empty() {
                entries.push(ThreadEntry::User { text, images: Vec::new(), checkpoint: None });
            }
        }
        Some(Value::Array(blocks)) => {
            let has_tool_result = blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
            if has_tool_result {
                // The structured result sits at the line's top level (camelCase
                // in the file format), paired with the tool_result block(s).
                let structured = v.get("toolUseResult");
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        settle_tool_result(entries, tool_index, b, structured);
                    }
                }
            } else {
                let (raw, images) = text_and_images(blocks);
                let text = command_envelope::normalize_user_text(&raw);
                if !text.is_empty() || !images.is_empty() {
                    entries.push(ThreadEntry::User { text, images, checkpoint: None });
                }
            }
        }
        _ => {}
    }
}

/// Attach a `tool_result` block to its `ToolCall` entry (found by id), moving it
/// to `Completed`/`Failed` and recording the structured result. An unmatched id
/// is dropped (nothing to settle).
fn settle_tool_result(
    entries: &mut [ThreadEntry],
    tool_index: &HashMap<String, usize>,
    block: &Value,
    structured: Option<&Value>,
) {
    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    let Some(&idx) = tool_index.get(id) else {
        return;
    };
    if let Some(ThreadEntry::ToolCall(tc)) = entries.get_mut(idx) {
        let content = flatten_tool_result_content(block.get("content"));
        let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        tc.result = Some(content.clone());
        tc.structured = structured.cloned();
        tc.status = if is_error {
            ToolCallStatus::Failed(content)
        } else {
            ToolCallStatus::Completed
        };
    }
}

/// An assistant line → one `Assistant` (text + thinking concatenated) followed
/// by a `ToolCall` per `tool_use` block, registered in `tool_index`.
fn import_assistant(
    v: &Value,
    entries: &mut Vec<ThreadEntry>,
    tool_index: &mut HashMap<String, usize>,
) {
    let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    let mut msg = AssistantMessage::default();
    let mut tools: Vec<ToolCall> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str)
                    && !t.is_empty()
                {
                    append_block(&mut msg.text, t);
                }
            }
            Some("thinking") => {
                if let Some(t) = b.get("thinking").and_then(Value::as_str)
                    && !t.is_empty()
                {
                    append_block(&mut msg.thinking, t);
                }
            }
            Some("tool_use") => {
                let id = b.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let name = b.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                tools.push(ToolCall::new(id, name, input));
            }
            _ => {}
        }
    }
    if !msg.is_empty() {
        entries.push(ThreadEntry::Assistant(msg));
    }
    for tc in tools {
        let id = tc.id.clone();
        entries.push(ThreadEntry::ToolCall(tc));
        if !id.is_empty() {
            tool_index.insert(id, entries.len() - 1);
        }
    }
}

/// Extract text + inline base64 images from a user message's content blocks.
fn text_and_images(blocks: &[Value]) -> (String, Vec<ChatImage>) {
    let mut text_parts: Vec<&str> = Vec::new();
    let mut images: Vec<ChatImage> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text_parts.push(t);
                }
            }
            Some("image") => {
                let src = b.get("source");
                let is_b64 =
                    src.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("base64");
                if is_b64
                    && let (Some(media_type), Some(data)) = (
                        src.and_then(|s| s.get("media_type")).and_then(Value::as_str),
                        src.and_then(|s| s.get("data")).and_then(Value::as_str),
                    )
                {
                    images.push(ChatImage {
                        media_type: media_type.to_string(),
                        data: data.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    (text_parts.join("\n"), images)
}

/// Push a compaction/truncation divider, its label trimmed to one capped line.
fn push_compaction(entries: &mut Vec<ThreadEntry>, summary: &str) {
    entries.push(ThreadEntry::ContextCompaction { summary: one_line(summary) });
}

/// Keep only the last [`MAX_IMPORT_ENTRIES`], prefixing an elision divider when
/// anything was dropped.
fn cap_entries(mut entries: Vec<ThreadEntry>) -> Vec<ThreadEntry> {
    if entries.len() <= MAX_IMPORT_ENTRIES {
        return entries;
    }
    let dropped = entries.len() - MAX_IMPORT_ENTRIES;
    let tail = entries.split_off(dropped);
    let mut out = Vec::with_capacity(tail.len() + 1);
    out.push(ThreadEntry::ContextCompaction {
        summary: format!("{dropped} earlier messages not shown"),
    });
    out.extend(tail);
    out
}

/// The text of a user line's `message.content` (string, or the joined `text`
/// blocks of an array).
fn user_text(v: &Value) -> String {
    match v.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Append a further finalized block to an accumulating field, newline-joined.
fn append_block(field: &mut String, t: &str) {
    if !field.is_empty() {
        field.push('\n');
    }
    field.push_str(t);
}

/// The suffix of a folded transcript that a chat already knowing
/// `known_user_turns` user prompts has NOT seen: everything from the
/// (known+1)-th user prompt onward.
///
/// This is how turns run in a companion terminal reach the chat view — the
/// terminal resumes the same session id interactively, so its turns land only
/// in the CLI's session log, never on the chat's connection. The chat re-folds
/// the log and appends this tail.
///
/// Fails safe in both directions: a log with no unseen prompts returns empty
/// (nothing to append, never a duplicate), and a log whose fold was capped
/// ([`MAX_IMPORT_ENTRIES`] elides oldest history, shrinking its user count
/// below `known_user_turns`) also returns empty rather than misaligning.
pub fn tail_beyond_known_turns(
    folded: Vec<ThreadEntry>,
    known_user_turns: usize,
) -> Vec<ThreadEntry> {
    let mut seen = 0usize;
    for (i, e) in folded.iter().enumerate() {
        if matches!(e, ThreadEntry::User { .. }) {
            seen += 1;
            if seen > known_user_turns {
                return folded[i..].to_vec();
            }
        }
    }
    Vec::new()
}

/// Collapse to a single capped line for a divider label.
fn one_line(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > LABEL_MAX_CHARS {
        let mut out: String = flat.chars().take(LABEL_MAX_CHARS).collect();
        out.push('…');
        out
    } else {
        flat
    }
}

#[cfg(test)]
mod tests;
