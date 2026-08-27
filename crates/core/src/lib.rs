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
pub use diff_review_note::{
    DiffReviewNote, NoteSide, anchor_text_is_checkable, anchor_text_matches,
    normalize_anchor_text,
};
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
pub use workspace::{ViewMode, WorkPhase, Workspace, WorktreeSettings};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AgentAdapter {
    #[default]
    ClaudeCode,
    Codex,
    /// The `pi` CLI. A TUI in a PTY like the others, and additionally a chat
    /// backend of its own (`pi --mode rpc`) — the same dual nature Claude and
    /// Codex have.
    ///
    /// The alias keeps tab-restore blobs written before the `Aider` variant
    /// was retired parsing; those tabs rehydrate as Pi (its successor in the
    /// built-in roster) instead of failing the whole snapshot.
    #[serde(alias = "Aider")]
    Pi,
    /// The `omp` CLI (a Pi fork). Same dual nature: a TUI in a PTY, and a
    /// chat backend of its own (`omp --mode rpc-ui`). Additive variant —
    /// this enum never crosses the postcard wire, so appending is safe
    /// (verified during the Aider retirement).
    Omp,
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

#[cfg(test)]
mod agent_adapter_tests {
    use super::AgentAdapter;

    /// Tab-restore blobs written while the retired `Aider` variant existed
    /// carry `"Aider"` as the serialized variant name. The alias on `Pi`
    /// must keep those blobs parsing (as Pi, its roster successor) instead
    /// of failing the whole snapshot.
    #[test]
    fn legacy_aider_blob_deserializes_as_pi() {
        let a: AgentAdapter = serde_json::from_str("\"Aider\"").expect("legacy blob parses");
        assert_eq!(a, AgentAdapter::Pi);
    }

    #[test]
    fn omp_round_trips_and_never_collides_with_pi() {
        let s = serde_json::to_string(&AgentAdapter::Omp).expect("serialize");
        assert_eq!(s, "\"Omp\"");
        let back: AgentAdapter = serde_json::from_str(&s).expect("parse");
        assert_eq!(back, AgentAdapter::Omp);
    }

    #[test]
    fn pi_still_round_trips_as_pi() {
        let s = serde_json::to_string(&AgentAdapter::Pi).expect("serialize");
        assert_eq!(s, "\"Pi\"", "the alias must not change what Pi serializes as");
        let back: AgentAdapter = serde_json::from_str(&s).expect("parse");
        assert_eq!(back, AgentAdapter::Pi);
    }
}
