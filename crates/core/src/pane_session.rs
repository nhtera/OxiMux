//! Pane session domain type — one UI pane (terminal/editor/git/agent)
//! inside a workspace.
//!
//! Persisted in the `pane_sessions` SQLite table (V001). `grid_position`
//! is an opaque caller-controlled string (format set by the UI layer); the
//! storage crate treats it as a blob.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSession {
    pub id: String,
    pub workspace_id: String,
    /// Optional FK to `agent_sessions`. SQLite `ON DELETE SET NULL` —
    /// when an agent session row is deleted, this becomes `None` without
    /// removing the pane row.
    pub agent_session_id: Option<String>,
    pub shell_command: String,
    pub grid_position: String,
    pub log_path: Option<String>,
    pub created_at: String,
}
