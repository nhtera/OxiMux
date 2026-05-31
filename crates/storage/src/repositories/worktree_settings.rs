//! `WorktreeSettingsRepo` — typed CRUD over the V006 `worktree_settings`
//! table (one optional SCM scratch row per workspace).
//!
//! All payload columns are nullable; `get` returns `None` when no row
//! exists at all (callers MUST treat that as `WorktreeSettings::default()`).
//! `upsert` writes every field unconditionally — callers who want to
//! merge against existing state should `get` first.

use oximux_core::WorktreeSettings;
use rusqlite::{Connection, OptionalExtension, params};

use super::now;
use crate::db::Db;
use crate::error::StorageError;
use crate::model::WorktreeSettingsRow;

/// Single upsert statement shared by [`WorktreeSettingsRepo::upsert`]
/// (one `with_conn`) and [`WorktreeSettingsRepo::modify`] (the write
/// half of the fused read-modify-write). Centralised so the SQL never
/// drifts between the two paths.
fn upsert_row(
    conn: &Connection,
    workspace_id: &str,
    settings: &WorktreeSettings,
    updated_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
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
}

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
    /// want field-level merging — or prefer [`modify`](Self::modify),
    /// which fuses the read and write under one connection lock.
    pub fn upsert(
        &self,
        workspace_id: &str,
        settings: &WorktreeSettings,
    ) -> Result<(), StorageError> {
        let updated_at = now();
        self.db.with_conn(|c| {
            upsert_row(c, workspace_id, settings, &updated_at)
        })?;
        Ok(())
    }

    /// Atomic read-modify-write of the row for `workspace_id`.
    ///
    /// Holds the connection mutex for the FULL read + mutate + write
    /// window, so two concurrent writers (`set_base_ref`,
    /// `cycle_view_mode`, `commit_draft` debounce) cannot interleave a
    /// stale read between each other's mutate and write.
    ///
    /// `f` receives `WorktreeSettings::default()` when no row exists
    /// yet and an INSERT is performed; otherwise the existing row is
    /// loaded, passed to `f`, and re-written via the same `ON CONFLICT`
    /// upsert as [`upsert`](Self::upsert) — so sibling fields the
    /// caller doesn't touch survive untouched.
    ///
    /// Cost: one extra `SELECT` per write vs. a bare `upsert`. The
    /// caller is expected to NOT do its own `get` before calling this
    /// (that would re-introduce the very race this method closes).
    pub fn modify<F>(&self, workspace_id: &str, f: F) -> Result<(), StorageError>
    where
        F: FnOnce(&mut WorktreeSettings),
    {
        let updated_at = now();
        self.db.with_conn(|c| {
            let mut settings: WorktreeSettings = c
                .query_row(
                    "SELECT workspace_id, base_ref, commit_draft, view_mode_override, updated_at \
                     FROM worktree_settings WHERE workspace_id = ?1",
                    [workspace_id],
                    WorktreeSettingsRow::from_row,
                )
                .optional()?
                .map(Into::into)
                .unwrap_or_default();
            f(&mut settings);
            upsert_row(c, workspace_id, &settings, &updated_at)
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
