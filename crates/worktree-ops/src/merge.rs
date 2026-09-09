//! Landing a worktree's branch in the project's default branch.
//!
//! **`merge_branch(x)` merges `x` into the repository it is called on.** So
//! landing worktree `feat-x` means opening a `Repository` at the *project
//! root* and merging `oximux/feat-x` into it. Opening the worktree instead
//! would merge main into the feature branch — plausible, silent and backwards.
//! That is why [`preflight_merge`] takes the project root explicitly and this
//! module never derives a repository from a workspace row.
//!
//! This is the most destructive operation the worktree crate offers: it mutates
//! the main checkout, which the user may be looking at, and `merge_branch`
//! auto-stashes that checkout when it is dirty. Every refusal below exists to
//! keep the user from discovering that after the fact.
//!
//! **The pre-flight is advice, not a lock.** Every check is a read, and the
//! merge that follows is unsynchronized: HEAD can move between the two.
//! [`apply_merge`] re-reads HEAD and the operation sentinels immediately before
//! merging and aborts if either changed, which closes the window that matters
//! cheaply. A full repository lock is out of scope for a single-user v1; that
//! is a decision, not an omission.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oximux_core::{GitOperation, MergeOutcome, Workspace};
use oximux_git::Repository;

use crate::paths::{path_is_within, paths_equal};

/// Why a merge was declined, with enough detail for the UI to say so.
///
/// Each variant names the thing standing in the way — the user's next question
/// after "can't merge" is always "because of what?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRefusal {
    /// The row is the project's own checkout (a synthesized primary row), not a
    /// linked worktree. Merging a branch into itself is not a thing to offer.
    NotAWorktree,
    /// The project root checkout is on some other branch. Merging would land
    /// the work somewhere the user did not ask for, under a name that says
    /// otherwise.
    RootNotOnDefault { on: Option<String>, default: String },
    /// A merge / rebase / cherry-pick / revert / bisect is paused in the
    /// project root. Merging now would auto-stash a half-finished operation
    /// out from under the user.
    OperationInProgress { operation: GitOperation },
    /// A live agent holds the project root. Not a lock — an advisory refusal,
    /// because auto-stashing a directory an agent is actively editing destroys
    /// work that agent has not committed.
    RootHeld { holder: PathBuf },
    /// The worktree branch is already reachable from the default branch. There
    /// is nothing to merge, and saying so beats running a merge that would
    /// auto-stash a dirty tree only to report `AlreadyUpToDate`.
    ///
    /// Not an error: this is the honest answer to a reasonable question.
    NothingToMerge,
    /// HEAD moved between the pre-flight and the merge. The branch would have
    /// landed on a base the user never saw.
    HeadMoved { was: String, now: String },
    /// A pre-flight check could not be completed (the repo would not open, or
    /// git failed to answer). Nothing was touched.
    PreflightFailed { error: String },
}

impl MergeRefusal {
    /// One sentence naming the obstacle, for a dialog body or a toast.
    ///
    /// `branch` is the worktree branch the user asked to land.
    pub fn message(&self, branch: &str) -> String {
        match self {
            Self::NotAWorktree => {
                "This row is the project's own checkout, so there is no branch to land."
                    .to_string()
            }
            Self::RootNotOnDefault { on, default } => match on {
                Some(on) => format!(
                    "The project folder is on \u{201c}{on}\u{201d}, not \u{201c}{default}\u{201d}. \
                     Switch it to \u{201c}{default}\u{201d} first \u{2014} merging now would land \
                     the work on \u{201c}{on}\u{201d}."
                ),
                None => format!(
                    "The project folder has a detached HEAD, not \u{201c}{default}\u{201d}. \
                     Check out \u{201c}{default}\u{201d} first."
                ),
            },
            Self::OperationInProgress { operation } => format!(
                "A {} is already in progress in the project folder. Finish or abort it first \
                 \u{2014} merging now would stash it out from under you.",
                operation.noun()
            ),
            Self::RootHeld { holder } => format!(
                "An agent is running in {}. Merging would stash its uncommitted edits. \
                 Stop it first.",
                holder.display()
            ),
            Self::NothingToMerge => format!(
                "Nothing to merge \u{2014} the default branch already has everything on \
                 \u{201c}{branch}\u{201d}."
            ),
            Self::HeadMoved { .. } => {
                "The project folder changed while this was being checked, so nothing was merged. \
                 Try again."
                    .to_string()
            }
            Self::PreflightFailed { error } => {
                format!("Couldn\u{2019}t check whether this merge is safe: {error}")
            }
        }
    }
}

