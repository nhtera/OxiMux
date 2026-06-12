//! Git working-tree state, parsed from `git status --porcelain=v2 --branch -z`.
//!
//! Lives in `oximux-core` (not `oximux-git`) so the app crate can depend on
//! these types without pulling in tokio + the git process layer. The git crate
//! owns the parser; this module owns the shapes.

use crate::ConflictKind;
use crate::DiffStatus;
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
    /// Files changed by commits on this branch that aren't on its base
    /// (the "Committed on Branch" section): the aggregate of
    /// `merge_base(base, HEAD)..HEAD`. Empty when no base resolves or the
    /// branch is level with its base. Enriched onto the status poll, so a
    /// computation failure leaves it empty rather than poisoning status.
    #[serde(default)]
    pub branch_committed: Vec<BranchCommittedFile>,
    /// The revision range `branch_committed` was computed against — needed
    /// to open a per-file range diff. `None` when no base resolved.
    #[serde(default)]
    pub branch_range: Option<BranchRange>,
}

/// One file in the "Committed on Branch" section — the net change a file
/// received across the branch's unpushed commits, summarised like a
/// status row (one status code + `+added −removed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCommittedFile {
    /// New path (post-change); for renames the destination, origin in
    /// `status`.
    pub path: PathBuf,
    pub status: DiffStatus,
    pub added: u32,
    pub removed: u32,
}

/// The revision range the branch-committed file list spans. Carries the
/// resolved base ref (for display) plus the concrete `merge_base..head`
/// OIDs a per-file range diff is computed against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRange {
    /// Resolved base ref, e.g. `origin/main` — display + provenance.
    pub base_ref: String,
    /// `merge-base(base_ref, HEAD)` OID — the left side of the range.
    pub merge_base: String,
    /// HEAD OID — the right side of the range.
    pub head: String,
}

/// One changed path. Mirrors porcelain v2 record types 1, 2, u, ?.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    pub path: PathBuf,
    pub index: IndexStatus,
    pub worktree: WorktreeStatus,
    /// Populated for record type 2 (rename/copy).
    pub rename: Option<RenameInfo>,
    /// UNSTAGED `(added, removed)` line counts (worktree vs index), from
    /// `git diff --numstat`. For untracked files this is the whole-file
    /// line count when the lazy untracked pass has produced one. `None`
    /// when the path has no countable unstaged diff (binary, mode-only,
    /// fully staged, fresh repo without HEAD).
    pub line_counts: Option<(u32, u32)>,
    /// STAGED `(added, removed)` line counts (index vs HEAD), from
    /// `git diff --cached --numstat`. Drives the Staged-section row badge
    /// so a partially staged file shows what would actually be committed.
    /// `serde(default)` keeps previously persisted snapshots loadable.
    #[serde(default)]
    pub staged_line_counts: Option<(u32, u32)>,
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
            staged_line_counts: None,
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

/// One row in the commit graph. Parsed from a NUL-terminated `git log
/// -z --pretty=format:<US-joined fields>` record (see `oximux_git::log`
/// for the exact format string).
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
    /// Parsed `%D` decorate field — branch / tag / HEAD ref labels
    /// attached to this commit. Empty when no refs decorate the commit
    /// (typical for older commits in the history). Order matches git's
    /// emission so the "HEAD -> branch" pair (when present) lands as
    /// `[Head, BranchTip { name }, ...]` and the UI can foreground the
    /// HEAD chip.
    #[serde(default)]
    pub refs: Vec<RefLabel>,
}

/// One ref pointing at a commit. Modeled to mirror the decorate
/// formats `git log` emits when given `--decorate` (which is the
/// default for `%D` on tty; `%D` itself works regardless).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefLabel {
    /// Bare `HEAD` — emitted only when HEAD is detached. The
    /// attached-HEAD case shows as `HEAD -> {branch}` which the parser
    /// represents as `Head` followed by `BranchTip { name }` in the
    /// emission order; renderers can collapse those two into a single
    /// "HEAD on {branch}" chip.
    Head,
    /// Local branch tip — `main`, `feature/x`, etc.
    BranchTip { name: String },
    /// Remote-tracking branch tip — `origin/main`, `upstream/dev`, etc.
    RemoteBranch { name: String },
    /// Annotated or lightweight tag — `tag: v1.2.3`.
    Tag { name: String },
    /// Anything `%D` emits that doesn't fit the above (e.g.
    /// `refs/stash`, `refs/notes/commits`). Keeps the raw label so
    /// renderers can still show it, just without specialized styling.
    Other { raw: String },
}
