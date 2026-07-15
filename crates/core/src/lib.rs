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
pub mod conflict_kind;
pub mod diff_review_note;
pub mod git_diff;
pub mod git_ops;
pub mod git_state;
pub mod pane_session;
pub mod pr_state;
pub mod project;
pub mod session_resumption;
pub mod workspace;

pub use agent_session::{
    AgentSession, AgentSessionId, AgentSidebandState, AgentSnapshot, AgentStatus, SidebandDetail,
};
pub use conflict_kind::ConflictKind;
pub use diff_review_note::{DiffReviewNote, NoteSide};
pub use git_diff::{
    ChangeRegion, CombinedDiff, CombinedDiffScope, DiffHunk, DiffLine, DiffLineKind, DiffStatus,
    FileDiff, FileGroup, HUNK_CONTEXT, LARGE_DIFF_LINE_THRESHOLD, change_regions,
};
pub use git_ops::{BranchInfo, GitOperation, MergeOutcome, StashEntry, StashRef, WorktreeInfo};
pub use git_state::{
    BranchCommittedFile, BranchRange, CommitInfo, FileStatus, GitState, IndexStatus, RefLabel,
    RenameInfo, RenameKind, WorktreeStatus,
};
pub use pane_session::PaneSession;
pub use pr_state::{ForgeRefKind, PrState};
pub use project::Project;
pub use session_resumption::SessionResumption;
pub use workspace::{ViewMode, Workspace, WorktreeSettings};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AgentAdapter {
    #[default]
    ClaudeCode,
    Codex,
    Aider,
    /// The `pi` CLI. A TUI in a PTY like the others, and additionally a chat
    /// backend of its own (`pi --mode rpc`) — the same dual nature Claude and
    /// Codex have.
    Pi,
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
