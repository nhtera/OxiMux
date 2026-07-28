//! Import a Codex CLI session rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-
//! <ts>-<uuid>.jsonl`) into a chat transcript (`Vec<ThreadEntry>`), so a Codex
//! session started outside OxiMux (the terminal `codex`, VS Code, another
//! app-server client) can be reopened and rendered in the chat view, then
//! continued live via `thread/resume`.
//!
//! The rollout is the legacy Responses-API shape — distinct from the app-server
//! v2 `ThreadItem` notifications the live `codex/map.rs` decoder handles. Each
//! line is `{timestamp, type, payload}`:
//!
//! - `type: "session_meta"` → carries the session `cwd` (first line), which the
//!   reopen uses to spawn the resumed connection in the right directory.
//! - `type: "response_item"` → the transcript items, mapped by `payload.type`:
//!   - `message` role user → [`ThreadEntry::User`]; role assistant →
//!     [`ThreadEntry::Assistant`]; developer/system roles are plumbing, skipped;
//!   - `reasoning` with a text `summary` → assistant thinking (encrypted-only
//!     reasoning carries no plaintext, skipped);
//!   - `function_call` / `custom_tool_call` / `local_shell_call` →
//!     [`ThreadEntry::ToolCall`] (a `call_id`→index map settles the result
//!     regardless of order);
//!   - `*_output` carriers → settle the matching tool call by `call_id`.
//! - `event_msg` / `turn_context` / `world_state` → live telemetry, skipped.
//!
//! Everything degrades to "skip": unparseable lines, unknown types, missing
//! fields, and format drift across CLI versions never panic — they yield a
//! smaller transcript. When the rollout can't be read at all, the caller resumes
//! with an empty seed (`thread/resume` still works with just the thread id).

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::entry::{AssistantMessage, ThreadEntry};
use super::session_import::MAX_IMPORT_ENTRIES;
use super::tool_call::{ToolCall, ToolCallStatus};

/// The result of importing a Codex rollout: the rendered transcript plus the
/// session's recorded working directory (from `session_meta`), so the reopen
/// spawns the resumed connection in the same cwd. `cwd` is `None` when the
/// rollout omitted it (older format / torn head).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexRolloutImport {
    pub entries: Vec<ThreadEntry>,
    pub cwd: Option<PathBuf>,
}

/// Locate the rollout file for a Codex thread id under `<codex_dir>/sessions`.
/// The rollout filename embeds the id (`rollout-<ts>-<uuid>.jsonl`), so this is a
/// bounded directory walk matching on the id substring — run lazily on reopen
/// (a single click), never during the session-history list scan. `None` when no
/// file matches (a stale index entry whose rollout was pruned).
pub fn locate_rollout(codex_dir: &Path, thread_id: &str) -> Option<PathBuf> {
    let sessions = codex_dir.join("sessions");
    if thread_id.is_empty() {
        return None;
    }
    find_matching(&sessions, thread_id, 0)
}

/// Depth-bounded recursive search for a `*.jsonl` filename containing `needle`.
/// Codex nests rollouts three levels deep (`YYYY/MM/DD`), so a depth cap keeps a
/// pathological tree from unbounded recursion.
fn find_matching(dir: &Path, needle: &str, depth: usize) -> Option<PathBuf> {
    const MAX_DEPTH: usize = 5;
    if depth > MAX_DEPTH {
        return None;
    }
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            subdirs.push(path);
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.ends_with(".jsonl")
            && name.contains(needle)
        {
            return Some(path);
        }
    }
    // Recurse into subdirectories (newest first — Codex dates them, so a reverse
    // sort tends to hit recent sessions sooner).
    subdirs.sort();
    for sub in subdirs.into_iter().rev() {
        if let Some(found) = find_matching(&sub, needle, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Read and import a Codex rollout file. IO errors propagate; a well-formed-but-
/// empty or all-noise file yields an empty transcript (and whatever cwd the head
/// recorded).
pub fn import_codex_rollout(path: &Path) -> std::io::Result<CodexRolloutImport> {
    let raw = std::fs::read_to_string(path)?;
    Ok(transcript_from_codex_str(&raw))
}

/// Pure core: map the newline-delimited rollout body into a transcript + cwd.
pub fn transcript_from_codex_str(raw: &str) -> CodexRolloutImport {
    let mut entries: Vec<ThreadEntry> = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    // call_id → index of its ToolCall entry, so a later (or out-of-order) output
    // settles the right card regardless of file order.
    let mut tool_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                // Keep the FIRST recorded cwd (the head describes this session; a
                // forked-from record may follow).
                if cwd.is_none()
                    && let Some(c) = v.pointer("/payload/cwd").and_then(Value::as_str)
                    && !c.is_empty()
                {
                    cwd = Some(PathBuf::from(c));
                }
            }
            Some("response_item") => {
                if let Some(payload) = v.get("payload") {
                    import_item(payload, &mut entries, &mut tool_index);
                }
            }
            // event_msg / turn_context / world_state → live telemetry, not history.
            _ => {}
        }
    }

    // A tool call with no recorded output (session interrupted mid-tool at EOF)
    // renders as Canceled rather than a perpetual spinner.
    for entry in &mut entries {
        if let ThreadEntry::ToolCall(tc) = entry
            && tc.status == ToolCallStatus::InProgress
        {
            tc.status = ToolCallStatus::Canceled;
        }
    }

    CodexRolloutImport { entries: cap_entries(entries), cwd }
}