/// The result of [`merge_into_default`].
#[derive(Debug)]
pub enum MergeResult {
    /// Nothing was touched, for the stated reason.
    Refused(MergeRefusal),
    /// `git merge` ran. Carries its outcome verbatim — including the nested
    /// [`MergeOutcome::AutoStashed`] shape, which the caller must peel rather
    /// than match as a peer of the others.
    Completed(MergeOutcome),
    /// `git merge` failed outright (an unknown branch, a refusal). Per the
    /// merge module's error-recovery contract any auto-stash is popped before
    /// the error is returned, so the checkout is normally back where it
    /// started.
    ///
    /// `stranded_auto_stash` is the exception the contract itself names: when
    /// that pop ALSO fails, the stash is left on the stack and `merge_branch`
    /// can only say so inside its error text. Text is not a pointer, so this
    /// flag carries the fact structurally — otherwise the one path where the
    /// merge failed *and* the user's uncommitted work went missing is the one
    /// path with no recovery notice.
    Failed {
        error: String,
        stranded_auto_stash: bool,
    },
}

/// What the mutation phase will do, decided by [`preflight_merge`].
#[derive(Debug, Clone)]
pub struct MergePlan {
    project_root: PathBuf,
    branch: String,
    /// HEAD at pre-flight time. [`apply_merge`] refuses if it has moved.
    head_before: String,
}

/// Pre-flight, then merge, against ONE holder snapshot.
///
/// Single-shot convenience for tests and for any host that can guarantee
/// nothing spawns underneath it. A host with live panes should call
/// [`preflight_merge`] and [`apply_merge`] separately so it can re-read holders
/// in between — the git round-trips in the pre-flight are hundreds of
/// milliseconds during which an agent can start in the project root.
pub async fn merge_into_default(
    project_root: &Path,
    workspace: &Workspace,
    default_branch: &str,
    holders: &HashSet<PathBuf>,
) -> MergeResult {
    match preflight_merge(project_root, workspace, default_branch, holders).await {
        Err(refusal) => MergeResult::Refused(refusal),
        Ok(plan) => apply_merge(&plan, holders).await,
    }
}

/// Check every refusal condition. Touches nothing.
///
/// `holders` is the set of directories with a live agent in them, computed by
/// the host and passed in. It is not computed here on purpose: liveness is a
/// fact only the host knows, and reimplementing it would let this refusal and
/// the host's own "live" indicator disagree with each other.
pub async fn preflight_merge(
    project_root: &Path,
    workspace: &Workspace,
    default_branch: &str,
    holders: &HashSet<PathBuf>,
) -> Result<MergePlan, MergeRefusal> {
    // A synthesized primary row IS the project's own checkout. Merging its
    // branch into itself is `AlreadyUpToDate` at best and nonsense at worst.
    if workspace.id.starts_with("primary:")
        || workspace.branch.is_empty()
        || paths_equal(Path::new(&workspace.worktree_path), project_root)
    {
        return Err(MergeRefusal::NotAWorktree);
    }

    let repo = match Repository::open(project_root).await {
        Ok(r) => r,
        Err(err) => {
            return Err(MergeRefusal::PreflightFailed {
                error: format!("open project repo: {err}"),
            });
        }
    };

    // The operation gate first: it is a filesystem stat with no subprocess, and
    // a paused rebase is the state in which every other check's answer is
    // untrustworthy anyway.
    if let Some(operation) = repo.current_operation() {
        return Err(MergeRefusal::OperationInProgress { operation });
    }

    match repo.current_branch().await {
        Ok(Some(on)) if on == default_branch => {}
        Ok(on) => {
            return Err(MergeRefusal::RootNotOnDefault {
                on,
                default: default_branch.to_string(),
            });
        }
        Err(err) => {
            return Err(MergeRefusal::PreflightFailed {
                error: format!("read current branch: {err}"),
            });
        }
    }

    // "Inside", not "equal": an agent's cwd is often a subdirectory of the
    // checkout it is working in, and auto-stashing under it loses exactly the
    // edits it has not committed yet.
    if let Some(holder) = holders.iter().find(|h| path_is_within(h, project_root)) {
        return Err(MergeRefusal::RootHeld {
            holder: holder.clone(),
        });
    }

    match repo.is_ancestor(&workspace.branch, "HEAD").await {
        Ok(true) => return Err(MergeRefusal::NothingToMerge),
        Ok(false) => {}
        Err(err) => {
            return Err(MergeRefusal::PreflightFailed {
                error: format!("compare \u{201c}{}\u{201d} with HEAD: {err}", workspace.branch),
            });
        }
    }

    let head_before = match repo.head_sha().await {
        Ok(sha) => sha,
        Err(err) => {
            return Err(MergeRefusal::PreflightFailed {
                error: format!("read HEAD: {err}"),
            });
        }
    };

    Ok(MergePlan {
        project_root: project_root.to_path_buf(),
        branch: workspace.branch.clone(),
        head_before,
    })
}

