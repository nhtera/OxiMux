//! Index writes. Every batch is one `BEGIN IMMEDIATE` transaction that adds
//! rows, merges session metadata and advances the file's watermark — so a
//! crash mid-file loses at most one batch and the next pass resumes from the
//! last committed line.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::schema::text_column;
use super::turn::{MetaDelta, Row};
use super::watermark::Watermark;
use super::{Agent, canonical_cwd_key};

pub fn load_watermark(conn: &Connection, path: &str) -> Result<Option<Watermark>> {
    Ok(conn
        .query_row(
            "SELECT byte_offset, mtime_ms, size_bytes, session_row_id, fail_count, failed_mtime_ms
             FROM files WHERE path = ?1",
            [path],
            |r| {
                Ok(Watermark {
                    offset: r.get::<_, i64>(0)? as u64,
                    mtime_ms: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    session_row_id: r.get(3)?,
                    fail_count: r.get(4)?,
                    failed_mtime_ms: r.get(5)?,
                })
            },
        )
        .optional()?)
}

/// Everything one batch writes.
pub struct Batch<'a> {
    pub path: &'a str,
    pub agent: Agent,
    /// Used when the transcript names no id of its own (Claude: file stem).
    pub fallback_session_id: &'a str,
    /// The session row this file already owns, if any.
    pub session_row_id: Option<i64>,
    pub meta: &'a MetaDelta,
    pub rows: &'a [Row],
    /// Bytes consumed after this batch (always a line end).
    pub offset: u64,
    pub mtime_ms: i64,
    /// Size recorded with the watermark: the file's stat size once it was
    /// read to its last complete line (so an unchanged file — even one ending
    /// in a partial line — is skipped next pass), else the bytes consumed (so
    /// a read paused mid-file differs from its stat and is resumed).
    pub size: u64,
}

/// Commit one batch; returns the file's session row id (`None` while the
/// file has yielded nothing worth a session row).
pub fn commit_batch(conn: &mut Connection, b: &Batch) -> Result<Option<i64>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let m = b.meta;
    let empty = b.rows.is_empty() && *m == MetaDelta::default();
    let key = m.cwd.as_deref().map(canonical_cwd_key);
    let session_row_id = match b.session_row_id {
        None if empty => None,
        Some(id) => {
            tx.execute(
                "UPDATE sessions SET
                   session_id = COALESCE(?2, session_id),
                   cwd = COALESCE(cwd, ?3), cwd_key = COALESCE(cwd_key, ?4),
                   branch = COALESCE(branch, ?5),
                   custom_title = COALESCE(?6, custom_title), ai_title = COALESCE(?7, ai_title),
                   last_prompt = COALESCE(last_prompt, ?8), first_prompt = COALESCE(first_prompt, ?9),
                   created_ms = COALESCE(created_ms, ?10),
                   updated_ms = MAX(COALESCE(updated_ms, 0), COALESCE(?11, 0)),
                   message_count = message_count + ?12
                 WHERE id = ?1",
                params![
                    id,
                    m.session_id,
                    m.cwd,
                    key,
                    m.branch,
                    m.custom_title,
                    m.ai_title,
                    m.last_prompt,
                    m.first_prompt,
                    m.first_ts_ms,
                    m.last_ts_ms,
                    m.message_count
                ],
            )?;
            Some(id)
        }
        None => {
            tx.execute(
                "INSERT INTO sessions(agent, session_id, file_path, custom_title, ai_title, last_prompt,
                   first_prompt, cwd, cwd_key, branch, created_ms, updated_ms, message_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    b.agent.as_str(),
                    m.session_id.as_deref().unwrap_or(b.fallback_session_id),
                    b.path,
                    m.custom_title,
                    m.ai_title,
                    m.last_prompt,
                    m.first_prompt,
                    m.cwd,
                    key,
                    m.branch,
                    m.first_ts_ms,
                    m.last_ts_ms,
                    m.message_count
                ],
            )?;
            Some(tx.last_insert_rowid())
        }
    };
    {
        let mut msg = tx.prepare_cached("INSERT INTO messages(session_row_id, role, ts_ms) VALUES (?1, ?2, ?3)")?;
        let mut fts = [
            tx.prepare_cached("INSERT INTO messages_fts(rowid, user_text, identifiers) VALUES (?1, ?2, ?3)")?,
            tx.prepare_cached("INSERT INTO messages_fts(rowid, assistant_text, identifiers) VALUES (?1, ?2, ?3)")?,
            tx.prepare_cached("INSERT INTO messages_fts(rowid, tool_text, identifiers) VALUES (?1, ?2, ?3)")?,
        ];
        for row in b.rows {
            msg.execute(params![session_row_id, row.role.as_str(), row.ts_ms])?;
            let id = tx.last_insert_rowid();
            fts[text_column(row.role)].execute(params![id, row.text, row.identifiers])?;
        }
    }
    tx.execute(
        "INSERT INTO files(path, agent, byte_offset, mtime_ms, size_bytes, session_row_id, fail_count, failed_mtime_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, NULL)
         ON CONFLICT(path) DO UPDATE SET byte_offset = ?3, mtime_ms = ?4, size_bytes = ?5,
           session_row_id = ?6, fail_count = 0, failed_mtime_ms = NULL",
        params![b.path, b.agent.as_str(), b.offset as i64, b.mtime_ms, b.size as i64, session_row_id],
    )?;
    tx.commit()?;
    Ok(session_row_id)
}

/// Forget a file's indexed content before it is re-read from the top. The
/// `files` row stays, offset reset, so its failure count survives a read
/// that fails again.
pub fn drop_content(conn: &mut Connection, path: &str) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    drop_session_in(&tx, path)?;
    tx.execute("UPDATE files SET byte_offset = 0, size_bytes = 0, session_row_id = NULL WHERE path = ?1", [path])?;
    tx.commit()?;
    Ok(())
}

fn drop_session_in(tx: &rusqlite::Transaction, path: &str) -> Result<()> {
    let session: Option<Option<i64>> =
        tx.query_row("SELECT session_row_id FROM files WHERE path = ?1", [path], |r| r.get(0)).optional()?;
    if let Some(Some(id)) = session {
        tx.execute("DELETE FROM messages_fts WHERE rowid IN (SELECT id FROM messages WHERE session_row_id = ?1)", [id])?;
        tx.execute("DELETE FROM messages WHERE session_row_id = ?1", [id])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
    }
    Ok(())
}

/// Count a failed read at `mtime_ms` (resets when the mtime moves on).
pub fn record_failure(conn: &Connection, path: &str, agent: Agent, mtime_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO files(path, agent, byte_offset, mtime_ms, size_bytes, fail_count, failed_mtime_ms)
         VALUES (?1, ?2, 0, 0, 0, 1, ?3)
         ON CONFLICT(path) DO UPDATE SET
           fail_count = CASE WHEN failed_mtime_ms = ?3 THEN fail_count + 1 ELSE 1 END,
           failed_mtime_ms = ?3",
        params![path, agent.as_str(), mtime_ms],
    )?;
    Ok(())
}

/// Retire up to `limit` indexed files that no longer exist on disk.
pub fn retire_missing(conn: &mut Connection, present: &HashSet<String>, limit: usize) -> Result<usize> {
    let gone: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM files")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.filter_map(Result::ok).filter(|p| !present.contains(p)).take(limit).collect()
    };
    if gone.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for p in &gone {
        drop_session_in(&tx, p)?;
        tx.execute("DELETE FROM files WHERE path = ?1", [p])?;
    }
    tx.commit()?;
    Ok(gone.len())
}
