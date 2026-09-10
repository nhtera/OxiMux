//! Merge with auto-stash recovery.
//!
//! The single entry point — [`Repository::merge_branch`] — runs
//! `git merge --no-edit <branch>`, honoring the user's `merge.ff` config (no
//! `--no-ff`). When the working tree is dirty going in, it stashes first and
//! arranges that the resulting [`MergeOutcome`] surfaces the stash ref on
//! BOTH the clean-merge and conflict paths so the caller can always restore
//! the pre-merge state.
//!
//! The caller is the desktop's `Merge into <default>` row action
//! (`apps/desktop/src/shell/workspace/merge_ops.rs`), which merges a
//! worktree's branch into the project's default branch by opening a
//! `Repository` at the PROJECT ROOT — `merge_branch` merges the named branch
//! into the repository it is called on, so opening the worktree instead would
//! merge main into the feature branch. The pre-flight that guards it lives in
//! `oximux-worktree-ops::merge`.
//!
//! Error-recovery contract (locked 2026-05-17, Q1):
//! When the merge fails with a NonZero exit (e.g. unknown branch) AFTER an
//! auto-stash was pushed, we pop the stash before returning `Err`. Atomic:
//! "merge failed = back to where I was." If the pop itself fails (rare), we
//! `tracing::warn!` and return the merge error chained with a note that the
//! stash ref is dangling.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::repository::Repository;
use oximux_core::{MergeOutcome, StashRef};
use std::path::PathBuf;

/// The `-m` text `merge_branch` gives its auto-stash.
///
/// Public because it is the only durable handle on that stash: `StashRef.index`
/// renumbers, so a caller that has to point a user back at a stash minutes or
/// hours later must match `git stash list` on this string. Hard-coding it a
/// second time at the call site would let the two drift apart silently, and the
/// symptom would be a recovery notice that can never resolve.
pub const AUTO_STASH_MESSAGE: &str = "oximux: auto-stash before merge";

impl Repository {
    /// Merge `branch` into the current HEAD with auto-stash recovery.
    ///
    /// See module docs for the error-recovery contract on merge failure.
    /// `MergeOutcome::AutoStashed` is used for any clean-merge path that
    /// pushed an auto-stash — the `pop_failed` field distinguishes the rare
    /// pop-conflicts-with-merge-result case so callers can surface manual
    /// recovery without losing the fact that the merge itself succeeded.
    /// Conflict outcomes carry the (un-popped) ref inside
    /// `Conflicted { auto_stash: Some(_) }`.
    pub async fn merge_branch(&self, branch: &str) -> Result<MergeOutcome> {
        if branch.is_empty() {
            return Err(GitError::invalid_input("branch name is empty"));
        }
        let auto_stash = if self.is_dirty().await? {
            Some(
                self.stash_push(Some(AUTO_STASH_MESSAGE), false)
                    .await?,
            )
        } else {
            None
        };

        let raw = GitCmd::new(self.workdir())
            .args(["merge", "--no-edit", "--", branch])
            .run_raw()
            .await?;

        if raw.status.success() {
            let inner = classify_clean_merge(&raw.stdout);
            return finish_clean(self, auto_stash, inner).await;
        }

        let code = raw.status.code().unwrap_or(-1);
        // Exit 1 covers BOTH "merge produced conflicts" (stdout has CONFLICT
        // markers, working tree has unmerged paths) AND early failures like
        // "merge: ghost - not something we can merge" (stderr). Differentiate
        // by querying the unmerged-path list — if empty, it's a hard failure.
        if code == 1 {
            let conflicts = self.list_conflicting_paths().await?;
            if !conflicts.is_empty() {
                return Ok(MergeOutcome::Conflicted {
                    conflicts,
                    auto_stash,
                });
            }
            // Fall through to the hard-failure path (pop stash + propagate).
        }

        // Hard merge failure (unknown branch, refusal, etc.) — pop the
        // auto-stash to restore the pre-merge state and propagate the error.
        // git emits "merge: <x> - not something we can merge" on stderr for
        // unknown-branch failures, but routine merge progress goes to stdout;
        // fall back to stdout when stderr is empty so the error message is
        // never lost.
        let stderr_raw = if raw.stderr.is_empty() {
            &raw.stdout
        } else {
            &raw.stderr
        };
        let stderr = trim_stderr_lossy(stderr_raw);
        if let Some(stash) = auto_stash
            && let Err(pop_err) = self.stash_pop(&stash).await
        {
            tracing::warn!(
                target: "oximux_git::merge",
                stash = %stash.ref_string(),
                merge_err = %stderr,
                pop_err = ?pop_err,
                "merge failed and auto-stash pop also failed — stash ref is dangling"
            );
            return Err(GitError::NonZero {
                code,
                stderr: format!(
                    "{stderr} (auto-stash {} could not be popped: {pop_err})",
                    stash.ref_string()
                ),
            });
        }
        Err(GitError::NonZero { code, stderr })
    }

