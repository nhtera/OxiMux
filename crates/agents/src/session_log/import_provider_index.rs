//! Session indexing for ACP-preset / import-only providers whose CLIs keep
//! their own on-disk stores, surfaced in the history/import picker alongside
//! Claude/Codex. Three providers, three store shapes:
//!
//! - **OpenCode** — SQLite `~/.local/share/opencode/opencode.db`, `session`
//!   table (`id`, `title`, `directory` = cwd, `time_created`, `time_updated`,
//!   integer unix-ms). Resumes via `opencode --session <id>`.
//! - **Copilot** — SQLite `~/.copilot/session-store.db`, `sessions` (`id`,
//!   `repository`, `updated_at`, `summary`) + `turns` (`session_id`,
//!   `turn_index`, `user_message`). Title = `summary`, else the first turn's
//!   user message. Resumes via `copilot --resume=<id>`.
//! - **Pi** — JSONL `~/.pi/agent/sessions/**/*.jsonl`, header line
//!   `{"type":"session","id","cwd","timestamp"}` + a `session_info.name` title
//!   record. Resumes via `pi --session <file>` (the file path is the handle).
//!
//! Everything fails soft: a missing store / table / column / malformed line
//! yields fewer entries, never a crash. SQLite is opened read-only. Every entry
//! lands as `adapter = Custom` with `preset_id = Some(<slug>)`, so the picker
//! can tag, filter, and resume it without a first-class core adapter.
//!
//! [`load_import_provider_preview`] renders the opening turns for the preview
//! pane from the same stores (OpenCode/Copilot SQLite tables, Pi rollout JSONL)
//! shown while browsing the ⌘⇧H history picker, before a session is resumed.
//!
//! [`load_import_provider_transcript`] renders the *full* chat-transcript shape
//! (`Vec<ThreadEntry>` — bubbles, plus tool activity as plain notice rows,
//! never raw JSON) that a live chat surface would seed from, matching the
//! fidelity the Claude/Codex importers hold (`thread::session_import`,
//! `thread::codex_session_import`). OpenCode and Pi have full mappers
//! ([`super::import_transcript_opencode`], [`super::import_transcript_pi`]).
//! Copilot does not: a spike of a live `~/.copilot/session-store.db` found its
//! `turns.user_message`/`assistant_response` columns *are* populated (readable
//! prose, not empty — [`copilot_preview`] already renders a picker blurb from
//! them), but Copilot has no ACP/live-chat backend in OxiMux at all — it only
//! ever reopens as a terminal resume (`copilot --resume=<id>`), so there is no
//! consumer for a full transcript today. Copilot stays resume-only for
//! [`load_import_provider_transcript`] until a chat surface exists to seed.

use std::fs;
use std::path::{Path, PathBuf};

use oximux_core::AgentAdapter;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::session_index::{
    SessionEntry, SessionScope, cwd_matches, line_value, read_head, truncate_prompt,
};
use super::session_preview::{PreviewMessage, PreviewRole};
use super::{parse_timestamp_ms, read_tail};

/// Registry preset ids these entries carry (the slug that drives resume).
pub const OPENCODE: &str = "opencode";
pub const COPILOT: &str = "copilot";
pub const PI: &str = "pi";

/// Tail window for a Pi rollout: the newest activity timestamp is at the end.
const PI_TAIL_BYTES: u64 = 64 * 1024;
/// Head window for a Pi rollout: the session header + first genuine prompt sit
/// near the top; injected/context turns are rare in Pi so a small head suffices.
const PI_HEAD_BYTES: u64 = 128 * 1024;
/// Depth cap for the Pi sessions walk (`sessions/<project>/…`).
const PI_MAX_DEPTH: usize = 5;

/// Open a provider SQLite store read-only. `None` when the file is absent or
/// can't be opened; the URI flag lets SQLite read a live WAL database.
/// `pub(super)` so the sibling transcript mappers ([`super::import_transcript_opencode`])
/// reuse the same never-write-lock guarantee instead of opening their own
/// connection.
pub(super) fn open_ro(db: &Path) -> Option<Connection> {
    if !db.exists() {
        return None;
    }
    Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()
}

