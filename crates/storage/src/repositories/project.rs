//! `ProjectRepo` — typed CRUD over the `projects` table.

use oximux_core::Project;
use rusqlite::{OptionalExtension, params};

use super::{classify_unique, new_id, now};
use crate::db::Db;
use crate::error::StorageError;
use crate::model::ProjectRow;

#[derive(Clone)]
pub struct ProjectRepo {
    db: Db,
}

impl ProjectRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn insert(
        &self,
        name: &str,
        root_path: &str,
        default_branch: &str,
    ) -> Result<Project, StorageError> {
        let id = new_id();
        let created_at = now();
        self.db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO projects (id, name, root_path, default_branch, created_at, last_opened_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                    params![id, name, root_path, default_branch, created_at],
                )
            })
            .map_err(|e| classify_unique("projects", "root_path", e))?;
        Ok(Project {
            id,
            name: name.to_string(),
            root_path: root_path.to_string(),
            default_branch: default_branch.to_string(),
            created_at,
            last_opened_at: None,
        })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<Project>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at \
                 FROM projects WHERE id = ?1",
                [id],
                ProjectRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// Lookup by absolute path. Used by the project picker's
    /// conflict-then-touch flow when a user re-opens a project older
    /// than the `list_recent(20)` horizon (so the in-memory scan misses).
    pub fn get_by_root_path(&self, root_path: &str) -> Result<Option<Project>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at \
                 FROM projects WHERE root_path = ?1",
                [root_path],
                ProjectRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// Insert a new project, or — on duplicate `root_path` — silently
    /// touch the existing row's `last_opened_at` and return it.
    ///
    /// Used by the project picker (step 5) so the "Open Folder…" flow
    /// re-promotes already-known paths to the top of the recent list
    /// without surfacing a Conflict to the user. Two SQL writes in the
    /// duplicate case: a failed INSERT (caught as Conflict) then an
    /// UPDATE — acceptable for an interactive-only path.
    pub fn insert_or_touch(
        &self,
        name: &str,
        root_path: &str,
        default_branch: &str,
    ) -> Result<Project, StorageError> {
        match self.insert(name, root_path, default_branch) {
            Ok(project) => Ok(project),
            Err(StorageError::Conflict {
                table, constraint, ..
            }) if table == "projects" && constraint == "root_path" => {
                // Re-fetch existing row; this is the canonical home for
                // the conflict→touch flow so consumers never see SQL.
                let existing = self
                    .get_by_root_path(root_path)?
                    .ok_or(StorageError::Conflict {
                        table: "projects".into(),
                        constraint: "root_path".into(),
                    })?;
                self.update_last_opened_at(&existing.id)?;
                Ok(existing)
            }
            Err(other) => Err(other),
        }
    }

    pub fn list_recent(&self, limit: usize) -> Result<Vec<Project>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, root_path, default_branch, created_at, last_opened_at \
                 FROM projects \
                 ORDER BY last_opened_at IS NULL, last_opened_at DESC, created_at DESC \
                 LIMIT ?1",
            )?;
            let iter = stmt.query_map([limit as i64], ProjectRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub fn update_last_opened_at(&self, id: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE projects SET last_opened_at = ?1 WHERE id = ?2",
                params![ts, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM projects WHERE id = ?1", [id])
                .map(|_| ())
        })?;
        Ok(())
    }
}
