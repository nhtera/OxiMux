//! What became of each send (the V033 `send_receipts`).
//!
//! **At most once.** A send is reserved `pending` under a hash of its key before
//! it is written, and moves on only with proof: `accepted` (the agent started on
//! it), `rejected` (the provider refused the turn before producing anything) or,
//! when the host went away first, `unknown`. Nothing resends an `accepted` or
//! `unknown` send on its own; a new send is a new key.
//!
//! **Keys are hashed.** A desktop send's key is its gate id, a remote caller's
//! is whatever string it chose; neither is stored, and the send's text never is.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// How long a receipt is kept: a week, so a chat reopened days after a quit
/// still knows its last send was read (the transcript blob outlives a day).
pub const KEEP_HOURS: i64 = 7 * 24;

/// Where a send stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptState {
    Pending,
    Accepted,
    Rejected,
    Unknown,
}

impl ReceiptState {
    pub fn as_text(self) -> &'static str {
        match self {
            ReceiptState::Pending => "pending",
            ReceiptState::Accepted => "accepted",
            ReceiptState::Rejected => "rejected",
            ReceiptState::Unknown => "unknown",
        }
    }

    /// An unrecognised value reads as `Unknown`: never resent on its own.
    fn from_text(text: &str) -> Self {
        match text {
            "pending" => ReceiptState::Pending,
            "accepted" => ReceiptState::Accepted,
            "rejected" => ReceiptState::Rejected,
            _ => ReceiptState::Unknown,
        }
    }
}

/// Whose key it is: the two never collide even when the strings match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// A desktop send's gate id.
    Gate,
    /// A remote caller's idempotency key (and a child's continuation id).
    V2,
}

/// The stored form of a key.
pub fn key_hash(kind: KeyKind, key: &str) -> String {
    let tag = match kind {
        KeyKind::Gate => "gate",
        KeyKind::V2 => "v2",
    };
    let digest = Sha256::digest(format!("{tag}:{key}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The receipts table. Cloning shares the connection.
#[derive(Clone)]
pub struct Receipts {
    conn: Arc<Mutex<Connection>>,
}

impl Receipts {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Reserve `key_hash` as `pending` before writing the send. `Some(state)`
    /// when the key was already recorded: the caller must not send it again.
    pub fn reserve(&self, session: &str, key_hash: &str, instance: &str, now: DateTime<Local>) -> Result<Option<ReceiptState>> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO send_receipts \
             (session_id, key_hash, state, host_instance, created_at, updated_at) \
             VALUES (?1, ?2, 'pending', ?3, ?4, ?4)",
            params![session, key_hash, instance, stamp(now)],
        )?;
        if n > 0 {
            return Ok(None);
        }
        read_state(&conn, session, key_hash)
    }

    /// Record proof for a send. A send with no proof yet (`pending`, or
    /// `unknown` when the proof arrives late) moves; `rejected` is final; and
    /// `rejected` overrides `accepted`, which a CLI reports before the provider
    /// answers (so the two settle the same in either order). A proof that beats
    /// its reservation inserts the row.
    pub fn settle(
        &self,
        session: &str,
        key_hash: &str,
        state: ReceiptState,
        instance: &str,
        now: DateTime<Local>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO send_receipts (session_id, key_hash, state, host_instance, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
             ON CONFLICT (session_id, key_hash) DO UPDATE SET state = excluded.state, \
             updated_at = excluded.updated_at WHERE send_receipts.state IN ('pending', 'unknown') \
             OR (excluded.state = 'rejected' AND send_receipts.state = 'accepted')",
            params![session, key_hash, state.as_text(), instance, stamp(now)],
        )
        .context("settle send receipt")?;
        Ok(())
    }

    /// A refused send is being sent again, verbatim and under the same key:
    /// it has no proof again until the agent answers it.
    pub fn resend(&self, session: &str, key_hash: &str, now: DateTime<Local>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE send_receipts SET state = 'pending', updated_at = ?3 \
             WHERE session_id = ?1 AND key_hash = ?2 AND state = 'rejected'",
            params![session, key_hash, stamp(now)],
        )?;
        Ok(())
    }

    /// Undo a reservation whose write failed: nothing reached the agent, so
    /// the same key may be sent again. A settled receipt is kept.
    pub fn forget_pending(&self, session: &str, key_hash: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM send_receipts WHERE session_id = ?1 AND key_hash = ?2 AND state = 'pending'",
            params![session, key_hash],
        )?;
        Ok(())
    }

    /// What is recorded for a send, if anything.
    pub fn state(&self, session: &str, key_hash: &str) -> Result<Option<ReceiptState>> {
        let conn = self.conn.lock().unwrap();
        read_state(&conn, session, key_hash)
    }

    /// A host has started: every send another instance reserved and never
    /// settled may or may not have reached its agent.
    pub fn take_over(&self, instance: &str, now: DateTime<Local>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE send_receipts SET state = 'unknown', updated_at = ?2 \
             WHERE state = 'pending' AND host_instance != ?1",
            params![instance, stamp(now)],
        )?)
    }

    /// Drop receipts older than [`KEEP_HOURS`].
    pub fn prune(&self, now: DateTime<Local>) -> Result<usize> {
        let cutoff = now - chrono::Duration::hours(KEEP_HOURS);
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM send_receipts WHERE created_at < ?1", params![stamp(cutoff)])?)
    }
}

fn read_state(conn: &Connection, session: &str, key_hash: &str) -> Result<Option<ReceiptState>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT state FROM send_receipts WHERE session_id = ?1 AND key_hash = ?2",
            params![session, key_hash],
            |r| r.get(0),
        )
        .optional()?;
    Ok(text.as_deref().map(ReceiptState::from_text))
}

fn stamp(at: DateTime<Local>) -> String {
    at.with_timezone(&chrono::Utc).to_rfc3339()
}

#[cfg(test)]
#[path = "send_receipts_tests.rs"]
mod tests;