/// Does a session with this recorded cwd belong in the current scope?
fn scope_keeps(scope: &SessionScope, cwd: Option<&str>) -> bool {
    match scope {
        SessionScope::AllProjects => true,
        SessionScope::Projects(targets) => {
            cwd.is_some_and(|c| targets.iter().any(|t| cwd_matches(c, t)))
        }
    }
}

/// Coerce a SQLite timestamp cell to unix millis. Integers are taken as ms when
/// already millisecond-scale, else promoted from seconds; text is parsed as
/// RFC 3339. `None` for null/garbage.
fn ts_cell_to_ms(v: &SqlValue) -> Option<i64> {
    match v {
        SqlValue::Integer(n) => Some(if *n >= 1_000_000_000_000 { *n } else { *n * 1000 }),
        SqlValue::Real(f) => Some(*f as i64),
        SqlValue::Text(s) => parse_timestamp_ms(s)
            .or_else(|| parse_sqlite_datetime_ms(s))
            .or_else(|| {
                s.trim()
                    .parse::<i64>()
                    .ok()
                    .map(|n| if n >= 1_000_000_000_000 { n } else { n * 1000 })
            }),
        _ => None,
    }
}

/// Parse SQLite's `datetime('now')` default format (`YYYY-MM-DD HH:MM:SS`, UTC,
/// no zone marker) to unix millis. Copilot writes RFC 3339 for real rows, but a
/// column filled by the schema default lands in this shape.
fn parse_sqlite_datetime_ms(s: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    Some(naive.and_utc().timestamp_millis())
}

/// A non-empty, one-line-capped string, or `None`.
fn clean_title(s: Option<String>) -> Option<String> {
    s.map(|t| truncate_prompt(&t)).filter(|t| !t.is_empty())
}

// --- OpenCode --------------------------------------------------------------

/// Index OpenCode's `session` rows. Directory is the recorded cwd, so it scopes
/// by project like Claude/Codex.
pub fn collect_opencode(home: &Path, scope: &SessionScope, out: &mut Vec<SessionEntry>) {
    let db = home.join(".local/share/opencode/opencode.db");
    let Some(conn) = open_ro(&db) else {
        return;
    };
    let sql = "SELECT id, title, directory, time_created, time_updated FROM session";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return;
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    });
    let Ok(rows) = rows else {
        return;
    };
    for (id, title, directory, created, updated) in rows.flatten() {
        if id.is_empty() {
            continue;
        }
        let cwd = directory.filter(|d| !d.is_empty());
        if !scope_keeps(scope, cwd.as_deref()) {
            continue;
        }
        out.push(SessionEntry {
            session_id: id,
            adapter: AgentAdapter::Custom,
            preset_id: Some(OPENCODE.to_string()),
            path: None,
            cwd,
            title: clean_title(title),
            custom_title: None,
            git_branch: None,
            tag: None,
            created_at_ms: created,
            last_message_ts_ms: updated.or(created),
            message_count: None,
            size_bytes: None,
            entry_count: None,
        });
    }
}

// --- Copilot ---------------------------------------------------------------