    /// Is `ancestor` reachable from `descendant`? (`git merge-base
    /// --is-ancestor`.)
    ///
    /// The merge pre-flight's "nothing to merge" test: when the worktree
    /// branch is already an ancestor of the default branch, merging it would
    /// report `AlreadyUpToDate` after mutating nothing — but it would still
    /// auto-stash a dirty tree on the way there. Answering the question first
    /// means a no-op merge never touches the user's working tree.
    ///
    /// Exit 1 is git's honest "no", not a failure, so it maps to `Ok(false)`.
    /// Any other non-zero (an unknown ref, a broken repo) stays an error —
    /// treating those as "no" would silently run a merge the caller believed
    /// it had ruled out.
    pub async fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        if ancestor.is_empty() || descendant.is_empty() {
            return Err(GitError::invalid_input("revision is empty"));
        }
        let raw = GitCmd::new(self.workdir())
            // `--` before the revisions: both are caller-supplied, and this is
            // the one comparison that runs BEFORE `add_worktree_from`'s own
            // validation on the create path.
            .args(["merge-base", "--is-ancestor", "--", ancestor, descendant])
            .run_raw()
            .await?;
        if raw.status.success() {
            return Ok(true);
        }
        match raw.status.code() {
            Some(1) => Ok(false),
            code => Err(GitError::NonZero {
                code: code.unwrap_or(-1),
                stderr: trim_stderr_lossy(&raw.stderr),
            }),
        }
    }

    /// Files that still have unresolved conflict markers, per
    /// `git diff --name-only --diff-filter=U`. Returned paths are
    /// workdir-relative; callers that need absolute paths join with
    /// `self.workdir()`. `pub` (not `pub(crate)`) so the SCM panel's
    /// conflict-banner "Open all in editor" button can iterate them
    /// without depending on `oximux-git` internals.
    pub async fn list_conflicting_paths(&self) -> Result<Vec<PathBuf>> {
        let out = GitCmd::new(self.workdir())
            .args(["diff", "--name-only", "--diff-filter=U"])
            .run()
            .await?;
        let text = String::from_utf8(out.stdout)
            .map_err(|e| GitError::parse(format!("non-utf8 in conflict-name listing: {e}")))?;
        Ok(text
            .lines()
            .map(|l| l.trim_end())
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect())
    }
}

async fn finish_clean(
    repo: &Repository,
    auto_stash: Option<StashRef>,
    inner: MergeOutcome,
) -> Result<MergeOutcome> {
    let Some(stash) = auto_stash else {
        return Ok(inner);
    };
    // Pop on user's behalf so the working tree returns to "dirty + merged".
    // On pop failure (rare — a stash that conflicts with the merge result),
    // the merge itself has already landed and HEAD has moved. Surfacing
    // `Err` here would mislead the caller into thinking the merge failed;
    // instead, return `AutoStashed { pop_failed: true }` so the UI can show
    // "merge OK, stash@{N} needs manual pop" while preserving the live ref.
    let pop_failed = match repo.stash_pop(&stash).await {
        Ok(()) => false,
        Err(pop_err) => {
            tracing::warn!(
                target: "oximux_git::merge",
                stash = %stash.ref_string(),
                pop_err = ?pop_err,
                "merge succeeded but auto-stash pop failed — stash ref preserved on stack"
            );
            true
        }
    };
    Ok(MergeOutcome::AutoStashed {
        stash_ref: stash,
        inner: Box::new(inner),
        pop_failed,
    })
}

/// Classify a successful merge from its stdout. (git emits its merge-summary
/// lines on stdout, not stderr.) Locale is pinned to `C` in `GitCmd`, so the
/// literal "Fast-forward" / "Already up to date." strings are reliable across
/// user environments.
fn classify_clean_merge(stdout: &[u8]) -> MergeOutcome {
    let s = String::from_utf8_lossy(stdout);
    if s.contains("Already up to date.") {
        MergeOutcome::AlreadyUpToDate
    } else if s.contains("Fast-forward") {
        MergeOutcome::FastForward
    } else {
        MergeOutcome::Merged
    }
}

fn trim_stderr_lossy(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ff_message() {
        let stdout = b"Updating abc..def\nFast-forward\n b.txt | 1 +\n";
        assert!(matches!(
            classify_clean_merge(stdout),
            MergeOutcome::FastForward
        ));
    }

    #[test]
    fn classify_non_ff_message() {
        let stdout = b"Merge made by the 'ort' strategy.\n";
        assert!(matches!(classify_clean_merge(stdout), MergeOutcome::Merged));
    }

    #[test]
    fn classify_already_up_to_date_distinguished() {
        let stdout = b"Already up to date.\n";
        assert!(matches!(
            classify_clean_merge(stdout),
            MergeOutcome::AlreadyUpToDate
        ));
    }
}
