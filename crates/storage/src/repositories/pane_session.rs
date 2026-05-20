//! `PaneSessionRepo` — typed CRUD over the `pane_sessions` table.
//! `grid_position` is an opaque caller-controlled string; format is set
//! by the UI layer (step 8).

use oximux_core::PaneSession;
use rusqlite::{OptionalExtension, params};

use super::{new_id, now};
use crate::db::Db;
use crate::error::StorageError;
use crate::model::PaneSessionRow;

#[derive(Clone)]
pub struct PaneSessionRepo {
    db: Db,
}

impl PaneSessionRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn insert(
        &self,
        workspace_id: &str,
        shell_command: &str,
        grid_position: &str,
        log_path: Option<&str>,
    ) -> Result<PaneSession, StorageError> {
        let id = new_id();
        let created_at = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO pane_sessions (id, workspace_id, agent_session_id, shell_command, grid_position, log_path, created_at) \
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)",
                params![id, workspace_id, shell_command, grid_position, log_path, created_at],
            )
            .map(|_| ())
        })?;
        Ok(PaneSession {
            id,
            workspace_id: workspace_id.to_string(),
            agent_session_id: None,
            shell_command: shell_command.to_string(),
            grid_position: grid_position.to_string(),
            log_path: log_path.map(str::to_string),
            created_at,
        })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<PaneSession>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, workspace_id, agent_session_id, shell_command, grid_position, log_path, created_at \
                 FROM pane_sessions WHERE id = ?1",
                [id],
                PaneSessionRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    pub fn list_for_workspace(&self, workspace_id: &str) -> Result<Vec<PaneSession>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, workspace_id, agent_session_id, shell_command, grid_position, log_path, created_at \
                 FROM pane_sessions WHERE workspace_id = ?1 \
                 ORDER BY created_at ASC",
            )?;
            let iter = stmt.query_map([workspace_id], PaneSessionRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub fn update_grid_position(&self, id: &str, grid_position: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE pane_sessions SET grid_position = ?1 WHERE id = ?2",
                params![grid_position, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM pane_sessions WHERE id = ?1", [id])
                .map(|_| ())
        })?;
        Ok(())
    }
}