/// Map one `response_item` payload into transcript entries.
fn import_item(
    payload: &Value,
    entries: &mut Vec<ThreadEntry>,
    tool_index: &mut std::collections::HashMap<String, usize>,
) {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => import_message(payload, entries),
        Some("reasoning") => {
            if let Some(text) = reasoning_text(payload) {
                entries.push(ThreadEntry::Assistant(AssistantMessage {
                    thinking: text,
                    ..Default::default()
                }));
            }
        }
        // A tool invocation. `function_call.arguments` is a JSON string;
        // `custom_tool_call.input` / `local_shell_call` carry a raw string or an
        // object — parse leniently, else wrap the raw string as `{input}`.
        Some("function_call" | "custom_tool_call" | "local_shell_call") => {
            let call_id = str_field(payload, "call_id");
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("tool")
                .to_string();
            let input = tool_input(payload);
            entries.push(ThreadEntry::ToolCall(ToolCall::new(call_id.clone(), name, input)));
            if !call_id.is_empty() {
                tool_index.insert(call_id, entries.len() - 1);
            }
        }
        Some("function_call_output" | "custom_tool_call_output" | "local_shell_call_output") => {
            settle_output(payload, entries, tool_index);
        }
        _ => {}
    }
}

/// Context injections and abort markers that codex records with role `user`
/// even though the user never typed them (observed across real 0.14x rollouts).
/// They must not become chat bubbles — and, as importantly, must not count as
/// user TURNS: the companion-terminal sync anchors on the user-prompt count,
/// and a live-started thread (whose user entries come only from the composer)
/// would misalign against a fold that counted these.
const PLUMBING_USER_TAGS: [&str; 6] = [
    "<user_instructions>",
    "<environment_context>",
    "<turn_aborted>",
    "<subagent_notification>",
    "<recommended_plugins>",
    "<skill",
];

/// Whether a user-role message body is one of the known plumbing injections.
fn is_plumbing_user_text(text: &str) -> bool {
    let t = text.trim_start();
    PLUMBING_USER_TAGS.iter().any(|tag| t.starts_with(tag))
}

/// A `message` item → a user or assistant entry (developer/system are plumbing,
/// skipped). Content blocks are `input_text` (user) / `output_text` (assistant).
fn import_message(payload: &Value, entries: &mut Vec<ThreadEntry>) {
    let role = payload.get("role").and_then(Value::as_str).unwrap_or_default();
    let text = message_text(payload);
    if text.trim().is_empty() {
        return;
    }
    match role {
        "user" if is_plumbing_user_text(&text) => {}
        "user" => entries.push(ThreadEntry::User { text, images: Vec::new(), checkpoint: None }),
        "assistant" => {
            entries.push(ThreadEntry::Assistant(AssistantMessage { text, ..Default::default() }));
        }
        // developer / system / tool → context plumbing, never a chat bubble.
        _ => {}
    }
}

/// Join a message payload's text content blocks (`input_text` / `output_text` /
/// a bare `text` field or string content).
fn message_text(payload: &Value) -> String {
    match payload.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => {
                    b.get("text").and_then(Value::as_str)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The plaintext of a reasoning item: its `summary` text blocks joined.
/// Encrypted-only reasoning (no `summary`) yields `None` — the ciphertext is
/// never rendered.
fn reasoning_text(payload: &Value) -> Option<String> {
    let joined = payload
        .get("summary")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|b| {
            b.get("text")
                .and_then(Value::as_str)
                .or_else(|| b.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

/// Parse a tool item's arguments into an input `Value`: `function_call.arguments`
/// (a JSON string), `custom_tool_call.input` / `local_shell_call` (object or raw
/// string). A non-JSON string is wrapped as `{"input": "<raw>"}` so the card
/// still shows something.
fn tool_input(payload: &Value) -> Value {
    let raw = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"));
    match raw {
        Some(Value::String(s)) => {
            serde_json::from_str::<Value>(s).unwrap_or_else(|_| serde_json::json!({ "input": s }))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// Settle a tool output onto its `ToolCall` by `call_id`. The output is a string
/// (`function_call_output`) or an array of `{input_text}` blocks
/// (`custom_tool_call_output`). An unmatched id is dropped.
fn settle_output(
    payload: &Value,
    entries: &mut [ThreadEntry],
    tool_index: &std::collections::HashMap<String, usize>,
) {
    let Some(id) = payload.get("call_id").and_then(Value::as_str) else {
        return;
    };
    let Some(&idx) = tool_index.get(id) else {
        return;
    };
    let content = output_text(payload.get("output"));
    if let Some(ThreadEntry::ToolCall(tc)) = entries.get_mut(idx) {
        tc.result = Some(content);
        tc.status = ToolCallStatus::Completed;
    }
}

/// Flatten a tool output value to a string: a bare string, or the joined text of
/// an array of `{type: input_text, text}` blocks.
fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str).or_else(|| b.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Keep only the last [`MAX_IMPORT_ENTRIES`], prefixing an elision divider when
/// anything was dropped (shared cap with the Claude importer).
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

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests;
