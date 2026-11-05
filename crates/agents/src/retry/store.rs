//! The automatic retries of failed turns (the V033 `turn_retries`).
//!
//! **The attempt count is durable.** It lives in the row keyed by the failed
//! turn, so neither a restart nor re-arming resets it; at most one retry is
//! live per session (a partial unique index). A row is resolved when the turn
//! succeeds, runs out of attempts, the user cancels it, or the user sends
//! something new.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// How long a resolved row is kept.
pub const KEEP_HOURS: i64 = 24;

/// A live row whose turn first failed this long ago was abandoned (its chat
/// closed with a retry armed): no retry waits that long in total.
pub const ABANDONED_DAYS: i64 = 7;

/// Why a retry stopped being live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Succeeded,
    Exhausted,
    Cancelled,
    /// The user sent something new.
    Superseded,
}

impl Resolution {
    pub fn as_text(self) -> &'static str {
        match self {
            Resolution::Succeeded => "succeeded",
            Resolution::Exhausted => "exhausted",
            Resolution::Cancelled => "cancelled",
            Resolution::Superseded => "superseded",
        }
    }
}

/// A session's live retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRetry {
    pub turn_key: String,
    pub class: String,
    pub reason: String,
    pub attempts: u32,
    /// When it fires, unix ms; `None` while none is armed.
    pub next_wake_at_ms: Option<i64>,
}

/// The retries table. Cloning shares the connection.
#[derive(Clone)]
pub struct RetryStore {
    conn: Arc<Mutex<Connection>>,
}

impl RetryStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Automatic attempts already fired for `turn_key` (0 when unseen).
    pub fn attempts(&self, session: &str, turn_key: &str) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT attempts FROM turn_retries WHERE session_id = ?1 AND turn_key = ?2",
                params![session, turn_key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Arm a retry of `turn_key` at `wake_at_ms`. A live retry of another turn
    /// in the same session is superseded; a cancelled one of this turn starts
    /// counting again (the user dropped it, as before). Returns the attempts
    /// already fired.
    #[allow(clippy::too_many_arguments)]
    pub fn arm(
        &self,
        session: &str,
        turn_key: &str,
        class: &str,
        reason: &str,
        wake_at_ms: i64,
        instance: &str,
        now: DateTime<Local>,
    ) -> Result<u32> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).context("begin retry arm")?;
        tx.execute(
            "UPDATE turn_retries SET resolved = 'superseded', resolved_at = ?3, next_wake_at_ms = NULL \
             WHERE session_id = ?1 AND turn_key != ?2 AND resolved IS NULL",
            params![session, turn_key, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO turn_retries (session_id, turn_key, class, reason, attempts, first_failed_at, \
             next_wake_at_ms, host_instance) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7) \
             ON CONFLICT (session_id, turn_key) DO UPDATE SET class = excluded.class, \
             reason = excluded.reason, next_wake_at_ms = excluded.next_wake_at_ms, \
             host_instance = excluded.host_instance, \
             attempts = CASE WHEN turn_retries.resolved = 'cancelled' THEN 0 ELSE turn_retries.attempts END, \
             first_failed_at = CASE WHEN turn_retries.resolved = 'cancelled' THEN excluded.first_failed_at \
             ELSE turn_retries.first_failed_at END, \
             resolved = NULL, resolved_at = NULL",
            params![session, turn_key, class, reason, stamp(now), wake_at_ms, instance],
        )?;
        let attempts: u32 = tx.query_row(
            "SELECT attempts FROM turn_retries WHERE session_id = ?1 AND turn_key = ?2",
            params![session, turn_key],
            |r| r.get(0),
        )?;
        tx.commit().context("commit retry arm")?;
        Ok(attempts)
    }

    /// The armed retry fired: one more attempt spent, nothing armed. Returns
    /// the attempts now spent.
    pub fn fired(&self, session: &str, turn_key: &str) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE turn_retries SET attempts = attempts + 1, next_wake_at_ms = NULL \
             WHERE session_id = ?1 AND turn_key = ?2",
            params![session, turn_key],
        )?;
        Ok(conn
            .query_row(
                "SELECT attempts FROM turn_retries WHERE session_id = ?1 AND turn_key = ?2",
                params![session, turn_key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// The session's live retry stops being live.
    pub fn resolve(&self, session: &str, how: Resolution, now: DateTime<Local>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE turn_retries SET resolved = ?2, resolved_at = ?3, next_wake_at_ms = NULL \
             WHERE session_id = ?1 AND resolved IS NULL",
            params![session, how.as_text(), stamp(now)],
        )?;
        Ok(n > 0)
    }

    /// The session's live retry, if any.
    pub fn live(&self, session: &str) -> Result<Option<LiveRetry>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT turn_key, class, reason, attempts, next_wake_at_ms FROM turn_retries \
                 WHERE session_id = ?1 AND resolved IS NULL",
                params![session],
                |r| {
                    Ok(LiveRetry {
                        turn_key: r.get(0)?,
                        class: r.get(1)?,
                        reason: r.get(2)?,
                        attempts: r.get(3)?,
                        next_wake_at_ms: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Drop rows resolved more than [`KEEP_HOURS`] ago, and live rows
    /// abandoned for [`ABANDONED_DAYS`]: first failed that long ago and with
    /// no wake since. A retry still waiting on a far reset is kept.
    pub fn prune(&self, now: DateTime<Local>) -> Result<usize> {
        let cutoff = now - chrono::Duration::hours(KEEP_HOURS);
        let abandoned = now - chrono::Duration::days(ABANDONED_DAYS);
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM turn_retries WHERE (resolved IS NOT NULL AND resolved_at < ?1) \
             OR (resolved IS NULL AND first_failed_at < ?2 AND COALESCE(next_wake_at_ms, 0) < ?3)",
            params![stamp(cutoff), stamp(abandoned), abandoned.timestamp_millis()],
        )?)
    }
}

fn stamp(at: DateTime<Local>) -> String {
    at.with_timezone(&chrono::Utc).to_rfc3339()
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
