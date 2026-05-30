//! oximux-core
//!
//! Domain types shared across the workspace: Project, Workspace,
//! PaneSession, AgentSession. Kept dependency-free beyond serde/thiserror
//! so any other crate can depend on it without pulling in GPUI, tokio, or
//! platform code.
//!
//! Persisted ids are `String` UUIDs (generated storage-side via the
//! `uuid` crate); runtime-transient handles live in their own newtypes
//! (e.g. `AgentSessionId`) and are never confused with persisted ids.

pub mod agent_session;
pub mod git_diff;
pub mod git_ops;
pub mod git_state;
pub mod pane_session;
pub mod project;
pub mod workspace;

pub use agent_session::{AgentSession, AgentSessionId, AgentStatus};
pub use git_diff::{
    DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff, LARGE_DIFF_LINE_THRESHOLD,
};
pub use git_ops::{BranchInfo, MergeOutcome, StashEntry, StashRef, WorktreeInfo};
pub use git_state::{
    CommitInfo, FileStatus, GitState, IndexStatus, RenameInfo, RenameKind, WorktreeStatus,
};
pub use pane_session::PaneSession;
pub use project::Project;
pub use workspace::{Workspace, WorktreeSettings};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentAdapter {
    ClaudeCode,
    Codex,
    Aider,
    /// Arbitrary shell command launched in a PTY. The program + args travel
    /// on `AgentSessionConfig::custom_command`; status detection falls back
    /// to the StatusMachine's Idle/Running/exit-code defaults since there
    /// is no canonical prompt pattern.
    Custom,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}
