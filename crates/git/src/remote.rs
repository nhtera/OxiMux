//! Remote operations on `Repository`: push / pull / sync / publish_branch.
//!
//! Thin wrappers over the `git` CLI. Each method runs with `GIT_OPTIONAL_LOCKS=0`
//! (inherited from `process::GitCmd`) so they don't fight the StatusPoller's
//! 500 ms tick for the index lock. Timeouts are stretched to 60 s because
//! network round-trips can be slow on a flaky connection — calling code is
//! responsible for surfacing a "still running" indicator if needed.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use std::time::Duration;

/// Remote ops can talk to a network — give them more headroom than the 10 s
/// default. Long enough to push/pull a moderate diff, short enough that a
/// genuinely hung command surfaces as `Timeout` instead of wedging the UI.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

impl Repository {
    /// `git push` against the configured upstream. Caller is responsible for
    /// ensuring the branch HAS an upstream (use `publish_branch` first
    /// otherwise — git's error is friendly but the round-trip is wasted).
    pub async fn push(&self) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["push"])
            .timeout(REMOTE_TIMEOUT)
            .run()
            .await?;
        Ok(())
    }

    /// `git pull --ff-only`. Fast-forward only so the user never lands an
    /// implicit merge commit; if pull would create a merge they need to run
    /// the merge UI explicitly (out of scope for v1).
    pub async fn pull(&self) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["pull", "--ff-only"])
            .timeout(REMOTE_TIMEOUT)
            .run()
            .await?;
        Ok(())
    }

    /// Pull then push, in that order — the conventional "Sync" verb.
    /// Failure on either step surfaces immediately — no rollback because the
    /// pull step never mutates remote state.
    pub async fn sync(&self) -> Result<()> {
        self.pull().await?;
        self.push().await
    }

    /// `git fetch --all --prune`. Updates remote-tracking refs without
    /// touching the working tree. Pruned refs disappear locally to match
    /// the remote; useful as a low-risk verb before deciding whether to
    /// pull or sync.
    pub async fn fetch(&self) -> Result<()> {
        GitCmd::new(self.workdir())
            .args(["fetch", "--all", "--prune"])
            .timeout(REMOTE_TIMEOUT)
            .run()
            .await?;
        Ok(())
    }

    /// Publish a new branch by pushing it with `-u <remote> <current-branch>`.
    /// Resolves the current branch name via `rev-parse --abbrev-ref HEAD`;
    /// detached HEAD returns `InvalidInput`.
    pub async fn publish_branch(&self, remote: &str) -> Result<()> {
        let branch = self.current_branch_name().await?;
        if branch == "HEAD" {
            return Err(GitError::invalid_input(
                "cannot publish a detached HEAD; create a branch first",
            ));
        }
        GitCmd::new(self.workdir())
            .args(["push", "-u", remote, &branch])
            .timeout(REMOTE_TIMEOUT)
            .run()
            .await?;
        Ok(())
    }

    /// Current branch short name, or `"HEAD"` when detached. Helper for
    /// `publish_branch`; private to the remote module because the status
    /// poller already exposes the same info through `GitState::branch`.
    async fn current_branch_name(&self) -> Result<String> {
        let out = GitCmd::new(self.workdir())
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .run()
            .await?;
        let name = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("branch name not utf-8: {e}")))?
            .trim()
            .to_string();
        if name.is_empty() {
            return Err(GitError::parse(
                "empty branch name from `git rev-parse --abbrev-ref HEAD`",
            ));
        }
        Ok(name)
    }
}
