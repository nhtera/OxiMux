//! `PaneBufferRepo` — per-pane scrollback BLOB storage backing the
//! terminal-tab restore path (Phase 4 step 16, F3.4). One row per leaf
//! inside a project's persisted tab snapshot, keyed by `(project_id,
//! ordinal, sub_pane_ordinal)`.
//!
//! `ordinal` indexes the DFS terminal-tab order across all groups.
//! `sub_pane_ordinal` indexes the DFS leaf order WITHIN a multi-
//! sub-pane terminal tab (`0` for single-pane tabs, matching pre-v4
//! snapshots). Together they identify the unique leaf whose scrollback
//! the row carries.
//!
//! Bytes are raw `serialize_term_capped` output (ANSI escapes + cells +
//! final CUP). The restore path feeds them into a fresh PTY's grid via
//! `TerminalBackend::prefill_grid` before the live shell starts producing
//! output, so the user sees prior scrollback intact across restarts.

use rusqlite::params;

use crate::db::Db;
use crate::error::StorageError;
use crate::repositories::now;

#[derive(Clone)]
pub struct PaneBufferRepo {
    db: Db,
}

impl PaneBufferRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Upsert one sub-pane's bytes at the given `(window_id, ordinal,
    /// sub_pane_ordinal)`. Single-sub-pane tabs always pass `0` for
    /// `sub_pane_ordinal` so legacy single-pane behavior is unchanged.
    /// `window_id` scopes the row to one app window; use `"main"` for
    /// the first/only window to match the V005 migration default.
    pub fn set(
        &self,
        project_id: &str,
        window_id: &str,
        ordinal: u32,
        sub_pane_ordinal: u32,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO pane_buffers \
                    (project_id, window_id, ordinal, sub_pane_ordinal, bytes, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(project_id, window_id, ordinal, sub_pane_ordinal) DO UPDATE SET \
                    bytes = excluded.bytes, updated_at = excluded.updated_at",
                params![project_id, window_id, ordinal, sub_pane_ordinal, bytes, ts],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Fetch every buffer for a project scoped to `window_id`, ordered
    /// by `(ordinal, sub_pane_ordinal)` ascending. Returns triples of
    /// `(tab_ordinal, sub_pane_ordinal, bytes)` so the restore path can
    /// dispatch scrollback into the right sub-pane via a HashMap lookup.
    pub fn get_all_for_project(
        &self,
        project_id: &str,
        window_id: &str,
    ) -> Result<Vec<(u32, u32, Vec<u8>)>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT ordinal, sub_pane_ordinal, bytes FROM pane_buffers \
                 WHERE project_id = ?1 AND window_id = ?2 \
                 ORDER BY ordinal ASC, sub_pane_ordinal ASC",
            )?;
            let rows = stmt.query_map(params![project_id, window_id], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
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

    /// Delete every buffer for a project scoped to `window_id`. Called
    /// before re-writing the snapshot on capture so a shrunken-pane
    /// layout doesn't leave orphaned rows behind. Other windows' rows
    /// for the same project are not affected.
    pub fn delete_for_project(
        &self,
        project_id: &str,
        window_id: &str,
    ) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM pane_buffers WHERE project_id = ?1 AND window_id = ?2",
                params![project_id, window_id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }
}

// Integration tests live in `tests/pane_buffer_repo.rs` so the migration
// ladder + ProjectRepo seam are exercised through the public crate surface.