/// Index Copilot's `sessions` rows, titling each from `summary` or its first
/// turn. The `sessions` table records the launch dir in a dedicated `cwd`
/// column (with `branch` for git and `created_at`/`updated_at` as ISO-8601
/// text), so Copilot scopes by project like the others. The schema is a
/// CLI-internal detail, so every field is read defensively.
pub fn collect_copilot(home: &Path, scope: &SessionScope, out: &mut Vec<SessionEntry>) {
    let db = home.join(".copilot/session-store.db");
    let Some(conn) = open_ro(&db) else {
        return;
    };
    let sql = "SELECT s.id, s.cwd, s.branch, s.created_at, s.updated_at, s.summary, \
               (SELECT t.user_message FROM turns t \
                WHERE t.session_id = s.id AND t.turn_index = 0 LIMIT 1) \
               FROM sessions s";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return;
    };
    let rows = stmt.query_map([], |r| {
        Ok(CopilotRow {
            id: r.get(0)?,
            cwd: r.get(1)?,
            branch: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
            summary: r.get(5)?,
            first_turn: r.get(6)?,
        })
    });
    let Ok(rows) = rows else {
        return;
    };
    for row in rows.flatten() {
        if row.id.is_empty() {
            continue;
        }
        let cwd = row.cwd.filter(|c| !c.is_empty());
        if !scope_keeps(scope, cwd.as_deref()) {
            continue;
        }
        let title = clean_title(row.summary).or_else(|| clean_title(row.first_turn));
        let created = ts_cell_to_ms(&row.created_at);
        let updated = ts_cell_to_ms(&row.updated_at);
        out.push(SessionEntry {
            session_id: row.id,
            adapter: AgentAdapter::Custom,
            preset_id: Some(COPILOT.to_string()),
            path: None,
            cwd,
            title,
            custom_title: None,
            git_branch: row.branch.filter(|b| !b.is_empty()),
            tag: None,
            created_at_ms: created,
            last_message_ts_ms: updated.or(created),
            message_count: None,
            size_bytes: None,
            entry_count: None,
        });
    }
}

/// One `sessions` row as read from Copilot's store (timestamps stay `SqlValue`
/// so integer or ISO-text columns both coerce via [`ts_cell_to_ms`]).
struct CopilotRow {
    id: String,
    cwd: Option<String>,
    branch: Option<String>,
    created_at: SqlValue,
    updated_at: SqlValue,
    summary: Option<String>,
    first_turn: Option<String>,
}

// --- Pi --------------------------------------------------------------------

/// Index Pi's per-session JSONL files under `~/.pi/agent/sessions`. The resume
/// handle is the file path (`pi --session <file>`), carried in `path`.
pub fn collect_pi(home: &Path, scope: &SessionScope, out: &mut Vec<SessionEntry>) {
    let sessions = home.join(".pi/agent/sessions");
    let mut files = Vec::new();
    collect_pi_files(&sessions, 0, &mut files);
    for path in files {
        let Some(entry) = build_pi_entry(&path) else {
            continue;
        };
        if !scope_keeps(scope, entry.cwd.as_deref()) {
            continue;
        }
        out.push(entry);
    }
}

