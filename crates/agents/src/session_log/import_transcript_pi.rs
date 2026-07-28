//! Map a Pi rollout (`~/.pi/agent/sessions/<cwd-slug>/<ts>_<uuid>.jsonl`) into a
//! chat transcript (`Vec<ThreadEntry>`), the same shape the Claude/Codex
//! importers build so a session started outside OxiMux renders with the same
//! fidelity once reopened.
//!
//! Pi's rollout is line-delimited JSON. Only `{"type":"message",...}` lines are
//! conversation; `session` (header), `model_change`, and `thinking_level_change`
//! are engine bookkeeping and skipped. A message's `content` is a bare string or
//! an array of blocks: `text` renders as the bubble body, `thinking` folds into
//! the assistant entry's `thinking` field (Pi carries no tool-call block on this
//! CLI version, but any block type this importer doesn't recognize degrades to a
//! plain notice row rather than ever rendering its raw JSON, so a future Pi
//! release that adds tool calls degrades safely instead of leaking).
//!
//! Unlike [`super::import_provider_index::collect_pi`] / its preview (which
//! read only head/tail windows for the fast list-scan), this reads the whole
//! file — reopening a specific session is a one-shot action, not a background
//! scan, so the cost is proportional to that one file, not every session in
//! the picker.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::session_index::line_value;
use crate::thread::{AssistantMessage, ThreadEntry, MAX_IMPORT_ENTRIES};

/// Locate the rollout file for a Pi session id under `~/.pi/agent/sessions`.
/// Pi embeds the id in the filename (`<ts>_<uuid>.jsonl`), so this reuses the
/// index scan's bounded walk and matches on the id substring. Run on a
/// background thread by callers — it stats every session file once. `None`
/// when no file matches (a pruned or foreign session).
pub fn locate_pi_session(home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    super::import_provider_index::collect_pi_files(&home.join(".pi/agent/sessions"), 0, &mut files);
    files.into_iter().find(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(session_id))
    })
}

/// Build the transcript for one Pi rollout file. IO errors (missing file,
/// permissions) degrade to an empty transcript — the resume still works with
/// no pre-rendered history.
pub fn pi_transcript(path: &Path) -> Vec<ThreadEntry> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    transcript_from_pi_str(&raw)
}

/// Pure core: map the rollout's newline-delimited body into a transcript.
fn transcript_from_pi_str(raw: &str) -> Vec<ThreadEntry> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(v) = line_value(line) else {
            // Torn/foreign line — skip, keep importing the rest.
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            // session / model_change / thinking_level_change → plumbing.
            continue;
        }
        if let Some(entry) = import_message(&v) {
            entries.push(entry);
        }
    }
    cap_entries(entries)
}

/// One `message` record → a `User`/`Assistant` entry, or `None` for an
/// unrecognized role / empty body.
fn import_message(v: &Value) -> Option<ThreadEntry> {
    let msg = v.get("message")?;
    let role = msg.get("role").and_then(Value::as_str)?;
    let content = msg.get("content")?;
    let (text, thinking) = content_text(content);
    match role {
        "user" if !text.trim().is_empty() => {
            Some(ThreadEntry::User { text, images: Vec::new(), checkpoint: None })
        }
        "assistant" if !text.trim().is_empty() || !thinking.trim().is_empty() => {
            Some(ThreadEntry::Assistant(AssistantMessage { text, thinking }))
        }
        // A recognized role with nothing renderable, or an unrecognized role
        // (e.g. a future "system"/"tool" turn) — nothing to show, not an error.
        _ => None,
    }
}

/// Join a message's content into `(text, thinking)`. A bare string is the
/// whole body's text; an array of blocks folds `text` blocks into `text` and
/// `thinking` blocks into `thinking` — any other block type is dropped rather
/// than rendered raw (there's no plain-notice slot inside a single bubble, and
/// Pi hasn't been observed emitting a tool-call block on this CLI version).
fn content_text(content: &Value) -> (String, String) {
    if let Some(s) = content.as_str() {
        return (s.to_string(), String::new());
    }
    let Some(blocks) = content.as_array() else {
        return (String::new(), String::new());
    };
    let mut text = String::new();
    let mut thinking = String::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    append_line(&mut text, t);
                }
            }
            Some("thinking") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    append_line(&mut thinking, t);
                }
            }
            _ => {}
        }
    }
    (text, thinking)
}

fn append_line(field: &mut String, t: &str) {
    if !field.is_empty() {
        field.push('\n');
    }
    field.push_str(t);
}

/// Keep only the last [`MAX_IMPORT_ENTRIES`], prefixing an elision divider when
/// anything was dropped (shared cap with the Claude/Codex/OpenCode importers).
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

#[cfg(test)]
mod tests;
