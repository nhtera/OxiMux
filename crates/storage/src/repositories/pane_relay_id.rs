//! `PaneRelayIdRepo` — per-leaf-tab mapping to relay-side PTY ids backing
//! the re-attach reconciliation. One row per live leaf-tab in a project's
//! tab snapshot, keyed by `(project_id, window_id, ordinal, sub_pane, tab)`.
//! `sub_pane` is the leaf's DFS position (aligned with `pane_buffers`);
//! `tab` is the per-pane tab index. Single-leaf/single-tab terminals use
//! `sub_pane = 0, tab = 0`.
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

/// One persisted leaf-tab row:
/// `(ordinal, sub_pane, tab, relay_pty_id, relay_session)`.
pub type PaneRelayRow = (u32, u32, u32, String, String);

#[derive(Clone)]
pub struct PaneRelayIdRepo {
    db: Db,
}

impl PaneRelayIdRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Upsert one leaf-tab's relay id at `(window_id, ordinal, sub_pane,
    /// tab)`. `window_id` scopes the row to one app window; use `"main"`
    /// for the first/only window to match the V005 migration default.
    /// `sub_pane` is the leaf's DFS position, `tab` its per-pane tab
    /// index — both `0` for a single-leaf/single-tab terminal.
    // Each argument maps 1:1 to a primary-key / payload column of the upsert;
    // bundling them into a struct would only add indirection at the call sites.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &self,
        project_id: &str,
        window_id: &str,
        ordinal: u32,
        sub_pane: u32,
        tab: u32,
        relay_pty_id: &str,
        relay_session: &str,
    ) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO pane_relay_ids \
                    (project_id, window_id, ordinal, sub_pane, tab, relay_pty_id, relay_session, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(project_id, window_id, ordinal, sub_pane, tab) DO UPDATE SET \
                    relay_pty_id = excluded.relay_pty_id, \
                    relay_session = excluded.relay_session, \
                    created_at = excluded.created_at",
                params![
                    project_id,
                    window_id,
                    ordinal,
                    sub_pane,
                    tab,
                    relay_pty_id,
                    relay_session,
                    ts
                ],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Fetch every `(ordinal, sub_pane, tab, relay_pty_id, relay_session)`
    /// for a project scoped to `window_id`, ordered by
    /// `(ordinal, sub_pane, tab)` ascending. Caller pairs the rows with
    /// leaf-tabs in the matching DFS order used by `pane_buffers`.
    pub fn get_all_for_project(
        &self,
        project_id: &str,
        window_id: &str,
    ) -> Result<Vec<PaneRelayRow>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT ordinal, sub_pane, tab, relay_pty_id, relay_session FROM pane_relay_ids \
                 WHERE project_id = ?1 AND window_id = ?2 \
                 ORDER BY ordinal ASC, sub_pane ASC, tab ASC",
            )?;
            let rows = stmt.query_map(params![project_id, window_id], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
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
