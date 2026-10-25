//! Child events waiting to reach their parent (the V032 `agent_event_outbox`).
//!
//! **Consumed on acceptance, never on write.** A row moves
//! `pending → claimed → sent → accepted → delivered`:
//! - `claimed`: a drainer took it and handed it to the parent's send gate,
//!   which may still be holding it (busy turn, Stop, prompt pending).
//! - `sent`: the line carrying it was written to the parent's CLI.
//! - `accepted`: the CLI said it started on the line. From here it is never
//!   resent verbatim.
//! - `delivered`: the turn that carried it reported it.
//!
//! Off the happy path: `unconfirmed` (written, but no proof of acceptance in
//! time, or the connection dropped) is never resent on its own; `undeliverable`
//! (the parent ended) is a UI-only record. A claim the gate gives back (the
//! user stopped the parent before the line was read) returns to `pending`.
//!
//! **One batch per drain.** Every row owed to a parent goes out together under
//! one key derived from the row ids and their attempt counts. The same rows
//! re-rendered after a restart carry the same key (so a line that may already
//! be in the CLI is never sent twice under two ids), a different set can never
//! collide, and a row sent again on purpose (its line was cancelled, or its
//! turn was interrupted after the CLI took it) counts an attempt first, so the
//! CLI never sees a uuid it has already seen.
//!
//! **Edge identity.** `UNIQUE(parent_session, child, kind, edge_id)` makes a
//! second watcher pass over the same child turn a no-op.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::envelope::ChildEvent;

/// First retry delay for a parent that could not be reached.
pub const BACKOFF_START_S: i64 = 1;

/// Longest retry delay.
pub const BACKOFF_CAP_S: i64 = 60;

/// Where a row stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxState {
    Pending,
    Claimed,
    Sent,
    Accepted,
    Delivered,
    Unconfirmed,
    Undeliverable,
}

impl OutboxState {
    pub fn as_text(self) -> &'static str {
        match self {
            OutboxState::Pending => "pending",
            OutboxState::Claimed => "claimed",
            OutboxState::Sent => "sent",
            OutboxState::Accepted => "accepted",
            OutboxState::Delivered => "delivered",
            OutboxState::Unconfirmed => "unconfirmed",
            OutboxState::Undeliverable => "undeliverable",
        }
    }

    /// An unrecognised value reads as `Unconfirmed`: shown to the user, never
    /// resent on its own.
    fn from_text(text: &str) -> Self {
        match text {
            "pending" => OutboxState::Pending,
            "claimed" => OutboxState::Claimed,
            "sent" => OutboxState::Sent,
            "accepted" => OutboxState::Accepted,
            "delivered" => OutboxState::Delivered,
            "undeliverable" => OutboxState::Undeliverable,
            _ => OutboxState::Unconfirmed,
        }
    }
}

/// One outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    pub parent: String,
    pub child: String,
    pub edge_id: String,
    pub event: ChildEvent,
    pub state: OutboxState,
    pub send_key: Option<Uuid>,
    pub attempts: u32,
}

/// The rows one drain delivers, and the key that identifies the delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxBatch {
    pub parent: String,
    pub key: Uuid,
    pub rows: Vec<OutboxRow>,
}

impl OutboxBatch {
    pub fn events(&self) -> Vec<ChildEvent> {
        self.rows.iter().map(|r| r.event.clone()).collect()
    }
}

/// The outbox table. Cloning shares the connection.
#[derive(Clone)]
pub struct Outbox {
    conn: Arc<Mutex<Connection>>,
}

const COLUMNS: &str = "id, parent_session, child, edge_id, payload, state, send_key, attempts";

impl Outbox {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Record an event a child owes `parent`. Returns `false` when the same
    /// edge was already recorded (a second watcher pass).
    pub fn push(&self, parent: &str, event: &ChildEvent, edge_id: &str, now: DateTime<Local>) -> Result<bool> {
        let payload = serde_json::to_string(event).context("encode child event")?;
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO agent_event_outbox \
             (parent_session, child, kind, edge_id, payload, state, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
            params![parent, event.child, event.kind.as_text(), edge_id, payload, stamp(now)],
        )?;
        Ok(n > 0)
    }

