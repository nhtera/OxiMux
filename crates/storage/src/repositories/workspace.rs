//! `WorkspaceRepo` — typed CRUD over the `workspaces` table. The
//! `(project_id, slug)` UNIQUE constraint is the foot-gun guard for the
//! step-6 worktree-add rollback flow.

use oximux_core::Workspace;
use rusqlite::{OptionalExtension, params};

use super::{classify_unique, new_id, now};
use crate::db::Db;
use crate::error::StorageError;
use crate::model::WorkspaceRow;

#[derive(Clone)]
pub struct WorkspaceRepo {
    db: Db,
}

impl WorkspaceRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Insert a new workspace. Returns [`StorageError::Conflict`] when the
    /// `(project_id, slug)` pair already exists — callers (step 6) should
    /// catch this before invoking `git worktree add` to avoid a half-baked
    /// state.
    pub fn insert(
        &self,
        project_id: &str,
        name: &str,
        slug: &str,
        branch: &str,
        worktree_path: &str,
    ) -> Result<Workspace, StorageError> {
        let id = new_id();
        let created_at = now();
        let status = "active";
        self.db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO workspaces (id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                    params![id, project_id, name, slug, branch, worktree_path, status, created_at],
                )
            })
            .map_err(|e| classify_unique("workspaces", "project_id_slug", e))?;
        Ok(Workspace {
            id,
            project_id: project_id.to_string(),
            name: name.to_string(),
            slug: slug.to_string(),
            branch: branch.to_string(),
            worktree_path: worktree_path.to_string(),
            status: status.to_string(),
            created_at,
            archived_at: None,
        })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<Workspace>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at \
                 FROM workspaces WHERE id = ?1",
                [id],
                WorkspaceRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// List active (non-archived) workspaces for a project, newest first.
    pub fn list_for_project(&self, project_id: &str) -> Result<Vec<Workspace>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, project_id, name, slug, branch, worktree_path, status, created_at, archived_at \
                 FROM workspaces \
                 WHERE project_id = ?1 AND archived_at IS NULL \
                 ORDER BY created_at DESC",
            )?;
            let iter = stmt.query_map([project_id], WorkspaceRow::from_row)?;
            iter.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub fn mark_archived(&self, id: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET archived_at = ?1, status = 'archived' WHERE id = ?2",
                params![ts, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    pub fn rename(&self, id: &str, new_name: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE workspaces SET name = ?1 WHERE id = ?2",
                params![new_name, id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Delete a workspace row. FK cascade removes `pane_sessions` and
    /// `agent_sessions` for the workspace. Call this in the error branch
    /// of `git worktree add` to keep DB and disk state consistent.
    pub fn delete(&self, id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM workspaces WHERE id = ?1", [id])
                .map(|_| ())
        })?;
        Ok(())
    }
}