/// Depth-bounded collection of every `*.jsonl` under the Pi sessions tree
/// (Pi groups files into `sessions/<project>/…`). Symlinks are never followed.
pub(crate) fn collect_pi_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > PI_MAX_DEPTH {
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_pi_files(&path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// Parse one Pi rollout into an entry. Requires a `session` header line with an
/// `id` (a torn/foreign file yields `None`). Title prefers a `session_info.name`
/// record, then the first genuine user message.
fn build_pi_entry(path: &Path) -> Option<SessionEntry> {
    let head = read_head(path, PI_HEAD_BYTES).unwrap_or_default();
    let tail = read_tail(path, PI_TAIL_BYTES).unwrap_or_default();

    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut created_at_ms: Option<i64> = None;
    let mut name: Option<String> = None;
    let mut first_user: Option<String> = None;

    for line in head.lines() {
        let Some(v) = line_value(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                session_id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                cwd = v
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                created_at_ms = v
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp_ms);
            }
            Some("session_info") if name.is_none() => {
                name = v
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            Some("message") if first_user.is_none() => {
                first_user = pi_user_text(&v);
            }
            _ => {}
        }
    }
    let session_id = session_id?;
    // A later rename (`session_info.name`) in the tail wins over the head's.
    let tail_name = pi_last_session_name(&tail);
    let title = clean_title(tail_name.or(name).or(first_user));
    let last_ts = pi_last_timestamp(&tail).or(created_at_ms);

    Some(SessionEntry {
        session_id,
        adapter: AgentAdapter::Custom,
        preset_id: Some(PI.to_string()),
        path: Some(path.to_string_lossy().into_owned()),
        cwd,
        title,
        custom_title: None,
        git_branch: None,
        tag: None,
        created_at_ms,
        last_message_ts_ms: last_ts,
        message_count: None,
        size_bytes: fs::metadata(path).ok().map(|m| m.len()),
        entry_count: None,
    })
}

/// First `text` block of a Pi user `message` record (string or block-array
/// content), or `None` for a non-user / empty message.
fn pi_user_text(v: &Value) -> Option<String> {
    let msg = v.get("message")?;
    if msg.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let content = msg.get("content")?;
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        content
            .as_array()?
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .find_map(|b| b.get("text").and_then(Value::as_str))?
            .to_string()
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Last `session_info.name` in the tail (a rename lands late in the file).
fn pi_last_session_name(tail: &str) -> Option<String> {
    let mut name = None;
    for line in tail.lines() {
        let Some(v) = line_value(line) else { continue };
        if v.get("type").and_then(Value::as_str) == Some("session_info")
            && let Some(n) = v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty())
        {
            name = Some(n.to_string());
        }
    }
    name
}

/// Last parseable RFC 3339 `timestamp` in the tail (newest activity).
fn pi_last_timestamp(tail: &str) -> Option<i64> {
    let mut last = None;
    for line in tail.lines() {
        let Some(v) = line_value(line) else { continue };
        if let Some(ms) = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms)
        {
            last = Some(ms);
        }
    }
    last
}

// --- Preview -------------------------------------------------------------

/// Per-message character cap for a previewed turn (mirrors `session_preview`).
const PREVIEW_MSG_CHARS: usize = 700;