/// Carry out the plan: re-read what could have changed, then merge.
///
/// `holders_now` must be read **immediately before calling this**, not reused
/// from [`preflight_merge`]. Re-checking the same snapshot proves nothing: an
/// agent can start during the pre-flight's git round-trips, and the auto-stash
/// that follows would take its uncommitted edits with no error for anyone to
/// catch.
pub async fn apply_merge(plan: &MergePlan, holders_now: &HashSet<PathBuf>) -> MergeResult {
    let repo = match Repository::open(&plan.project_root).await {
        Ok(r) => r,
        Err(err) => {
            return MergeResult::Refused(MergeRefusal::PreflightFailed {
                error: format!("re-open project repo: {err}"),
            });
        }
    };

    if let Some(holder) = holders_now
        .iter()
        .find(|h| path_is_within(h, &plan.project_root))
    {
        return MergeResult::Refused(MergeRefusal::RootHeld {
            holder: holder.clone(),
        });
    }
    // Re-checked, not merely re-read: an operation started during the
    // pre-flight leaves HEAD where it was, so the HEAD compare below would miss
    // it. This one is a stat, so there is no reason not to.
    if let Some(operation) = repo.current_operation() {
        return MergeResult::Refused(MergeRefusal::OperationInProgress { operation });
    }
    let head_now = match repo.head_sha().await {
        Ok(sha) => sha,
        Err(err) => {
            return MergeResult::Refused(MergeRefusal::PreflightFailed {
                error: format!("re-read HEAD: {err}"),
            });
        }
    };
    if head_now != plan.head_before {
        return MergeResult::Refused(MergeRefusal::HeadMoved {
            was: plan.head_before.clone(),
            now: head_now,
        });
    }

    // Counted before, compared after: `merge_branch` reports a dangling
    // auto-stash only inside its error string, and a caller cannot act on
    // prose. Comparing the count is precise where scanning for the message
    // alone would also match a stash stranded by an earlier merge.
    let auto_stashes_before = count_auto_stashes(&repo).await;

    match repo.merge_branch(&plan.branch).await {
        Ok(outcome) => MergeResult::Completed(outcome),
        Err(err) => {
            let after = count_auto_stashes(&repo).await;
            MergeResult::Failed {
                error: err.to_string(),
                stranded_auto_stash: stash_was_stranded(auto_stashes_before, after),
            }
        }
    }
}

/// Did this merge leave an auto-stash behind?
///
/// `None` means the stash list could not be read. The bias has to point at
/// *reporting* a stash that may not exist, never at missing one that does: a
/// spurious recovery offer costs the user one click and resolves to "those
/// changes are no longer stashed", whereas a missed one is the phase's single
/// point of data loss. Unknown-before is the same question — the count it would
/// have been compared against is gone.
fn stash_was_stranded(before: Option<usize>, after: Option<usize>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => after > before,
        _ => true,
    }
}

