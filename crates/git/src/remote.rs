//! Remote operations on `Repository`: push / pull / sync / publish_branch
//! and the read-only `list_remote_branches` lookup that backs the
//! BaseRef picker.
//!
//! Thin wrappers over the `git` CLI. Each method runs with `GIT_OPTIONAL_LOCKS=0`
//! (inherited from `process::GitCmd`) so they don't fight the StatusPoller's
//! 500 ms tick for the index lock. Timeouts are stretched to 60 s because
//! network round-trips can be slow on a flaky connection — calling code is
//! responsible for surfacing a "still running" indicator if needed.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::BranchInfo;
use std::time::{Duration, Instant};

/// Remote ops can talk to a network — give them more headroom than the 10 s
/// default. Long enough to push/pull a moderate diff, short enough that a
/// genuinely hung command surfaces as `Timeout` instead of wedging the UI.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `list_remote_branches` result stays warm. Picked to be
/// short enough that a manual `git fetch` outside the app is reflected
/// quickly, long enough that the BaseRef picker doesn't shell out on
/// every keystroke during filtering. Callers that need fresh data
/// (e.g. immediately after a programmatic `fetch`) pass
/// `force_refresh = true` to bypass the cache.
const REMOTE_BRANCH_CACHE_TTL: Duration = Duration::from_secs(5);

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

    /// List remote-tracking branches across every configured remote
    /// (`origin/main`, `upstream/release`, ...) in the form
    /// `<remote>/<branch>`. Results are cached for
    /// [`REMOTE_BRANCH_CACHE_TTL`]; pass `force_refresh = true` to
    /// bypass the cache (e.g. after a programmatic `fetch`).
    ///
    /// `is_current` is always `false` and `upstream` is always `None` —
    /// remote-tracking refs don't have their own upstream and are never
    /// the working tree's current branch.
    pub async fn list_remote_branches(&self, force_refresh: bool) -> Result<Vec<BranchInfo>> {
        if !force_refresh
            && let Some(cached) = self.cached_remote_branches()
        {
            return Ok(cached);
        }
        let fresh = self.fetch_remote_branch_list().await?;
        self.store_remote_branches_cache(fresh.clone());
        Ok(fresh)
    }

    fn cached_remote_branches(&self) -> Option<Vec<BranchInfo>> {
        let guard = self.remote_branch_cache.read().ok()?;
        let (recorded_at, branches) = guard.as_ref()?;
        if recorded_at.elapsed() < REMOTE_BRANCH_CACHE_TTL {
            Some(branches.clone())
        } else {
            None
        }
    }

    fn store_remote_branches_cache(&self, branches: Vec<BranchInfo>) {
        // Lock poisoning here is benign — the cache is best-effort. Drop
        // the update rather than panicking the caller's task.
        if let Ok(mut guard) = self.remote_branch_cache.write() {
            *guard = Some((Instant::now(), branches));
        }
    }

    async fn fetch_remote_branch_list(&self) -> Result<Vec<BranchInfo>> {
        let out = GitCmd::new(self.workdir())
            .args([
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/remotes/",
            ])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in `git for-each-ref`: {e}")))?;
        Ok(parse_remote_branch_list(&text))
    }
}

/// Parse the output of our `git for-each-ref refs/remotes/`
/// invocation. Each non-empty line is a short refname like
/// `origin/main`. Skips:
/// - `<remote>/HEAD` aliases (they just point at the remote's default
///   branch — already listed under its real name).
/// - Bare remote names with no slash (some git versions emit these for
///   unresolved HEADs).
pub(crate) fn parse_remote_branch_list(text: &str) -> Vec<BranchInfo> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let name = raw.trim_end_matches(['\r', ' ']).trim_start();
        if name.is_empty() {
            continue;
        }
        if name.ends_with("/HEAD") || !name.contains('/') {
            continue;
        }
        out.push(BranchInfo {
            name: name.to_string(),
            is_current: false,
            upstream: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_empty_and_head_aliases() {
        let text = "\
origin/main
origin/HEAD
origin/release
upstream/main

upstream/HEAD
";
        let branches = parse_remote_branch_list(text);
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["origin/main", "origin/release", "upstream/main"]);
        // Every entry MUST be remote-tracking shape.
        assert!(branches.iter().all(|b| !b.is_current));
        assert!(branches.iter().all(|b| b.upstream.is_none()));
    }

    #[test]
    fn parse_drops_bare_remote_names() {
        // Defensive: some git versions emit a bare remote name for an
        // unresolved HEAD. We never want to surface "origin" as a branch.
        let text = "origin\norigin/main\n";
        let branches = parse_remote_branch_list(text);
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["origin/main"]);
    }

    #[test]
    fn parse_trims_trailing_whitespace() {
        let text = "origin/main  \r\norigin/release\r\n";
        let names: Vec<String> = parse_remote_branch_list(text)
            .iter()
            .map(|b| b.name.clone())
            .collect();
        assert_eq!(names, ["origin/main", "origin/release"]);
    }

    #[test]
    fn parse_empty_input_yields_empty_vec() {
        assert!(parse_remote_branch_list("").is_empty());
        assert!(parse_remote_branch_list("\n\n\n").is_empty());
    }
}
