//! `AgentSessionRepo` — typed CRUD over the `agent_sessions` table. The
//! three-column codec (`status`, `exit_code`, `status_detail`) is owned
//! by `AgentStatus::{as_str, exit_code_for_storage, detail_for_storage,
//! from_row}` in `oximux-core`; this repo only plumbs the values.

use oximux_core::{AgentSession, AgentStatus};
use rusqlite::{OptionalExtension, params};

use super::{new_id, now};
use crate::db::Db;
use crate::error::StorageError;
use crate::model::AgentSessionRow;

#[derive(Clone)]
pub struct AgentSessionRepo {
    db: Db,
}

impl AgentSessionRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn insert(
        &self,
        workspace_id: &str,
        adapter_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<AgentSession, StorageError> {
        let id = new_id();
        let started_at = now();
        let status = AgentStatus::Idle;
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO agent_sessions (id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, NULL)",
                params![
                    id,
                    workspace_id,
                    adapter_id,
                    model,
                    effort,
                    status.as_str(),
                    started_at,
                ],
            )
            .map(|_| ())
        })?;
        Ok(AgentSession {
            id,
            workspace_id: workspace_id.to_string(),
            adapter_id: adapter_id.to_string(),
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            status,
            started_at: Some(started_at),
            ended_at: None,
            title: None,
        })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<AgentSession>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title \
                 FROM agent_sessions WHERE id = ?1",
                [id],
                AgentSessionRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    pub fn list_for_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentSession>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title \
                 FROM agent_sessions WHERE workspace_id = ?1 \
                 ORDER BY started_at DESC",
            )?;
            let iter = stmt.query_map([workspace_id], AgentSessionRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Encode an [`AgentStatus`] into the three columns and write it.
    pub fn update_status(&self, id: &str, status: &AgentStatus) -> Result<(), StorageError> {
        let slug = status.as_str();
        let exit_code = status.exit_code_for_storage();
        let detail = status.detail_for_storage();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET status = ?1, exit_code = ?2, status_detail = ?3 WHERE id = ?4",
                params![slug, exit_code, detail, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Persist the agent's title (its most recent prompt). Written as the turn
    /// progresses so a restored session keeps its rail title across a restart.
    pub fn update_title(&self, id: &str, title: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET title = ?1 WHERE id = ?2",
                params![title, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    pub fn update_ended_at(&self, id: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET ended_at = ?1 WHERE id = ?2",
                params![ts, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Mark a session interrupted by a quit/shutdown AND stamp `ended_at` in one
    /// write, returning the timestamp. The boot sweep loses the live process, so
    /// `update_status` alone would leave `ended_at` NULL — making the row look
    /// infinitely old, so the rail's recency cutoff would cull it and lose the
    /// agent's persisted title. Stamping the shutdown time keeps the session as
    /// recent "sleeping" history so it (and its title) survive the restart.
    pub fn mark_interrupted_at_shutdown(&self, id: &str) -> Result<String, StorageError> {
        let status = AgentStatus::Interrupted;
        let slug = status.as_str();
        let exit_code = status.exit_code_for_storage();
        let detail = status.detail_for_storage();
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET status = ?1, exit_code = ?2, status_detail = ?3, ended_at = ?4 WHERE id = ?5",
                params![slug, exit_code, detail, ts, id],
            )
            .map(|_| ())
        })?;
        Ok(ts)
    }

    /// Sessions in any NON-TERMINAL status with no `ended_at` — i.e. they
    /// were alive when the app went down. The startup path calls
    /// `update_status(id, AgentStatus::Interrupted)` on each.
    ///
    /// Matching every non-terminal slug (not just `'running'`) matters
    /// because the status machine decays `Running` → `Idle` after output
    /// silence: an agent sitting at its prompt when the app dies leaves an
    /// `idle` row, which a running-only sweep would orphan forever.
    ///
    /// The literals MUST stay in lockstep with `AgentStatus::as_str` and
    /// `AgentStatus::is_terminal`; the codec test
    /// `agent_status_non_terminal_slugs_match_query` (in
    /// `crates/storage/tests/agent_status_codec.rs`) machine-checks the
    /// link.
    pub fn list_unfinished_at_shutdown(&self) -> Result<Vec<AgentSession>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title \
                 FROM agent_sessions \
                 WHERE status IN ('idle', 'running', 'waiting_input', 'needs_approval') \
                   AND ended_at IS NULL \
                 ORDER BY started_at ASC",
            )?;
            let iter = stmt.query_map([], AgentSessionRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}
