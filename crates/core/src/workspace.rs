//! Workspace domain type — one git worktree per task within a project.
//!
//! Persisted in the `workspaces` SQLite table (V001). `UNIQUE(project_id,
//! slug)` is enforced by the schema, not this type.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub slug: String,
    pub branch: String,
    pub worktree_path: String,
    /// `"active"` or `"archived"`. String column for forward extensibility
    /// without a schema migration.
    pub status: String,
    pub created_at: String,
    pub archived_at: Option<String>,
}

/// Per-worktree SCM scratch state, persisted in the V006
/// `worktree_settings` SQLite table keyed by `workspace_id`.
///
/// Every field is optional — `None` means "fall back to the global
/// default" (e.g. the SCM panel's `view_mode` setting, the repo's
/// default branch as the diff base, an empty composer textarea). The
/// row only exists once at least one field has been set; readers MUST
/// treat a missing row as `WorktreeSettings::default()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSettings {
    /// Base ref the diff view / dropdown / graph compare against. `None`
    /// = the repo's default branch as resolved by `git symbolic-ref`.
    pub base_ref: Option<String>,
    /// Inline-composer textarea contents persisted across panel re-mounts
    /// and app restarts. Cleared by the commit completion hook (Phase 07).
    pub commit_draft: Option<String>,
    /// Per-worktree override of the global `scm_view_mode` setting (e.g.
    /// `"list"` / `"tree"`). `None` = inherit from global settings.
    pub view_mode_override: Option<String>,
}
