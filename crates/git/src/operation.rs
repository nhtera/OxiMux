//! Long-running-git-operation detection.
//!
//! Git lets the user start a merge / rebase / cherry-pick / revert /
//! bisect that may pause mid-flight (waiting for conflict resolution,
//! manual edits, "git rebase --continue"). The SCM panel needs to
//! know when one is in progress so it can render an "X in progress"
//! amber banner and gate destructive ops.
//!
//! Detection reads `.git/`-relative sentinel files via `fs::metadata`
//! — no `git` invocation, no IO that can hang, microsecond cost per
//! call. Suitable for running on every poll tick.
//!
//! Worktree-aware: uses [`Repository::git_dir`] (captured at open()
//! via `git rev-parse --git-dir`), NOT `workdir/.git`. For a linked
//! worktree these differ — sentinels live in
//! `<main>/.git/worktrees/<name>/` for that specific worktree, not in
//! the main `.git/`.
//!
//! [`Repository::git_dir`]: crate::Repository

use oximux_core::GitOperation;

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;

impl Repository {
    /// Detect which long-running git operation (if any) is currently
    /// in progress on this worktree.
    ///
    /// Precedence when multiple sentinels coexist (a corrupted state
    /// — git itself permits only one at a time, but partial cleanup
    /// after a crash can leave stragglers):
    ///
    /// 1. `Rebase`     — `.git/rebase-merge/` OR `.git/rebase-apply/`
    /// 2. `Merge`      — `.git/MERGE_HEAD`
    /// 3. `CherryPick` — `.git/CHERRY_PICK_HEAD`
    /// 4. `Revert`     — `.git/REVERT_HEAD`
    /// 5. `Bisect`     — `.git/BISECT_LOG`
    ///
    /// Rebase wins over Merge: under normal operation git permits
    /// only one of these states at a time, but a partially-cleaned
    /// `.git/` (after a crash or interrupted sequence) can leave a
    /// stale `MERGE_HEAD` alongside an active `rebase-merge/`. When
    /// both are present, `Rebase` is the more specific diagnosis —
    /// the rebase is the live operation; the MERGE_HEAD is a
    /// straggler.
    ///
    /// Synchronous + cheap — runs `fs::metadata` against five paths.
    /// No `Result` because a missing `.git/` is unrecoverable upstream
    /// (the `Repository` couldn't have been constructed) and per-file
    /// IO errors collapse to "no sentinel here" — surfacing them
    /// would add noise without affecting the user's choice.
    pub fn current_operation(&self) -> Option<GitOperation> {
        if self.git_dir.join("rebase-merge").is_dir()
            || self.git_dir.join("rebase-apply").is_dir()
        {
            return Some(GitOperation::Rebase);
        }
        if self.git_dir.join("MERGE_HEAD").is_file() {
            return Some(GitOperation::Merge);
        }
        if self.git_dir.join("CHERRY_PICK_HEAD").is_file() {
            return Some(GitOperation::CherryPick);
        }
        if self.git_dir.join("REVERT_HEAD").is_file() {
            return Some(GitOperation::Revert);
        }
        if self.git_dir.join("BISECT_LOG").is_file() {
            return Some(GitOperation::Bisect);
        }
        None
    }

    /// Abort the in-progress operation, returning the worktree to its
    /// pre-operation state. Each op has its own escape hatch:
    /// `git merge/rebase/cherry-pick/revert --abort`, and `git bisect reset`
    /// for bisect (which has no `--abort`). Always safe to offer — abort
    /// discards the partial operation rather than the user's prior commits.
    pub async fn abort_operation(&self, op: GitOperation) -> Result<()> {
        let args: &[&str] = match op {
            GitOperation::Merge => &["merge", "--abort"],
            GitOperation::Rebase => &["rebase", "--abort"],
            GitOperation::CherryPick => &["cherry-pick", "--abort"],
            GitOperation::Revert => &["revert", "--abort"],
            GitOperation::Bisect => &["bisect", "reset"],
        };
        GitCmd::new(self.workdir()).args(args).run().await?;
        Ok(())
    }

    /// Resume a paused sequencer operation after the user staged their
    /// conflict resolutions (`git rebase/cherry-pick/revert --continue`).
    ///
    /// `-c core.editor=true` accepts the prepared commit message without
    /// opening an interactive editor — the GUI has no TTY to host one, so
    /// the default `core.editor` would hang the command. Operations without
    /// a continue step (`Merge`, `Bisect`) return `InvalidInput`; callers
    /// gate on [`GitOperation::supports_continue`] so this is only reached
    /// defensively.
    ///
    /// If conflicts remain unstaged, git rejects the continue and the raw
    /// stderr rides the `Err` to the status row — the operation stays paused
    /// rather than advancing past unresolved markers.
    pub async fn continue_operation(&self, op: GitOperation) -> Result<()> {
        let verb = match op {
            GitOperation::Rebase => "rebase",
            GitOperation::CherryPick => "cherry-pick",
            GitOperation::Revert => "revert",
            GitOperation::Merge | GitOperation::Bisect => {
                return Err(GitError::invalid_input(
                    "operation has no --continue step",
                ));
            }
        };
        GitCmd::new(self.workdir())
            .args(["-c", "core.editor=true", verb, "--continue"])
            .run()
            .await?;
        Ok(())
    }
}
