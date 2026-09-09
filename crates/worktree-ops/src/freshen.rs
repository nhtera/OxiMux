//! Bring the local default branch up to date before branching off it.
//!
//! A worktree is created from whatever HEAD happens to be, so a default branch
//! last pulled a week ago gives the new worktree a week-old base. Turning this
//! on makes creation fetch and fast-forward first.
//!
//! **What that does and does not reach.** `git worktree add -b <branch> <path>`
//! takes no start point, so the new branch is cut from HEAD. When the main
//! checkout is sitting on the default branch — the usual state for a
//! worktree-driven workflow, where the root stays on `main` and the work
//! happens elsewhere — the fast-forward moves HEAD, and the new worktree does
//! start from the freshened commit. When HEAD is on some other branch, this
//! updates the local default *ref* and the new worktree is still cut from
//! HEAD, unchanged. Choosing the start point explicitly is Phase 2's job, not
//! this setting's.
//!
//! **Every refusal is silent and normal.** This runs on the create path, where
//! the user asked for a worktree and not for a sync — so a dirty tree, a
//! local-only commit, a repository with no remote, or a network that is not
//! there must all leave creation completely unaffected. There is exactly one
//! success case and every other outcome is [`Freshened::Skipped`], carrying the
//! reason for the log and nothing else.
//!
//! **Never merges, never forces.** The only mutating commands are
//! `merge --ff-only` and `fetch <remote> <branch>:<branch>`, and git itself
//! refuses both when the move is not a fast-forward. The ancestry check below
//! is a cheap way to skip early with a legible reason, not the safety net —
//! the safety net is git's.

use oximux_git::Repository;

/// What the freshen attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshened {
    /// The local default branch moved forward to the remote's.
    FastForwarded { branch: String, to: String },
    /// Nothing was done, for a reason that is not a failure.
    Skipped(SkipReason),
}

impl Freshened {
    /// One line for the provisioning transcript. A skip reads as an ordinary
    /// statement, not as a warning — the user asked for a worktree, and none
    /// of these outcomes stood in the way of getting one.
    pub fn summary(&self) -> String {
        match self {
            Self::FastForwarded { branch, to } => {
                let short: String = to.chars().take(8).collect();
                format!("{branch} updated to {short}")
            }
            Self::Skipped(reason) => reason.summary(),
        }
    }
}

/// Why a freshen did nothing. Every variant is an ordinary state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The setting is off.
    Disabled,
    /// No default branch could be detected — nothing to freshen.
    NoDefaultBranch,
    /// `git fetch` failed: no remote, no network, no credentials.
    FetchFailed(String),
    /// The remote has no counterpart for the default branch.
    NoRemoteCounterpart,
    /// Local and remote are already the same commit.
    AlreadyCurrent,
    /// The local branch has commits the remote does not — fast-forwarding
    /// would lose them, so it is not attempted.
    LocalOnlyCommits,
    /// The default branch is checked out and has uncommitted changes.
    WorkingTreeDirty,
    /// Git refused the fast-forward. Kept verbatim for the log; the create
    /// carries on regardless.
    Refused(String),
}

impl SkipReason {
    /// One line naming what was left alone, and why.
    pub fn summary(&self) -> String {
        match self {
            Self::Disabled => "left the default branch alone (setting is off)".to_string(),
            Self::NoDefaultBranch => "no default branch to update".to_string(),
            Self::FetchFailed(_) => "could not reach the remote; using what is on disk".to_string(),
            Self::NoRemoteCounterpart => "the remote has no copy of that branch".to_string(),
            Self::AlreadyCurrent => "the default branch was already up to date".to_string(),
            Self::LocalOnlyCommits => {
                "the default branch has commits the remote does not; left alone".to_string()
            }
            Self::WorkingTreeDirty => {
                "the default branch has uncommitted changes; left alone".to_string()
            }
            Self::Refused(err) => format!("git declined to update the default branch: {err}"),
        }
    }
}

/// The remote to freshen from. `origin` is the only one OxiMux's default-branch
/// detection consults (`origin/HEAD`), so freshening from anything else would
/// update a branch against a remote the rest of the app is not looking at.
const REMOTE: &str = "origin";

/// Fetch and fast-forward `default_branch` in `repo`, or skip with a reason.
///
/// `default_branch` comes from the caller because it is already resolved on the
/// create path — re-detecting here would cost a second round of git calls to
/// answer a question that was just answered.
pub async fn freshen_default_branch(repo: &Repository, default_branch: &str) -> Freshened {
    // One branch from one remote, not `--all --prune`: the setting promises to
    // freshen the default branch, and fetching every remote (and pruning their
    // refs) is both broader than that promise and slower on the create path.
    if let Err(err) = repo.fetch_remote_branch(REMOTE, default_branch).await {
        // No remote, offline, credentials, or no such branch upstream — all
        // the same answer: work with what is on disk. This is the most common
        // skip by a wide margin.
        return Freshened::Skipped(SkipReason::FetchFailed(err.to_string()));
    }

    let remote_ref = format!("refs/remotes/{REMOTE}/{default_branch}");
    let local_ref = format!("refs/heads/{default_branch}");
    // A git failure and a missing ref are different answers, and reporting the
    // first as the second would put "no remote counterpart" in the log for a
    // repository that has one.
    let remote_sha = match repo.sha_of(&remote_ref).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return Freshened::Skipped(SkipReason::NoRemoteCounterpart),
        Err(err) => return Freshened::Skipped(SkipReason::Refused(err.to_string())),
    };
    let local_sha = match repo.sha_of(&local_ref).await {
        Ok(Some(sha)) => sha,
        // The remote has the branch and we do not. Creating it is a different
        // operation than freshening one, and not what the setting promises.
        Ok(None) => return Freshened::Skipped(SkipReason::NoRemoteCounterpart),
        Err(err) => return Freshened::Skipped(SkipReason::Refused(err.to_string())),
    };
    if local_sha == remote_sha {
        return Freshened::Skipped(SkipReason::AlreadyCurrent);
    }
    // Not an ancestor means the local branch has commits of its own, or the
    // two have diverged. Either way a fast-forward is not what is wanted, and
    // git would refuse it a moment later anyway.
    match repo.is_ancestor(&local_sha, &remote_sha).await {
        Ok(true) => {}
        Ok(false) => return Freshened::Skipped(SkipReason::LocalOnlyCommits),
        Err(err) => return Freshened::Skipped(SkipReason::Refused(err.to_string())),
    }

    // Checked out here or not: the two cases need different commands, because
    // git will not fetch onto a branch that a worktree has checked out.
    let on_default = matches!(repo.current_branch().await, Ok(Some(b)) if b == default_branch);
    let result = if on_default {
        match repo.is_dirty().await {
            Ok(true) => return Freshened::Skipped(SkipReason::WorkingTreeDirty),
            Ok(false) => {}
            // Unknown dirtiness is treated as dirty: the whole point is to
            // never touch a working tree that might hold the user's edits.
            Err(err) => return Freshened::Skipped(SkipReason::Refused(err.to_string())),
        }
        repo.fast_forward_to(&format!("{REMOTE}/{default_branch}")).await
    } else {
        repo.fetch_branch_fast_forward(REMOTE, default_branch).await
    };

    match result {
        Ok(()) => Freshened::FastForwarded {
            branch: default_branch.to_string(),
            to: remote_sha,
        },
        Err(err) => Freshened::Skipped(SkipReason::Refused(err.to_string())),
    }
}