/// Opening user/assistant turns for an import-provider row's preview pane —
/// read from the same store the collector indexed. OpenCode/Copilot turns come
/// from their SQLite tables (keyed by `session_id`); Pi from its rollout JSONL
/// (`path`). Bounded to `max_messages`; any read failure yields an empty preview.
pub fn load_import_provider_preview(
    home: &Path,
    preset_id: &str,
    session_id: &str,
    path: Option<&str>,
    max_messages: usize,
) -> Vec<PreviewMessage> {
    match preset_id {
        OPENCODE => opencode_preview(home, session_id, max_messages),
        COPILOT => copilot_preview(home, session_id, max_messages),
        PI => path
            .map(|p| pi_preview(Path::new(p), max_messages))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The full chat-transcript shape (`Vec<ThreadEntry>`) for an import-provider
/// row, so a chat surface can seed it before a live resume connects — the
/// counterpart of [`load_import_provider_preview`]'s short blurb. OpenCode
/// reads its SQLite store by `session_id`; Pi reads its rollout JSONL by
/// `path`. Copilot has no chat-surface consumer yet (see the module docs), so
/// it yields an empty transcript here — its picker blurb still works via
/// [`load_import_provider_preview`], and it always resumes in a terminal.
pub fn load_import_provider_transcript(
    home: &Path,
    preset_id: &str,
    session_id: &str,
    path: Option<&str>,
) -> Vec<crate::thread::ThreadEntry> {
    match preset_id {
        OPENCODE => super::import_transcript_opencode::opencode_transcript(home, session_id),
        PI => path
            .map(|p| super::import_transcript_pi::pi_transcript(Path::new(p)))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Trim + cap a previewed message body, preserving newlines.
fn cap_preview(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= PREVIEW_MSG_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(PREVIEW_MSG_CHARS).collect();
    out.push('…');
    out
}

/// Map a wire role string to a preview role (only user/assistant are shown).
fn preview_role(role: &str) -> Option<PreviewRole> {
    match role {
        "user" => Some(PreviewRole::User),
        "assistant" => Some(PreviewRole::Assistant),
        _ => None,
    }
}

/// OpenCode preview: join `message` (role) with its `part` rows (text blocks),
/// concatenating a message's text parts into one turn. `part.data` carries other
/// kinds (`step-start`, `reasoning`, `tool`) that aren't readable prose — skipped.
fn opencode_preview(home: &Path, session_id: &str, max: usize) -> Vec<PreviewMessage> {
    let db = home.join(".local/share/opencode/opencode.db");
    let Some(conn) = open_ro(&db) else {
        return Vec::new();
    };
    let sql = "SELECT m.id, m.data, p.data FROM message m \
               JOIN part p ON p.message_id = m.id \
               WHERE m.session_id = ?1 \
               ORDER BY m.time_created, p.time_created";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([session_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    // (message_id, role, accumulated text) — merge consecutive parts of a message.
    let mut acc: Vec<(String, PreviewRole, String)> = Vec::new();
    for (mid, mdata, pdata) in rows.flatten() {
        let Some(role) = line_value(&mdata)
            .as_ref()
            .and_then(|v| v.get("role").and_then(Value::as_str))
            .and_then(preview_role)
        else {
            continue;
        };
        let Some(text) = line_value(&pdata).as_ref().and_then(part_text) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        match acc.last_mut() {
            Some((last_mid, _, buf)) if *last_mid == mid => {
                buf.push('\n');
                buf.push_str(&text);
            }
            _ => {
                if acc.len() >= max {
                    break;
                }
                acc.push((mid, role, text));
            }
        }
    }
    acc.into_iter()
        .map(|(_, role, text)| PreviewMessage {
            role,
            text: cap_preview(&text),
        })
        .collect()
}

/// Text of an OpenCode `part` whose `data` is a `{"type":"text","text":…}` block.
fn part_text(data: &Value) -> Option<String> {
    if data.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    data.get("text").and_then(Value::as_str).map(str::to_string)
}

/// Copilot preview: each `turns` row carries the user message + assistant
/// response for that turn, emitted as two turns in order.
fn copilot_preview(home: &Path, session_id: &str, max: usize) -> Vec<PreviewMessage> {
    let db = home.join(".copilot/session-store.db");
    let Some(conn) = open_ro(&db) else {
        return Vec::new();
    };
    let sql = "SELECT user_message, assistant_response FROM turns \
               WHERE session_id = ?1 ORDER BY turn_index";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([session_id], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (user_msg, assistant) in rows.flatten() {
        for (role, body) in [
            (PreviewRole::User, user_msg),
            (PreviewRole::Assistant, assistant),
        ] {
            if out.len() >= max {
                return out;
            }
            if let Some(text) = body.filter(|t| !t.trim().is_empty()) {
                out.push(PreviewMessage {
                    role,
                    text: cap_preview(&text),
                });
            }
        }
    }
    out
}

/// Pi preview: pull the opening user/assistant `message` turns from the rollout
/// JSONL head, skipping plumbing (`session`/`session_info`/tool) records.
fn pi_preview(path: &Path, max: usize) -> Vec<PreviewMessage> {
    let head = read_head(path, PI_HEAD_BYTES).unwrap_or_default();
    let mut out = Vec::new();
    for line in head.lines() {
        if out.len() >= max {
            break;
        }
        let Some(v) = line_value(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        if let Some(msg) = pi_preview_message(&v) {
            out.push(msg);
        }
    }
    out
}

/// One previewable turn from a Pi `message` record (user/assistant only). Content
/// is a bare string or an array of blocks; the `text` blocks are joined.
fn pi_preview_message(v: &Value) -> Option<PreviewMessage> {
    let msg = v.get("message")?;
    let role = preview_role(msg.get("role").and_then(Value::as_str)?)?;
    let content = msg.get("content")?;
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        content
            .as_array()?
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(PreviewMessage {
        role,
        text: cap_preview(&text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(paths: &[&str]) -> SessionScope {
        SessionScope::Projects(paths.iter().map(|s| s.to_string()).collect())
    }

    // --- OpenCode ---

    fn write_opencode_db(home: &Path) {
        let dir = home.join(".local/share/opencode");
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT, directory TEXT, \
             time_created INTEGER, time_updated INTEGER);
             INSERT INTO session VALUES ('ses_a','Greeting','/p/one',1000,2000);
             INSERT INTO session VALUES ('ses_b','Fix parser','/p/two',1500,3000);
             INSERT INTO session VALUES ('ses_c','',NULL,10,20);",
        )
        .unwrap();
    }

    #[test]
    fn opencode_reads_sessions_with_cwd_and_title() {
        let home = tempfile::tempdir().unwrap();
        write_opencode_db(home.path());
        let mut out = Vec::new();
        collect_opencode(home.path(), &SessionScope::AllProjects, &mut out);
        assert_eq!(out.len(), 3);
        let a = out.iter().find(|e| e.session_id == "ses_a").unwrap();
        assert_eq!(a.preset_id.as_deref(), Some("opencode"));
        assert_eq!(a.adapter, AgentAdapter::Custom);
        assert_eq!(a.cwd.as_deref(), Some("/p/one"));
        assert_eq!(a.title.as_deref(), Some("Greeting"));
        assert_eq!(a.last_message_ts_ms, Some(2000));
        // Empty title + null directory degrade to None, not crash.
        let c = out.iter().find(|e| e.session_id == "ses_c").unwrap();
        assert_eq!(c.title, None);
        assert_eq!(c.cwd, None);
    }

    #[test]
    fn opencode_scopes_by_directory() {
        let home = tempfile::tempdir().unwrap();
        write_opencode_db(home.path());
        let mut out = Vec::new();
        collect_opencode(home.path(), &scope(&["/p/two"]), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "ses_b");
    }

    #[test]
    fn opencode_missing_db_is_empty() {
        let home = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        collect_opencode(home.path(), &SessionScope::AllProjects, &mut out);
        assert!(out.is_empty());
    }

    // --- Copilot ---

    fn write_copilot_db(home: &Path) {
        let dir = home.join(".copilot");
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("session-store.db")).unwrap();
        // Mirrors the real `~/.copilot/session-store.db` schema (dedicated `cwd`
        // + `branch` columns; a `created_at` filled by SQLite's datetime default).
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, \
             branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
             CREATE TABLE turns (id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER, user_message TEXT);
             INSERT INTO sessions VALUES ('cop_a','/p/one','','local','main','Session Initialization','2026-06-20T09:00:00.000Z','2026-06-20T10:00:00.000Z');
             INSERT INTO sessions VALUES ('cop_b','/p/two','','local','dev',NULL,'2026-06-21 09:00:00','2026-06-21 10:00:00');
             INSERT INTO turns VALUES (1,'cop_b',0,'the very first prompt');
             INSERT INTO turns VALUES (2,'cop_b',1,'a later turn');",
        )
        .unwrap();
    }

    #[test]
    fn copilot_reads_cwd_branch_title_from_summary_or_first_turn() {
        let home = tempfile::tempdir().unwrap();
        write_copilot_db(home.path());
        let mut out = Vec::new();
        collect_copilot(home.path(), &SessionScope::AllProjects, &mut out);
        assert_eq!(out.len(), 2);
        let a = out.iter().find(|e| e.session_id == "cop_a").unwrap();
        assert_eq!(a.preset_id.as_deref(), Some("copilot"));
        assert_eq!(a.title.as_deref(), Some("Session Initialization"));
        assert_eq!(a.cwd.as_deref(), Some("/p/one"));
        assert_eq!(a.git_branch.as_deref(), Some("main"));
        assert_eq!(a.last_message_ts_ms, parse_timestamp_ms("2026-06-20T10:00:00.000Z"));
        // No summary → first turn (turn_index 0) becomes the title; the
        // SQLite `datetime('now')` text form ("YYYY-MM-DD HH:MM:SS") still parses.
        let b = out.iter().find(|e| e.session_id == "cop_b").unwrap();
        assert_eq!(b.title.as_deref(), Some("the very first prompt"));
        assert_eq!(b.cwd.as_deref(), Some("/p/two"));
        assert!(b.last_message_ts_ms.unwrap() > 0);
    }

    #[test]
    fn copilot_scopes_by_cwd() {
        let home = tempfile::tempdir().unwrap();
        write_copilot_db(home.path());
        let mut out = Vec::new();
        collect_copilot(home.path(), &scope(&["/p/two"]), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "cop_b");
    }

    // --- Pi ---

    fn write_pi_session(home: &Path, rel: &str, body: &str) {
        let path = home.join(".pi/agent/sessions").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn pi_reads_header_title_and_resume_path() {
        let home = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"session","version":3,"id":"pi-1","timestamp":"2026-06-20T08:00:00.000Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"u1","timestamp":"2026-06-20T08:00:01Z","message":{"role":"user","content":[{"type":"text","text":"kick things off"}]}}"#,
            "\n",
            r#"{"type":"session_info","id":"i1","timestamp":"2026-06-20T08:05:00Z","name":"renamed session"}"#,
            "\n",
        );
        write_pi_session(home.path(), "proj/2026-06-20_session.jsonl", body);
        let mut out = Vec::new();
        collect_pi(home.path(), &SessionScope::AllProjects, &mut out);
        assert_eq!(out.len(), 1);
        let e = &out[0];
        assert_eq!(e.session_id, "pi-1");
        assert_eq!(e.preset_id.as_deref(), Some("pi"));
        assert_eq!(e.cwd.as_deref(), Some("/proj"));
        // A `session_info.name` rename wins over the first user message.
        assert_eq!(e.title.as_deref(), Some("renamed session"));
        // The resume handle is the file path.
        assert!(e.path.as_deref().unwrap().ends_with("2026-06-20_session.jsonl"));
        assert_eq!(e.last_message_ts_ms, parse_timestamp_ms("2026-06-20T08:05:00Z"));
    }

    #[test]
    fn pi_falls_back_to_first_user_message_for_title() {
        let home = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"session","id":"pi-2","timestamp":"2026-06-20T08:00:00Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"u1","message":{"role":"user","content":"plain string prompt"}}"#,
            "\n",
        );
        write_pi_session(home.path(), "2026_session.jsonl", body);
        let mut out = Vec::new();
        collect_pi(home.path(), &SessionScope::AllProjects, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("plain string prompt"));
    }

    #[test]
    fn pi_scopes_by_cwd_and_skips_headerless_files() {
        let home = tempfile::tempdir().unwrap();
        write_pi_session(
            home.path(),
            "a.jsonl",
            "{\"type\":\"session\",\"id\":\"keep\",\"cwd\":\"/proj\",\"timestamp\":\"2026-06-20T08:00:00Z\"}\n",
        );
        write_pi_session(
            home.path(),
            "b.jsonl",
            "{\"type\":\"session\",\"id\":\"drop\",\"cwd\":\"/other\",\"timestamp\":\"2026-06-20T08:00:00Z\"}\n",
        );
        // No `session` header → skipped, never panics.
        write_pi_session(home.path(), "c.jsonl", "{\"type\":\"message\"}\nnot json\n");
        let mut out = Vec::new();
        collect_pi(home.path(), &scope(&["/proj"]), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "keep");
    }

    // --- Previews ---

    #[test]
    fn opencode_preview_joins_message_role_with_text_parts() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".local/share/opencode");
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO message VALUES ('m1','s1',1,'{\"role\":\"user\"}');
             INSERT INTO message VALUES ('m2','s1',2,'{\"role\":\"assistant\"}');
             INSERT INTO part VALUES ('p1','m1','s1',1,'{\"type\":\"text\",\"text\":\"hello there\"}');
             INSERT INTO part VALUES ('p2','m2','s1',2,'{\"type\":\"step-start\"}');
             INSERT INTO part VALUES ('p3','m2','s1',3,'{\"type\":\"text\",\"text\":\"Hi!\"}');
             INSERT INTO part VALUES ('p4','m2','s1',4,'{\"type\":\"text\",\"text\":\"How can I help?\"}');",
        )
        .unwrap();
        let msgs = load_import_provider_preview(home.path(), OPENCODE, "s1", None, 8);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, PreviewRole::User);
        assert_eq!(msgs[0].text, "hello there");
        // step-start part skipped; the two text parts of m2 are concatenated.
        assert_eq!(msgs[1].role, PreviewRole::Assistant);
        assert_eq!(msgs[1].text, "Hi!\nHow can I help?");
    }

    #[test]
    fn copilot_preview_emits_user_then_assistant_per_turn() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".copilot");
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("session-store.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE turns (id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER, user_message TEXT, assistant_response TEXT);
             INSERT INTO turns VALUES (1,'s1',0,'first q','first a');
             INSERT INTO turns VALUES (2,'s1',1,'second q',NULL);",
        )
        .unwrap();
        let msgs = load_import_provider_preview(home.path(), COPILOT, "s1", None, 8);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, PreviewRole::User);
        assert_eq!(msgs[0].text, "first q");
        assert_eq!(msgs[1].role, PreviewRole::Assistant);
        assert_eq!(msgs[1].text, "first a");
        // A null assistant_response is skipped; the next user turn still shows.
        assert_eq!(msgs[2].role, PreviewRole::User);
        assert_eq!(msgs[2].text, "second q");
    }

    #[test]
    fn pi_preview_reads_message_turns_skipping_non_text() {
        let home = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"session","id":"pi-1","cwd":"/proj","timestamp":"2026-06-20T08:00:00Z"}"#,
            "\n",
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
            "\n",
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","text":"hmm"},{"type":"text","text":"done"}]}}"#,
            "\n",
        );
        write_pi_session(home.path(), "s.jsonl", body);
        let path = home.path().join(".pi/agent/sessions/s.jsonl");
        let msgs =
            load_import_provider_preview(home.path(), PI, "pi-1", Some(path.to_str().unwrap()), 8);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, PreviewRole::User);
        assert_eq!(msgs[0].text, "do the thing");
        // The `thinking` block is dropped; only the `text` block is kept.
        assert_eq!(msgs[1].role, PreviewRole::Assistant);
        assert_eq!(msgs[1].text, "done");
    }

    // --- Transcript dispatch ---
    //
    // The mapping logic itself (turn ordering, tool degrade, corrupt-store
    // handling) is covered in each sibling mapper's own tests
    // (`import_transcript_opencode`, `import_transcript_pi`); these just prove
    // `load_import_provider_transcript` routes to the right one.

    #[test]
    fn transcript_dispatch_routes_opencode_by_session_id() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".local/share/opencode");
        fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO message VALUES ('m1','s1',1,'{\"role\":\"user\"}');
             INSERT INTO part VALUES ('p1','m1','s1',1,'{\"type\":\"text\",\"text\":\"hi\"}');",
        )
        .unwrap();
        let entries = load_import_provider_transcript(home.path(), OPENCODE, "s1", None);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn transcript_dispatch_routes_pi_by_path() {
        let home = tempfile::tempdir().unwrap();
        write_pi_session(
            home.path(),
            "s.jsonl",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        );
        let path = home.path().join(".pi/agent/sessions/s.jsonl");
        let entries = load_import_provider_transcript(
            home.path(),
            PI,
            "unused",
            Some(path.to_str().unwrap()),
        );
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn transcript_dispatch_yields_empty_for_copilot_and_unknown_presets() {
        let home = tempfile::tempdir().unwrap();
        assert!(load_import_provider_transcript(home.path(), COPILOT, "s1", None).is_empty());
        assert!(load_import_provider_transcript(home.path(), "unknown", "s1", None).is_empty());
    }
}
