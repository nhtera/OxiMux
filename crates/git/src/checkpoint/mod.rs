//! Per-turn repository checkpoints for the agent chat rewind feature.
//!
//! A checkpoint is a **dangling commit object**: the current worktree content
//! (including untracked files, minus oversized ones) written through a
//! temporary index and `commit-tree`, with **no ref created**. Invisible to
//! every normal git surface (`git log --all`, branches, reflog); kept alive by
//! git's gc grace period (`gc.pruneExpire`, ~2 weeks by default), after which
//! restore reports [`CheckpointError::ObjectMissing`] and callers degrade
//! gracefully.
//!
//! The user's real index, HEAD, branches, and staged state are never touched.

mod checkpoint_engine;
mod checkpoint_exclude;
mod checkpoint_temp_index;

pub use checkpoint_engine::{CheckpointEngine, CheckpointSha};

use crate::error::GitError;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CheckpointError {
    /// git is too old for `git restore` (< 2.23) — feature disabled.
    #[error("git {version} does not support checkpoints (need >= 2.23)")]
    Unsupported { version: String },

    /// The checkpoint commit object was pruned by git gc.
    #[error("checkpoint object no longer exists (pruned by git gc)")]
    ObjectMissing,

    /// Underlying git invocation failed.
    #[error(transparent)]
    Git(#[from] GitError),

    /// Filesystem work around the git dir (temp index copy, exclude edit).
    #[error("checkpoint io: {0}")]
    Io(String),
}

impl CheckpointError {
    pub(crate) fn io(e: impl std::fmt::Display) -> Self {
        Self::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CheckpointError>;
