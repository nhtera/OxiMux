//! Git working-tree state, parsed from `git status --porcelain=v2 --branch -z`.
//!
//! Lives in `oximux-core` (not `oximux-git`) so the app crate can depend on
//! these types without pulling in tokio + the git process layer. The git crate
//! owns the parser; this module owns the shapes.

use crate::ConflictKind;
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
    /// `(added, removed)` line counts vs `HEAD`. Populated by merging
    /// `git diff --numstat HEAD` into the porcelain v2 file list. `None`
    /// when the path has no countable diff (binary, mode-only, untracked,
    /// fresh repo without HEAD).
    pub line_counts: Option<(u32, u32)>,
    /// Three-way-merge conflict classification — `Some` only for `u`
    /// records whose XY pair maps to one of the seven legal conflict
    /// codes (`DD`, `AU`, `UD`, `UA`, `DU`, `AA`, `UU`).
    pub conflict_kind: Option<ConflictKind>,
}

impl FileStatus {
    /// Construct a `FileStatus` for a non-rename, non-conflict, no-numstat
    /// path. Phase-02 helper that keeps the dozens of test / sample sites
    /// out of `{rename: None, line_counts: None, conflict_kind: None}`
    /// boilerplate.
    pub fn with_status(path: PathBuf, index: IndexStatus, worktree: WorktreeStatus) -> Self {
        Self {
            path,
            index,
            worktree,
            rename: None,
            line_counts: None,
            conflict_kind: None,
        }
    }
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

/// One row in the commit graph (phase-05). Parsed from a NUL-terminated
/// `git log -z --pretty=format:<US-joined fields>` record (see
/// `oximux_git::log` for the exact format string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    /// Full 40-char SHA.
    pub oid: String,
    /// 7-char short SHA.
    pub short_oid: String,
    pub subject: String,
    pub author: String,
    /// Short author date — e.g. "May 20". Format-locale-stable via
    /// `GitCmd`'s `LANG=C` and explicit `--date=format-local:%b %d`.
    pub short_date: String,
    /// Full commit message body (everything after the subject + blank line).
    /// Empty string when the commit has no body. Newlines preserved so the
    /// UI can render paragraphs as written.
    pub body: String,
}
