//! oximux-core
//!
//! Domain types shared across the workspace: Project, Workspace, Pane, AgentSession.
//! Kept dependency-free beyond serde/thiserror so any other crate can depend on it
//! without pulling in GPUI, tokio, or platform code.
//!
//! Phase 0: skeleton only. Real types land in Phase 1+ as call sites need them.

pub mod agent_session;
pub mod git_diff;
pub mod git_ops;
pub mod git_state;

pub use agent_session::{AgentSessionId, AgentStatus};
pub use git_diff::{
    DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff, LARGE_DIFF_LINE_THRESHOLD,
};
pub use git_ops::{BranchInfo, MergeOutcome, StashEntry, StashRef, WorktreeInfo};
pub use git_state::{
    CommitInfo, FileStatus, GitState, IndexStatus, RenameInfo, RenameKind, WorktreeStatus,
};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Stable identifier for any persisted entity. SQLite primary key in Phase 4.
pub type Id = u64;

/// A project rooted at a directory. One project may host many workspaces
/// (one per worktree / per task).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub root: PathBuf,
}

/// A workspace = one worktree + the panes the user has opened against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Id,
    pub project_id: Id,
    pub name: String,
    pub worktree_path: PathBuf,
}

/// A pane inside a workspace. PaneKind lets the dock layout multiplex
/// terminal / editor / git / agent surfaces inside one tile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pane {
    pub id: Id,
    pub workspace_id: Id,
    pub kind: PaneKind,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneKind {
    Terminal,
    Editor,
    Git,
    Agent,
}

/// An agent session bound to a pane. Implementation lives in `oximux-agents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: Id,
    pub pane_id: Id,
    pub adapter: AgentAdapter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentAdapter {
    ClaudeCode,
    Codex,
    Aider,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}
