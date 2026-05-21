//! `PaneBufferRepo` — per-pane scrollback BLOB storage backing the
//! terminal-tab restore path (Phase 4 step 16). One row per leaf inside a
//! project's persisted tab snapshot, keyed by `(project_id, ordinal)`.
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

    /// Upsert one pane's bytes at the given ordinal.
    pub fn set(
        &self,
        project_id: &str,
        ordinal: u32,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO pane_buffers (project_id, ordinal, bytes, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(project_id, ordinal) DO UPDATE SET \
                    bytes = excluded.bytes, updated_at = excluded.updated_at",
                params![project_id, ordinal, bytes, ts],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Fetch every buffer for a project, ordered by `ordinal` ascending.
    /// Caller pairs the returned bytes with leaves in the matching DFS
    /// order produced by the tab restore path.
    pub fn get_all_for_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<(u32, Vec<u8>)>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT ordinal, bytes FROM pane_buffers \
                 WHERE project_id = ?1 ORDER BY ordinal ASC",
            )?;
            let rows = stmt.query_map([project_id], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok::<_, rusqlite::Error>(out)
        })?;
        Ok(rows)
    }

    /// Delete every buffer for a project. Called before re-writing the
    /// snapshot on capture so a shrunken-pane layout doesn't leave
    /// orphaned rows behind.
    pub fn delete_for_project(&self, project_id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM pane_buffers WHERE project_id = ?1",
                params![project_id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }
}

// Integration tests live in `tests/pane_buffer_repo.rs` so the migration
// ladder + ProjectRepo seam are exercised through the public crate surface.
