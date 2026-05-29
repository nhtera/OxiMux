//! `PaneRelayIdRepo` — per-pane mapping to relay-side PTY ids backing
//! the Phase 5 re-attach reconciliation. One row per persisted leaf in
//! a project's tab snapshot, keyed by `(project_id, ordinal)`.
//!
//! On every successful spawn through `RelayBackend`, the factory writes
//! the new (relay_pty_id, relay_session_id) here so the next app
//! launch can: (a) ListPtys, (b) match against persisted rows, (c)
//! attach surviving PTYs vs. fall back to fresh-spawn + visual replay.
//!
//! `relay_session` is the daemon's `HelloAck.session_id` from the
//! session that minted the PTY id. When the daemon restarts under
//! the supervisor, the new session_id won't match — every row tied
//! to the old session can be bulk-deleted in one round trip instead
//! of N PtyNotFound errors.

use rusqlite::params;

use crate::db::Db;
use crate::error::StorageError;
use crate::repositories::now;

#[derive(Clone)]
pub struct PaneRelayIdRepo {
    db: Db,
}

impl PaneRelayIdRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Upsert one pane's relay id at the given `(window_id, ordinal)`.
    /// `window_id` scopes the row to one app window; use `"main"` for
    /// the first/only window to match the V005 migration default.
    pub fn set(
        &self,
        project_id: &str,
        window_id: &str,
        ordinal: u32,
        relay_pty_id: &str,
        relay_session: &str,
    ) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO pane_relay_ids \
                    (project_id, window_id, ordinal, relay_pty_id, relay_session, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(project_id, window_id, ordinal) DO UPDATE SET \
                    relay_pty_id = excluded.relay_pty_id, \
                    relay_session = excluded.relay_session, \
                    created_at = excluded.created_at",
                params![
                    project_id,
                    window_id,
                    ordinal,
                    relay_pty_id,
                    relay_session,
                    ts
                ],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Fetch every `(ordinal, relay_pty_id, relay_session)` for a
    /// project scoped to `window_id`, ordered by `ordinal` ascending.
    /// Caller pairs the rows with leaves in the matching DFS order used
    /// by `pane_buffers`.
    pub fn get_all_for_project(
        &self,
        project_id: &str,
        window_id: &str,
    ) -> Result<Vec<(u32, String, String)>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT ordinal, relay_pty_id, relay_session FROM pane_relay_ids \
                 WHERE project_id = ?1 AND window_id = ?2 ORDER BY ordinal ASC",
            )?;
            let rows = stmt.query_map(params![project_id, window_id], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok::<_, rusqlite::Error>(out)
        })?;
        Ok(rows)
    }

    /// Delete every row for a project scoped to `window_id`. Used by
    /// the reconciliation path to clear stale ids before writing a fresh
    /// snapshot. Other windows' rows for the same project are unaffected.
    pub fn delete_for_project(
        &self,
        project_id: &str,
        window_id: &str,
    ) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM pane_relay_ids WHERE project_id = ?1 AND window_id = ?2",
                params![project_id, window_id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Bulk-delete every row whose `relay_session` matches. Called once
    /// per launch after the supervisor returns the live session_id IF
    /// it differs from any persisted session_id — saves N round trips
    /// to `Attach` that would all return `PtyNotFound`.
    pub fn delete_for_session(&self, relay_session: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM pane_relay_ids WHERE relay_session = ?1",
                params![relay_session],
            )
            .map(|_| ())
        })?;
        Ok(())
    }
}

// Integration tests live in `tests/pane_relay_id_repo.rs` so the
// migration ladder + ProjectRepo seam are exercised through the
// public crate surface.
