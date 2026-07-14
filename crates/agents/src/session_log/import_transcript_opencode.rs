//! Map an OpenCode SQLite session (`~/.local/share/opencode/opencode.db`)
//! into a chat transcript (`Vec<ThreadEntry>`), the same shape the Claude/Codex
//! importers (`thread::session_import`, `thread::codex_session_import`) build so
//! a session started outside OxiMux can be rendered with the same fidelity.
//!
//! OpenCode's `message` rows carry the turn's role; the joined `part` rows carry
//! the turn's content, one row per block (`{"type":"text","text":…}`,
//! `{"type":"reasoning","text":…}`, `{"type":"tool",...}`, plus session plumbing
//! like `step-start`/`step-finish`/`patch`/`compaction` that this importer
//! ignores — they describe engine bookkeeping, not something a user typed or
//! read). A message's consecutive `text` parts fold into one bubble (mirrors
//! [`super::import_provider_index::load_import_provider_preview`]); `reasoning`
//! parts fold into the assistant entry's `thinking` field. A `tool` part never
//! renders its raw JSON — it becomes a single plain notice row (the part's
//! `title`, else its `tool` name, plus `state.status`), consistent with the
//! degrade-to-plain-row bar the Claude/Codex importers hold for anything they
//! can't map to a bubble.
//!
//! Read-only: SQLite is opened via [`super::import_provider_index::open_ro`]
//! (`SQLITE_OPEN_READ_ONLY`), never creating a `-wal`/`-shm` file or taking a
//! write lock even against a live OpenCode server's database. A missing store,
//! missing tables, or a malformed row degrades to an empty transcript — a
//! genuinely unreadable store (present but unqueryable) yields a single notice
//! row rather than nothing, so the reopen visibly explains the gap instead of
//! looking broken.

use std::path::Path;

use serde_json::Value;

use super::import_provider_index::open_ro;
use super::session_index::line_value;
use crate::thread::{AssistantMessage, ThreadEntry, MAX_IMPORT_ENTRIES};

/// Relative path (from `$HOME`) to OpenCode's session store.
const OPENCODE_DB_REL: &str = ".local/share/opencode/opencode.db";

/// Notice row shown when the store exists but couldn't be read (schema drift,
/// a torn row) — the session still resumes fine in the terminal, only the
/// pre-rendered history is unavailable.
const UNREADABLE_NOTICE: &str = "Transcript unavailable — resume to view full history.";

/// Build the transcript for one OpenCode session id. `home` is `$HOME`
/// (injected so tests can point at a temp dir). An absent database yields an
/// empty transcript (nothing indexed this session in the first place); a
/// present-but-unqueryable database yields the single notice row.
pub fn opencode_transcript(home: &Path, session_id: &str) -> Vec<ThreadEntry> {
    let db = home.join(OPENCODE_DB_REL);
    let Some(conn) = open_ro(&db) else {
        return Vec::new();
    };
    let sql = "SELECT m.id, m.data, p.data FROM message m \
               JOIN part p ON p.message_id = m.id \
               WHERE m.session_id = ?1 \
               ORDER BY m.time_created, p.time_created";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return vec![unreadable_notice()];
    };
    let rows = stmt.query_map([session_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    });
    let Ok(rows) = rows else {
        return vec![unreadable_notice()];
    };

    let mut entries = Vec::new();
    let mut acc = TurnAcc::default();
    for (mid, mdata, pdata) in rows.flatten() {
        let Some(mval) = line_value(&mdata) else { continue };
        let Some(role) = mval.get("role").and_then(Value::as_str) else { continue };
        if acc.mid.as_deref() != Some(mid.as_str()) {
            acc.flush(&mut entries);
            acc = TurnAcc { mid: Some(mid), role: role.to_string(), ..Default::default() };
        }
        let Some(pval) = line_value(&pdata) else { continue };
        match pval.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = pval.get("text").and_then(Value::as_str).filter(|t| !t.is_empty())
                {
                    acc.push_text(t);
                }
            }
            Some("reasoning") if role == "assistant" => {
                if let Some(t) = pval.get("text").and_then(Value::as_str).filter(|t| !t.is_empty())
                {
                    acc.push_thinking(t);
                }
            }
            // A tool part never renders its raw JSON — queued as a plain
            // notice that trails the turn's bubble (mirrors the Claude/Codex
            // importers, which emit the assistant's text first, then its tool
            // calls), so a mid-message tool doesn't fracture the reply text.
            Some("tool") => {
                acc.tool_notices.push(tool_notice(&pval));
            }
            // step-start / step-finish / patch / compaction / reasoning-on-a-
            // non-assistant-row — engine plumbing, never rendered.
            _ => {}
        }
    }
    acc.flush(&mut entries);
    cap_entries(entries)
}

/// Accumulates one message's parts — its `text`/`reasoning` blocks fold into
/// one bubble, its `tool` blocks queue as trailing notice rows — until the
/// message id changes or the stream ends, then emits them in turn order.
#[derive(Default)]
struct TurnAcc {
    mid: Option<String>,
    role: String,
    text: String,
    thinking: String,
    tool_notices: Vec<ThreadEntry>,
}

impl TurnAcc {
    fn push_text(&mut self, t: &str) {
        append_line(&mut self.text, t);
    }

    fn push_thinking(&mut self, t: &str) {
        append_line(&mut self.thinking, t);
    }

    /// Emit this turn's bubble (if it collected text/thinking) followed by any
    /// queued tool notices, then reset for the next message.
    fn flush(&mut self, entries: &mut Vec<ThreadEntry>) {
        match self.role.as_str() {
            "user" if !self.text.is_empty() => {
                entries.push(ThreadEntry::User {
                    text: std::mem::take(&mut self.text),
                    images: Vec::new(),
                    checkpoint: None,
                });
            }
            "assistant" if !self.text.is_empty() || !self.thinking.is_empty() => {
                entries.push(ThreadEntry::Assistant(AssistantMessage {
                    text: std::mem::take(&mut self.text),
                    thinking: std::mem::take(&mut self.thinking),
                }));
            }
            _ => {}
        }
        self.text.clear();
        self.thinking.clear();
        entries.append(&mut self.tool_notices);
    }
}

fn append_line(field: &mut String, t: &str) {
    if !field.is_empty() {
        field.push('\n');
    }
    field.push_str(t);
}

/// A `tool` part → a plain notice row. Only scalar label fields are read
/// (`title`/`tool`/`state.status`) — the tool's structured input/output is
/// never rendered, so this can't leak raw JSON.
fn tool_notice(part: &Value) -> ThreadEntry {
    let label = part
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| part.get("tool").and_then(Value::as_str).map(|t| format!("Tool: {t}")))
        .unwrap_or_else(|| "Tool call".to_string());
    let status = part.pointer("/state/status").and_then(Value::as_str);
    let summary = match status {
        Some(s) if !s.is_empty() => format!("{label} ({s})"),
        _ => label,
    };
    ThreadEntry::ContextCompaction { summary }
}

fn unreadable_notice() -> ThreadEntry {
    ThreadEntry::ContextCompaction { summary: UNREADABLE_NOTICE.to_string() }
}

/// Keep only the last [`MAX_IMPORT_ENTRIES`], prefixing an elision divider when
/// anything was dropped (shared cap with the Claude/Codex importers).
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
