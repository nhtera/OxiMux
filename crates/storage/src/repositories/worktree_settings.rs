//! `WorktreeSettingsRepo` — typed CRUD over the V006 `worktree_settings`
//! table (one optional SCM scratch row per workspace).
//!
//! All payload columns are nullable; `get` returns `None` when no row
//! exists at all (callers MUST treat that as `WorktreeSettings::default()`).
//! `upsert` writes every field unconditionally — callers who want to
//! merge against existing state should `get` first.

use oximux_core::WorktreeSettings;
use rusqlite::{OptionalExtension, params};

use super::now;
use crate::db::Db;
use crate::error::StorageError;
use crate::model::WorktreeSettingsRow;

#[derive(Clone)]
pub struct WorktreeSettingsRepo {
    db: Db,
}

impl WorktreeSettingsRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Fetch the row for `workspace_id`. `Ok(None)` when no row exists;
    /// the caller is expected to substitute `WorktreeSettings::default()`.
    pub fn get(&self, workspace_id: &str) -> Result<Option<WorktreeSettings>, StorageError> {
        let row = self.db.with_conn(|c| {
            c.query_row(
                "SELECT workspace_id, base_ref, commit_draft, view_mode_override, updated_at \
                 FROM worktree_settings WHERE workspace_id = ?1",
                [workspace_id],
                WorktreeSettingsRow::from_row,
            )
            .optional()
        })?;
        Ok(row.map(Into::into))
    }

    /// Insert or replace the row for `workspace_id`. Every field is
    /// written unconditionally; pass through the result of `get` if you
    /// want field-level merging.
    pub fn upsert(
        &self,
        workspace_id: &str,
        settings: &WorktreeSettings,
    ) -> Result<(), StorageError> {
        let updated_at = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO worktree_settings \
                     (workspace_id, base_ref, commit_draft, view_mode_override, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(workspace_id) DO UPDATE SET \
                     base_ref = excluded.base_ref, \
                     commit_draft = excluded.commit_draft, \
                     view_mode_override = excluded.view_mode_override, \
                     updated_at = excluded.updated_at",
                params![
                    workspace_id,
                    settings.base_ref,
                    settings.commit_draft,
                    settings.view_mode_override,
                    updated_at,
                ],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Remove the row for `workspace_id`. Returns `Ok(())` when no row
    /// existed (consistent with other repos' missing-row contract).
    pub fn delete(&self, workspace_id: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "DELETE FROM worktree_settings WHERE workspace_id = ?1",
                [workspace_id],
            )
            .map(|_| ())
        })?;
        Ok(())
    }
}