/// How many entries on the stash stack carry the auto-stash message, or `None`
/// when the list could not be read. See [`stash_was_stranded`] for why the
/// unknown is propagated rather than flattened to zero.
async fn count_auto_stashes(repo: &Repository) -> Option<usize> {
    repo.stash_list()
        .await
        .ok()
        .map(|entries| {
            entries
                .iter()
                .filter(|e| e.message == oximux_git::AUTO_STASH_MESSAGE)
                .count()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(branch: &str) -> Workspace {
        Workspace {
            id: "ws1".into(),
            project_id: "p1".into(),
            name: "Feat".into(),
            slug: "feat".into(),
            branch: branch.into(),
            worktree_path: "/wt/feat".into(),
            status: "active".into(),
            created_at: String::new(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    #[test]
    fn every_refusal_names_the_obstacle() {
        let on_other = MergeRefusal::RootNotOnDefault {
            on: Some("release".into()),
            default: "main".into(),
        }
        .message("oximux/feat");
        assert!(on_other.contains("release"), "{on_other}");
        assert!(on_other.contains("main"), "{on_other}");

        let paused = MergeRefusal::OperationInProgress {
            operation: GitOperation::Rebase,
        }
        .message("oximux/feat");
        assert!(paused.contains("rebase"), "{paused}");

        let held = MergeRefusal::RootHeld {
            holder: PathBuf::from("/repos/app/src"),
        }
        .message("oximux/feat");
        assert!(held.contains("/repos/app/src"), "{held}");

        let nothing = MergeRefusal::NothingToMerge.message("oximux/feat");
        assert!(nothing.contains("oximux/feat"), "{nothing}");
        // Naming the branch is not enough — this one shipped as "…has nothing
        // the default branch is missing", which parses only on the second read.
        // Live verification caught it; the assertion now pins the phrasing that
        // matches the equivalent outcome headline.
        assert!(nothing.starts_with("Nothing to merge"), "{nothing}");
        assert!(nothing.contains("already has everything"), "{nothing}");
    }

    /// A detached HEAD is not a branch, and the message must not print one.
    #[test]
    fn a_detached_head_is_named_as_detached_not_as_a_branch() {
        let msg = MergeRefusal::RootNotOnDefault {
            on: None,
            default: "main".into(),
        }
        .message("oximux/feat");
        assert!(msg.contains("detached"), "{msg}");
        assert!(msg.contains("main"), "{msg}");
    }

    /// The pre-flight must reject a primary row before it opens anything —
    /// `/definitely/not/a/repo` would fail `Repository::open` and surface as
    /// `PreflightFailed` if the order were wrong.
    #[tokio::test]
    async fn a_primary_row_is_refused_before_any_repository_is_opened() {
        let mut primary = ws("main");
        primary.id = "primary:p1".into();
        let refusal = preflight_merge(
            Path::new("/definitely/not/a/repo"),
            &primary,
            "main",
            &HashSet::new(),
        )
        .await
        .expect_err("a primary row has no branch to land");
        assert_eq!(refusal, MergeRefusal::NotAWorktree);
    }

    /// The bias has to point at over-reporting. A stash list that cannot be
    /// read after a failed merge must not be silently read as "nothing was
    /// stranded" — that is the one path that loses the user's work with no
    /// record of it.
    #[test]
    fn an_unreadable_stash_list_is_treated_as_possibly_stranded() {
        assert!(stash_was_stranded(None, Some(0)));
        assert!(stash_was_stranded(Some(0), None));
        assert!(stash_was_stranded(None, None));
        // A readable pair still answers precisely.
        assert!(stash_was_stranded(Some(0), Some(1)));
        assert!(!stash_was_stranded(Some(1), Some(1)));
        assert!(!stash_was_stranded(Some(0), Some(0)));
    }

    /// A row whose `worktree_path` IS the project root is the same case wearing
    /// a different id.
    #[tokio::test]
    async fn a_row_pointing_at_the_project_root_is_refused_the_same_way() {
        let mut row = ws("main");
        row.worktree_path = "/definitely/not/a/repo".into();
        let refusal = preflight_merge(
            Path::new("/definitely/not/a/repo"),
            &row,
            "main",
            &HashSet::new(),
        )
        .await
        .expect_err("the project's own checkout");
        assert_eq!(refusal, MergeRefusal::NotAWorktree);
    }
}
