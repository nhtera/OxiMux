//! Domain types for stash/branch/worktree/merge ops. Plain data — no IO,
//! no git invocation. Lives in `oximux-core` so UI layers can hold these
//! without depending on `oximux-git`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Opaque reference to a stash entry. The `index` field is the `N` in
/// `stash@{N}` at the time the ref was minted.
///
/// **v1 caveat:** the index is only stable while no other process mutates the
/// stash stack. v1 is single-user (OxiMux + the user's terminal), so concurrent
/// stash ops are unlikely; callers that hold a `StashRef` across a long
/// suspension should re-fetch via [`stash_list`](crate::git_ops) before use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashRef {
    pub index: usize,
}

impl StashRef {
    /// Renders as `stash@{N}` for passing to git commands.
    pub fn ref_string(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

/// One entry from `git stash list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashEntry {
    pub stash_ref: StashRef,
    /// Branch active when the stash was created (`On <branch>:` field).
    pub branch: String,
    /// Message at push time, or git's default `WIP on <branch>: <sha> <subject>`.
    pub message: String,
}

/// Local branch with current/upstream metadata. v1 ignores remote-only branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    /// Configured upstream short-name (e.g. `origin/main`). `None` if no tracking ref.
    pub upstream: Option<String>,
}

/// One worktree (main or linked) reported by `git worktree list --porcelain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Checked-out branch, or `None` if HEAD is detached.
    pub branch: Option<String>,
    /// Full HEAD SHA.
    pub head: String,
    /// True for the primary worktree (the one containing `.git/`).
    pub is_main: bool,
    /// True if `git worktree lock` is in effect.
    pub is_locked: bool,
}

/// Result of [`Repository::merge_branch`](crate::git_ops). The auto-stash ref
/// is preserved across BOTH success and conflict paths so the caller can
/// always recover the stashed work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MergeOutcome {
    /// Fast-forward; no merge commit was created.
    FastForward,
    /// True (non-FF) merge committed cleanly.
    Merged,
    /// Branch was already at or behind HEAD — nothing to merge, no commit
    /// created. Distinguished from `Merged` so the UI can show "nothing to do"
    /// rather than "merge complete."
    AlreadyUpToDate,
    /// Merge produced conflicts. Working tree has conflict markers.
    /// `auto_stash` is `Some` when main was dirty pre-merge (stash NOT popped —
    /// caller must resolve conflicts, then pop manually).
    Conflicted {
        conflicts: Vec<PathBuf>,
        auto_stash: Option<StashRef>,
    },
    /// Main was dirty pre-merge and auto-stash was pushed.
    ///
    /// - `pop_failed=false`: stash was popped successfully; `stash_ref` is
    ///   informational (audit trail).
    /// - `pop_failed=true`: merge committed cleanly but pop hit a conflict
    ///   against the merge result. Working tree is at the merged HEAD; the
    ///   stash is still on the stack at `stash_ref` and the caller must
    ///   resolve manually (e.g. surface "stash@{N} needs manual pop").
    ///
    /// Either way the merge itself succeeded.
    AutoStashed {
        stash_ref: StashRef,
        inner: Box<MergeOutcome>,
        pop_failed: bool,
    },
}

/// Long-running git operation that has been started but not yet
/// completed, detected from the presence of sentinel files inside
/// `.git/`. Surfaced by
/// [`Repository::current_operation`](crate::git_ops) so the SCM panel
/// can render an "X in progress" banner and gate destructive ops.
///
/// Detection precedence (highest priority first):
///
/// 1. `Rebase`   — `.git/rebase-merge/` OR `.git/rebase-apply/` exists
/// 2. `Merge`    — `.git/MERGE_HEAD` exists
/// 3. `CherryPick` — `.git/CHERRY_PICK_HEAD` exists
/// 4. `Revert`   — `.git/REVERT_HEAD` exists
/// 5. `Bisect`   — `.git/BISECT_LOG` exists
///
/// Under normal operation git permits only one of these states at a
/// time. A partially-cleaned `.git/` (after a crash or interrupted
/// sequence) can leave a stale `MERGE_HEAD` alongside an active
/// `rebase-merge/`, which is why `Rebase` outranks `Merge`: the
/// rebase is the live operation, the `MERGE_HEAD` is a straggler.
///
/// The plain-data shape (no IO, no git invocation) keeps it inside
/// `oximux-core` so UI layers can hold it without depending on
/// `oximux-git`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GitOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl GitOperation {
    /// User-facing label for the operation banner: "Merge in progress",
    /// "Rebase in progress", etc. Stable copy — referenced by snapshot
    /// tests in the UI layer, so changing this rotates banner text.
    pub fn banner_label(self) -> &'static str {
        match self {
            GitOperation::Merge => "Merge in progress",
            GitOperation::Rebase => "Rebase in progress",
            GitOperation::CherryPick => "Cherry-pick in progress",
            GitOperation::Revert => "Revert in progress",
            GitOperation::Bisect => "Bisect in progress",
        }
    }

    /// Whether the operation has a `--continue` step that resumes it after
    /// the user resolves conflicts. The sequencer operations (rebase,
    /// cherry-pick, revert) replay one commit at a time and pause on
    /// conflict, so they continue. A merge has no `--continue` (completing
    /// it is just a normal commit), and bisect is a search session with no
    /// resume-after-conflict semantics — both offer Abort only.
    pub fn supports_continue(self) -> bool {
        matches!(
            self,
            GitOperation::Rebase | GitOperation::CherryPick | GitOperation::Revert
        )
    }
}

#[cfg(test)]
mod tests {
    use super::GitOperation;

    #[test]
    fn only_sequencer_ops_support_continue() {
        // Sequencer ops replay commit-by-commit and pause on conflict →
        // resumable. Merge/Bisect have no --continue step → Abort only.
        assert!(GitOperation::Rebase.supports_continue());
        assert!(GitOperation::CherryPick.supports_continue());
        assert!(GitOperation::Revert.supports_continue());
        assert!(!GitOperation::Merge.supports_continue());
        assert!(!GitOperation::Bisect.supports_continue());
    }
}
