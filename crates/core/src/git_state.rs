//! Git working-tree state, parsed from `git status --porcelain=v2 --branch -z`.
//!
//! Lives in `oximux-core` (not `oximux-git`) so the app crate can depend on
//! these types without pulling in tokio + the git process layer. The git crate
//! owns the parser; this module owns the shapes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Snapshot of `git status` for one working tree.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    /// Current branch name. `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Configured upstream tracking ref, e.g. `origin/main`. None if unset.
    pub upstream: Option<String>,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// HEAD object id. `None` for an initial (no-commit) repo.
    pub head_oid: Option<String>,
    /// One entry per changed/untracked path.
    pub files: Vec<FileStatus>,
}

/// One changed path. Mirrors porcelain v2 record types 1, 2, u, ?.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    pub path: PathBuf,
    pub index: IndexStatus,
    pub worktree: WorktreeStatus,
    /// Populated for record type 2 (rename/copy).
    pub rename: Option<RenameInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameInfo {
    pub orig_path: PathBuf,
    pub kind: RenameKind,
    /// Similarity score 0..=100 reported by git.
    pub score: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenameKind {
    Rename,
    Copy,
}

/// Index-side status code from porcelain v2 (the X column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexStatus {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
    Unmerged,
}

/// Worktree-side status code from porcelain v2 (the Y column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeStatus {
    Unmodified,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Unmerged,
}

impl FileStatus {
    /// True if any change is staged in the index.
    pub fn is_staged(&self) -> bool {
        !matches!(
            self.index,
            IndexStatus::Unmodified | IndexStatus::Untracked | IndexStatus::Ignored,
        )
    }

    /// True if any change exists in the worktree (unstaged).
    pub fn is_unstaged(&self) -> bool {
        !matches!(self.worktree, WorktreeStatus::Unmodified)
    }
}
