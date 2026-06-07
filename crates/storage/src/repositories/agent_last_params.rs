//! `AgentLastParamsRepo` — last-used model + reasoning-effort per agent
//! adapter, so the launch picker can preselect the user's previous choice.
//! Keyed by the adapter slug (`claude-code`, `codex`, …). `model` / `effort`
//! are nullable: `None` means "adapter default, pass no flag".

use rusqlite::params;

use crate::db::Db;
use crate::error::StorageError;
use crate::repositories::now;

#[derive(Clone)]
pub struct AgentLastParamsRepo {
    db: Db,
}

impl AgentLastParamsRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Every saved row as `(adapter_id, model, effort)`. Used at boot to
    /// preload the picker's preselect cache.
    pub fn list_all(&self) -> Result<Vec<(String, Option<String>, Option<String>)>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT adapter_id, model, effort FROM agent_last_params")?;
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    /// Upsert the last-used params for one adapter slug.
    pub fn upsert(
        &self,
        adapter_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO agent_last_params (adapter_id, model, effort, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(adapter_id) DO UPDATE SET \
                   model = excluded.model, \
                   effort = excluded.effort, \
                   updated_at = excluded.updated_at",
                params![adapter_id, model, effort, ts],
            )
            .map(|_| ())
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[test]
    fn upsert_then_list_roundtrips() {
        let db = open_memory().expect("open_memory");
        let repo = AgentLastParamsRepo::new(db);
        repo.upsert("claude-code", Some("opus"), Some("high"))
            .expect("upsert");
        let all = repo.list_all().expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "claude-code");
        assert_eq!(all[0].1.as_deref(), Some("opus"));
        assert_eq!(all[0].2.as_deref(), Some("high"));
    }

    #[test]
    fn upsert_overwrites_existing_adapter() {
        let db = open_memory().expect("open_memory");
        let repo = AgentLastParamsRepo::new(db);
        repo.upsert("codex", Some("o3"), None).expect("first");
        repo.upsert("codex", Some("gpt-5-codex"), None)
            .expect("second");
        let all = repo.list_all().expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.as_deref(), Some("gpt-5-codex"));
    }

    #[test]
    fn null_params_roundtrip_as_none() {
        let db = open_memory().expect("open_memory");
        let repo = AgentLastParamsRepo::new(db);
        repo.upsert("aider", None, None).expect("upsert");
        let all = repo.list_all().expect("list");
        assert_eq!(all[0].1, None);
        assert_eq!(all[0].2, None);
    }
}
