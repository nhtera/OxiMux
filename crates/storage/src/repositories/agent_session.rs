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
                "INSERT INTO agent_sessions (id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title, last_message) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, NULL, NULL)",
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
            last_message: None,
        })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<AgentSession>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title, last_message \
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
                "SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title, last_message \
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

    /// Persist the agent's last assistant reply (captured on `Stop`) so a
    /// restored session keeps showing its finished-turn message in the rail.
    pub fn update_last_message(&self, id: &str, message: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE agent_sessions SET last_message = ?1 WHERE id = ?2",
                params![message, id],
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
    /// Re-adopt a quit-interrupted session on restore: atomically flip the
    /// most-recently-ended `interrupted` session for this `(workspace, adapter)`
    /// back to live (`idle`, no end/exit/detail) and return its id + original
    /// `started_at`, so the restored tab REUSES that row — keeping its persisted
    /// title and history — instead of orphaning it and inserting a duplicate.
    ///
    /// Ordered by `ended_at` DESC so the agents open at the last quit (freshly
    /// stamped by the boot sweep) are claimed before older interrupted history.
    /// The single-statement `UPDATE ... WHERE id = (SELECT ... LIMIT 1)
    /// RETURNING` is atomic, so concurrent per-tab restores each claim a
    /// distinct row. Returns `None` when nothing is claimable (first launch, or
    /// an untracked agent) — the caller then inserts a fresh row.
    pub fn claim_interrupted_for_restore(
        &self,
        workspace_id: &str,
        adapter_id: &str,
    ) -> Result<Option<(String, Option<String>)>, StorageError> {
        let claimed = self.db.with_conn(|c| {
            c.query_row(
                "UPDATE agent_sessions \
                   SET status = 'idle', exit_code = NULL, status_detail = NULL, ended_at = NULL \
                 WHERE id = ( \
                   SELECT id FROM agent_sessions \
                   WHERE workspace_id = ?1 AND adapter_id = ?2 AND status = 'interrupted' \
                   ORDER BY ended_at DESC, started_at DESC LIMIT 1 \
                 ) \
                 RETURNING id, started_at",
                params![workspace_id, adapter_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
        })?;
        Ok(claimed)
    }

    pub fn list_unfinished_at_shutdown(&self) -> Result<Vec<AgentSession>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, workspace_id, adapter_id, model, effort, status, exit_code, status_detail, started_at, ended_at, title, last_message \
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    fn repo() -> AgentSessionRepo {
        AgentSessionRepo::new(open_memory().expect("memory db"))
    }

    #[test]
    fn claim_reopens_interrupted_and_preserves_title() {
        let repo = repo();
        let s = repo.insert("ws-1", "claude-code", None, None).expect("insert");
        repo.update_title(&s.id, "fix the parser").expect("title");
        repo.mark_interrupted_at_shutdown(&s.id).expect("interrupt");

        // Restore claims the SAME row back to live.
        let (id, _started) = repo
            .claim_interrupted_for_restore("ws-1", "claude-code")
            .expect("claim ok")
            .expect("a row to claim");
        assert_eq!(id, s.id);

        // It is live again (idle, no end stamp) and KEEPS its persisted title.
        let row = repo.get_by_id(&s.id).expect("get").expect("row");
        assert_eq!(row.status, AgentStatus::Idle);
        assert_eq!(row.ended_at, None);
        assert_eq!(row.title.as_deref(), Some("fix the parser"));

        // Nothing left to claim → None (so the caller inserts a fresh row).
        assert!(
            repo.claim_interrupted_for_restore("ws-1", "claude-code")
                .expect("claim ok")
                .is_none()
        );
    }

    #[test]
    fn claim_only_matches_same_workspace_and_adapter() {
        let repo = repo();
        let s = repo.insert("ws-1", "claude-code", None, None).expect("insert");
        repo.mark_interrupted_at_shutdown(&s.id).expect("interrupt");
        // A different workspace or adapter never claims it.
        assert!(
            repo.claim_interrupted_for_restore("ws-2", "claude-code")
                .expect("ok")
                .is_none()
        );
        assert!(
            repo.claim_interrupted_for_restore("ws-1", "codex")
                .expect("ok")
                .is_none()
        );
        // The matching keys do.
        assert!(
            repo.claim_interrupted_for_restore("ws-1", "claude-code")
                .expect("ok")
                .is_some()
        );
    }

    #[test]
    fn claim_skips_non_interrupted_rows() {
        let repo = repo();
        // A live (idle) session is not claimable — only quit-interrupted rows.
        repo.insert("ws-1", "claude-code", None, None).expect("insert");
        assert!(
            repo.claim_interrupted_for_restore("ws-1", "claude-code")
                .expect("ok")
                .is_none()
        );
    }
}