    /// Claim every row owed to `parent` that is due, as one batch, for this
    /// host instance. `None` when nothing is due.
    pub fn claim(&self, parent: &str, instance: &str, now: DateTime<Local>) -> Result<Option<OutboxBatch>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin outbox claim")?;
        let rows = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {COLUMNS} FROM agent_event_outbox WHERE parent_session = ?1 \
                 AND state = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?2) \
                 ORDER BY id"
            ))?;
            stmt.query_map(params![parent, stamp(now)], read_row)?.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            return Ok(None);
        }
        let key = batch_key(rows.iter().map(|r| (r.id, r.attempts)));
        for row in &rows {
            tx.execute(
                "UPDATE agent_event_outbox SET state = 'claimed', claimed_by = ?2, claimed_at = ?3, \
                 send_key = ?4 WHERE id = ?1",
                params![row.id, instance, stamp(now), key.to_string()],
            )?;
        }
        tx.commit().context("commit outbox claim")?;
        let rows = rows
            .into_iter()
            .map(|r| OutboxRow { state: OutboxState::Claimed, send_key: Some(key), ..r })
            .collect();
        Ok(Some(OutboxBatch { parent: parent.to_string(), key, rows }))
    }

    /// The parent could not be reached (unbound, adapter error): give the
    /// batch back and try again later, each time waiting twice as long.
    pub fn release_with_backoff(&self, key: Uuid, now: DateTime<Local>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let attempts: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(attempts), 0) FROM agent_event_outbox WHERE send_key = ?1 \
                 AND state = 'claimed'",
                params![key.to_string()],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let next = now + chrono::Duration::seconds(backoff_s(attempts + 1));
        conn.execute(
            "UPDATE agent_event_outbox SET state = 'pending', claimed_by = NULL, claimed_at = NULL, \
             send_key = NULL, attempts = attempts + 1, next_attempt_at = ?2 \
             WHERE send_key = ?1 AND state = 'claimed'",
            params![key.to_string(), stamp(next)],
        )?;
        Ok(())
    }

    /// The gate gave the batch back unsent (the parent's view went away), or
    /// the CLI confirmed it dropped the line before starting on it (the user
    /// stopped the parent). Due again at once, under a new key.
    pub fn unclaim(&self, key: Uuid) -> Result<usize> {
        self.reopen(key, &["claimed", "sent"])
    }

    /// The CLI started on the line but the turn was interrupted before it
    /// reported it. The rows go out again with the next delivery, under a new
    /// key (never the same line twice).
    pub fn rearm(&self, key: Uuid) -> Result<usize> {
        self.reopen(key, &["accepted"])
    }

    fn reopen(&self, key: Uuid, from: &[&str]) -> Result<usize> {
        let from = from.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            &format!(
                "UPDATE agent_event_outbox SET state = 'pending', claimed_by = NULL, claimed_at = NULL, \
                 send_key = NULL, attempts = attempts + 1 WHERE send_key = ?1 AND state IN ({from})"
            ),
            params![key.to_string()],
        )?)
    }

    /// The batch's line was written to the parent's CLI.
    pub fn mark_sent(&self, key: Uuid) -> Result<usize> {
        self.transition(key, &["claimed"], "sent", None)
    }

    /// The CLI started on the line: never resend it verbatim. Also settles a
    /// line given up on as unconfirmed whose proof arrived late.
    pub fn mark_accepted(&self, key: Uuid, now: DateTime<Local>) -> Result<usize> {
        self.transition(key, &["claimed", "sent", "unconfirmed"], "accepted", Some(("accepted_at", now)))
    }

    /// The turn that carried the line reported it (late proof included).
    pub fn mark_delivered(&self, key: Uuid, now: DateTime<Local>) -> Result<usize> {
        self.transition(key, &["claimed", "sent", "accepted", "unconfirmed"], "delivered", Some(("delivered_at", now)))
    }

    /// Written, but no proof it was taken. Shown, never resent on its own.
    pub fn mark_unconfirmed(&self, key: Uuid) -> Result<usize> {
        self.transition(key, &["sent"], "unconfirmed", None)
    }

    /// The parent's session went away (its tab closed, its process ended):
    /// batches its gate still held go back to `pending` for the next bind;
    /// lines already written but not seen accepted become `unconfirmed`.
    pub fn release_parent(&self, parent: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // A new key next time: the gate that held the batch may still hold
        // (and later write) the old one.
        conn.execute(
            "UPDATE agent_event_outbox SET state = 'pending', claimed_by = NULL, claimed_at = NULL, \
             send_key = NULL, attempts = attempts + 1 WHERE parent_session = ?1 AND state = 'claimed'",
            params![parent],
        )?;
        conn.execute(
            "UPDATE agent_event_outbox SET state = 'unconfirmed' WHERE parent_session = ?1 AND state = 'sent'",
            params![parent],
        )?;
        Ok(())
    }

    /// Everything still owed to a parent that has ended becomes a UI-only
    /// record.
    pub fn settle_undeliverable(&self, parent: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE agent_event_outbox SET state = 'undeliverable' WHERE parent_session = ?1 \
             AND state IN ('pending', 'claimed')",
            params![parent],
        )?)
    }

    /// A new host instance took the data directory over (the host lock proves
    /// the previous one is gone). Its claims that were never written go back
    /// to `pending`; lines it had written but not seen accepted become
    /// `unconfirmed`, since the CLI may have read them.
    pub fn take_over(&self, instance: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE agent_event_outbox SET state = 'pending', claimed_by = NULL, claimed_at = NULL, \
             send_key = NULL WHERE state = 'claimed' AND claimed_by IS NOT ?1",
            params![instance],
        )?;
        conn.execute(
            "UPDATE agent_event_outbox SET state = 'unconfirmed' WHERE state = 'sent' \
             AND claimed_by IS NOT ?1",
            params![instance],
        )?;
        Ok(())
    }

    /// Events still on their way to `parent` (pending or held in its gate),
    /// for "N child events waiting".
    pub fn pending_count(&self, parent: &str) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM agent_event_outbox WHERE parent_session = ?1 \
             AND state IN ('pending', 'claimed')",
            params![parent],
            |r| r.get(0),
        )?)
    }

    /// Parents with rows due now (not waiting out a backoff), for the
    /// drainer's sweep.
    pub fn parents_with_pending(&self, now: DateTime<Local>) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT parent_session FROM agent_event_outbox WHERE state = 'pending' \
             AND (next_attempt_at IS NULL OR next_attempt_at <= ?1) ORDER BY parent_session",
        )?;
        let parents = stmt.query_map(params![stamp(now)], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(parents)
    }

    /// Parents with a batch handed to their gate or written and not yet
    /// settled: the drainer must hear those parents' receipts.
    pub fn parents_in_flight(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT parent_session FROM agent_event_outbox \
             WHERE state IN ('claimed', 'sent', 'accepted', 'unconfirmed') ORDER BY parent_session",
        )?;
        let parents = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(parents)
    }

    /// Whether an event for this edge was ever recorded (any state).
    pub fn has_edge(&self, parent: &str, child: &str, kind: &str, edge_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: u32 = conn.query_row(
            "SELECT COUNT(*) FROM agent_event_outbox WHERE parent_session = ?1 AND child = ?2 \
             AND kind = ?3 AND edge_id = ?4",
            params![parent, child, kind, edge_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Every row for `parent`, oldest first (tests and the UI's held list).
    pub fn rows_for(&self, parent: &str) -> Result<Vec<OutboxRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("SELECT {COLUMNS} FROM agent_event_outbox WHERE parent_session = ?1 ORDER BY id"))?;
        let rows = stmt.query_map(params![parent], read_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn transition(
        &self,
        key: Uuid,
        from: &[&str],
        to: &str,
        stamp_column: Option<(&str, DateTime<Local>)>,
    ) -> Result<usize> {
        let from = from.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
        let conn = self.conn.lock().unwrap();
        let n = match stamp_column {
            Some((column, at)) => conn.execute(
                &format!(
                    "UPDATE agent_event_outbox SET state = ?2, {column} = ?3 \
                     WHERE send_key = ?1 AND state IN ({from})"
                ),
                params![key.to_string(), to, stamp(at)],
            )?,
            None => conn.execute(
                &format!("UPDATE agent_event_outbox SET state = ?2 WHERE send_key = ?1 AND state IN ({from})"),
                params![key.to_string(), to],
            )?,
        };
        Ok(n)
    }
}

/// The delivery key for a set of rows: their sorted `(id, attempts)` pairs,
/// hashed. Shaped as a uuid because it doubles as the id the parent's CLI
/// reports the line by.
pub fn batch_key(rows: impl IntoIterator<Item = (i64, u32)>) -> Uuid {
    let mut rows: Vec<(i64, u32)> = rows.into_iter().collect();
    rows.sort_unstable();
    let mut h = Sha256::new();
    h.update(b"oximux-outbox");
    for (id, attempts) in rows {
        h.update(id.to_le_bytes());
        h.update(attempts.to_le_bytes());
    }
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

/// Seconds to wait before the `attempt`-th retry (1-based): 1, 2, 4 … capped.
pub fn backoff_s(attempt: u32) -> i64 {
    let shift = attempt.saturating_sub(1).min(16);
    (BACKOFF_START_S << shift).min(BACKOFF_CAP_S)
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<OutboxRow> {
    let payload: String = row.get(4)?;
    let event = serde_json::from_str(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let state: String = row.get(5)?;
    let key: Option<String> = row.get(6)?;
    Ok(OutboxRow {
        id: row.get(0)?,
        parent: row.get(1)?,
        child: row.get(2)?,
        edge_id: row.get(3)?,
        event,
        state: OutboxState::from_text(&state),
        send_key: key.and_then(|k| Uuid::parse_str(&k).ok()),
        attempts: row.get(7)?,
    })
}

/// UTC, so text comparison orders correctly (same as the lineage table).
fn stamp(at: DateTime<Local>) -> String {
    at.with_timezone(&chrono::Utc).to_rfc3339()
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
