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
