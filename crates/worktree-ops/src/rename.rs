//! Renaming a workspace: the branch, the directory and the row, or none of them.
//!
//! A rename is the second operation in this crate that mutates three things
//! which must agree, and it follows the same shape as
//! [`create_workspace_with_rollback`](crate::create_workspace_with_rollback):
//! this module owns the ordering and the undo, so no host has to reimplement
//! either.
//!
//! Ordering is the whole design. `git worktree move` is the step that can
//! half-fail; `git branch -m` is cheap and exactly reversible. So the move goes
//! first and the branch second, and a failure at the branch step walks the move
//! back. The row is written last, because a row is the one thing that can be
//! rewritten with no filesystem consequence at all.
//!
//! Every refusal is checked *before* anything is touched. A refusal discovered
//! after a partial move is the failure mode this module exists to design out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oximux_core::Workspace;
use oximux_git::{Repository, validate_slug};
use oximux_storage::WorkspaceRepo;

use crate::paths::{path_is_within, paths_equal};

/// Why a rename was declined, with enough detail for the UI to say so.
///
/// Each variant names the thing standing in the way. A refusal that cannot name
/// its cause is indistinguishable from a bug, and the user's next question is
/// always "because of what?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameRefusal {
    /// The branch tracks an upstream. Renaming it would orphan the remote ref
    /// and break any open PR pointing at it.
    Pushed { upstream: String },
    /// A live agent or terminal is holding the worktree directory.
    ///
    /// This is a hard refusal, not advice. POSIX does not lock a directory
    /// because a process's cwd is inside it, so `git worktree move` *succeeds*
    /// on macOS under a live agent: the PTY then runs on a path git no longer
    /// records, nothing returns an error, and there is nothing for the rollback
    /// to catch. The pre-flight is the only guard that exists.
    InUse { holders: Vec<PathBuf> },
    /// The target branch name is already taken.
    BranchExists { branch: String },
    /// The target directory already exists.
    PathExists { path: PathBuf },
    /// The new name does not reduce to a usable slug.
    InvalidSlug { reason: String },
    /// A pre-flight check could not be completed (the repo would not open, or
    /// git failed to answer). Nothing was touched — reporting these as a
    /// rollback would log "rename rolled back" for a repository that never
    /// opened, which misleads whoever reads that line first.
    PreflightFailed { error: String },
    /// Git itself declined to move the worktree — it is locked, or the repo has
    /// submodules. Carries git's own text, because guessing at git's reason is
    /// worse than quoting it.
    ///
    /// This is a refusal rather than a rollback: nothing had been changed when
    /// it happened, and the user still has the label-only option. Never attempt
    /// a manual `mv` here — that desynchronises git's `gitdir` pointer and
    /// produces exactly the wedged state this module exists to remove.
    MoveRefused { error: String },
    /// The row is the project's own checkout (a synthesized primary row), not a
    /// linked worktree. There is nothing here to rename.
    NotAWorktree,
}

impl RenameRefusal {
    /// One sentence naming the obstacle, for a dialog body.
    pub fn message(&self, current_branch: &str) -> String {
        match self {
            Self::Pushed { upstream } => format!(
                "\u{201c}{current_branch}\u{201d} has been pushed to \u{201c}{upstream}\u{201d}. \
                 Renaming it would orphan the remote branch and break any open pull request."
            ),
            Self::InUse { holders } => {
                let first = holders
                    .first()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "this worktree".to_string());
                format!(
                    "An agent or terminal is still running in {first}. \
                     Close it first \u{2014} moving the directory underneath it would leave it \
                     running on a path git no longer tracks."
                )
            }
            Self::BranchExists { branch } => {
                format!("A branch named \u{201c}{branch}\u{201d} already exists.")
            }
            Self::PathExists { path } => {
                format!("{} already exists.", path.display())
            }
            Self::InvalidSlug { reason } => {
                format!("That name cannot be used as a branch name: {reason}.")
            }
            Self::MoveRefused { error } => {
                format!("Git would not move the worktree directory: {error}")
            }
            Self::PreflightFailed { error } => {
                format!("Couldn\u{2019}t check whether this rename is safe: {error}")
            }
            Self::NotAWorktree => {
                "This row is the project's own checkout, not a worktree.".to_string()
            }
        }
    }
}

/// The result of [`rename_with_rollback`].
#[derive(Debug)]
pub enum RenameOutcome {
    /// Branch, directory and row all moved. Carries the updated row.
    Renamed(Box<Workspace>),
    /// Nothing was touched, for the stated reason. The caller may still offer
    /// to change the display label alone.
    Refused(RenameRefusal),
    /// A step failed and everything was walked back. Carries git's own text.
    RolledBack { error: String },
    /// A step failed and the walk-back ALSO failed. Carries both, because this
    /// is the state a human has to repair by hand and guessing at it is worse
    /// than being told.
    RollbackFailed { error: String, rollback: String },
}

/// Rename `workspace` to `new_name`: move the worktree, rename the branch, then
/// rewrite the row — or refuse, having touched nothing.
///
/// `new_worktree_path` is supplied by the caller rather than derived here, for
/// the same reason `create_workspace_with_rollback` takes one: the path scheme
/// belongs to the host, not to this crate.
///
/// `holders` is the set of worktree directories with a live agent or terminal
/// in them, computed by the host and passed in. It is not computed here on
/// purpose: liveness is a fact only the host knows, and reimplementing it would
/// let the refusal and the rail's own "live" dot disagree with each other.
pub async fn rename_with_rollback(
    project_root: &Path,
    workspace: &Workspace,
    new_name: &str,
    new_slug: &str,
    new_worktree_path: &Path,
    holders: &HashSet<PathBuf>,
    workspace_repo: &WorkspaceRepo,
) -> RenameOutcome {
    // Single-shot convenience: pre-flight and mutate against ONE holder
    // snapshot. Correct for tests and for any host that can guarantee nothing
    // spawns underneath it; a host with live panes should call
    // [`preflight_rename`] and [`apply_rename`] separately so it can re-read
    // holders in between. See `apply_rename` for why that matters.
    match preflight_rename(project_root, workspace, new_slug, new_worktree_path, holders).await {
        Err(refusal) => RenameOutcome::Refused(refusal),
        Ok(plan) => {
            apply_rename(
                project_root,
                workspace,
                new_name,
                &plan,
                holders,
                workspace_repo,
            )
            .await
        }
    }
}

/// What the mutation phase will do, decided by [`preflight_rename`].
#[derive(Debug, Clone)]
pub struct RenamePlan {
    new_slug: String,
    new_branch: String,
    old_path: PathBuf,
    new_path: PathBuf,
    /// Whether the directory actually has to move (a label-only slug change
    /// leaves it where it is).
    moved: bool,
}

/// Check every refusal condition. Touches nothing.
///
/// Split from [`apply_rename`] because the git round-trips in here —
/// `Repository::open`, `upstream_of`, `list_branches` — are hundreds of
/// milliseconds of subprocess latency, and a host that snapshots holders once
/// and mutates after all of them has a race it cannot detect. See
/// [`apply_rename`].
pub async fn preflight_rename(
    project_root: &Path,
    workspace: &Workspace,
    new_slug: &str,
    new_worktree_path: &Path,
    holders: &HashSet<PathBuf>,
) -> Result<RenamePlan, RenameRefusal> {
    // A synthesized primary row has no DB row behind it and IS the repo's own
    // checkout; there is nothing to move.
    if workspace.id.starts_with("primary:")
        || workspace.worktree_path == project_root.to_string_lossy()
    {
        return Err(RenameRefusal::NotAWorktree);
    }
    if let Err(err) = validate_slug(new_slug) {
        return Err(RenameRefusal::InvalidSlug {
            reason: err.to_string(),
        });
    }

    let old_path = PathBuf::from(&workspace.worktree_path);
    let new_branch = format!("oximux/{new_slug}");

    let repo = match Repository::open(project_root).await {
        Ok(r) => r,
        Err(err) => {
            return Err(RenameRefusal::PreflightFailed {
                error: format!("open project repo: {err}"),
            });
        }
    };

    // "Inside", not "equal": a hand-launched agent's cwd is the raw terminal
    // directory, which is often a SUBDIRECTORY of the worktree. An equality
    // check would miss it and — on macOS, where nothing stops the move — leave
    // it running on a path git no longer records.
    if let Some(holder) = holders.iter().find(|h| path_is_within(h, &old_path)) {
        return Err(RenameRefusal::InUse {
            holders: vec![holder.clone()],
        });
    }
    match repo.upstream_of(&workspace.branch).await {
        Ok(Some(upstream)) => return Err(RenameRefusal::Pushed { upstream }),
        Ok(None) => {}
        Err(err) => {
            return Err(RenameRefusal::PreflightFailed {
                error: format!("check upstream: {err}"),
            });
        }
    }
    if new_worktree_path.exists() && !paths_equal(new_worktree_path, &old_path) {
        return Err(RenameRefusal::PathExists {
            path: new_worktree_path.to_path_buf(),
        });
    }
    if new_branch != workspace.branch {
        match repo.list_branches().await {
            Ok(branches) => {
                if branches.iter().any(|b| b.name == new_branch) {
                    return Err(RenameRefusal::BranchExists {
                        branch: new_branch,
                    });
                }
            }
            Err(err) => {
                return Err(RenameRefusal::PreflightFailed {
                    error: format!("list branches: {err}"),
                });
            }
        }
    }

    Ok(RenamePlan {
        new_slug: new_slug.to_string(),
        moved: !paths_equal(new_worktree_path, &old_path),
        new_branch,
        old_path,
        new_path: new_worktree_path.to_path_buf(),
    })
}

/// Carry out the plan: move, rename the branch, write the row — walking back on
/// any failure.
///
/// `holders_now` must be read **immediately before calling this**, not reused
/// from [`preflight_rename`]. Re-checking the same snapshot proves nothing: an
/// agent can spawn during the pre-flight's git round-trips, and on macOS the
/// move then succeeds and orphans it with no error for the rollback to catch.
/// A fresh read is the only guarantee available, and this is the last point at
/// which one is possible.
pub async fn apply_rename(
    project_root: &Path,
    workspace: &Workspace,
    new_name: &str,
    plan: &RenamePlan,
    holders_now: &HashSet<PathBuf>,
    workspace_repo: &WorkspaceRepo,
) -> RenameOutcome {
    let RenamePlan {
        new_slug,
        new_branch,
        old_path,
        new_path,
        moved,
    } = plan;
    let (moved, new_worktree_path) = (*moved, new_path.as_path());

    let repo = match Repository::open(project_root).await {
        Ok(r) => r,
        Err(err) => {
            return RenameOutcome::Refused(RenameRefusal::PreflightFailed {
                error: format!("open project repo: {err}"),
            });
        }
    };

    // The fresh holder read. Everything above this line is still reversible by
    // doing nothing; everything below it is not.
    if let Some(holder) = holders_now.iter().find(|h| path_is_within(h, old_path)) {
        return RenameOutcome::Refused(RenameRefusal::InUse {
            holders: vec![holder.clone()],
        });
    }

    // ---- Mutation. From here on, every failure walks back. ----

    if moved && let Err(err) = repo.move_worktree(old_path, new_worktree_path).await {
        // Nothing had been changed yet, so this is a refusal the user can still
        // answer with label-only — not a rollback.
        return RenameOutcome::Refused(RenameRefusal::MoveRefused {
            error: err.to_string(),
        });
    }

    if *new_branch != workspace.branch
        && let Err(err) = repo.rename_branch(&workspace.branch, new_branch).await
    {
        let error = format!("rename branch: {err}");
        if moved && let Err(back) = repo.move_worktree(new_worktree_path, old_path).await {
            return RenameOutcome::RollbackFailed {
                error,
                rollback: format!("move worktree back: {back}"),
            };
        }
        return RenameOutcome::RolledBack { error };
    }

    // The row is written last: it is the only step with no filesystem
    // consequence, so a failure here can be walked back cleanly.
    let new_path_str = new_worktree_path.to_string_lossy().to_string();
    if let Err(err) = workspace_repo.rename_full(
        &workspace.id,
        new_name,
        new_slug,
        new_branch,
        &new_path_str,
    ) {
        let error = format!("update workspace row: {err}");
        let mut rollback_errors = Vec::new();
        if *new_branch != workspace.branch
            && let Err(back) = repo.rename_branch(new_branch, &workspace.branch).await
        {
            rollback_errors.push(format!("rename branch back: {back}"));
        }
        if moved && let Err(back) = repo.move_worktree(new_worktree_path, old_path).await {
            rollback_errors.push(format!("move worktree back: {back}"));
        }
        return if rollback_errors.is_empty() {
            RenameOutcome::RolledBack { error }
        } else {
            RenameOutcome::RollbackFailed {
                error,
                rollback: rollback_errors.join("; "),
            }
        };
    }

    let mut renamed = workspace.clone();
    renamed.name = new_name.to_string();
    renamed.slug = new_slug.clone();
    renamed.branch = new_branch.clone();
    renamed.worktree_path = new_path_str;
    RenameOutcome::Renamed(Box::new(renamed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_name_the_obstacle() {
        // Every refusal message must say what is in the way — a user's next
        // question after "can't rename" is always "because of what?".
        let pushed = RenameRefusal::Pushed {
            upstream: "origin/oximux/feat".to_string(),
        };
        assert!(pushed.message("oximux/feat").contains("origin/oximux/feat"));

        let in_use = RenameRefusal::InUse {
            holders: vec![PathBuf::from("/wt/feat")],
        };
        assert!(in_use.message("oximux/feat").contains("/wt/feat"));

        let branch = RenameRefusal::BranchExists {
            branch: "oximux/taken".to_string(),
        };
        assert!(branch.message("oximux/feat").contains("oximux/taken"));

        let path = RenameRefusal::PathExists {
            path: PathBuf::from("/wt/taken"),
        };
        assert!(path.message("oximux/feat").contains("/wt/taken"));

        let slug = RenameRefusal::InvalidSlug {
            reason: "slug is empty".to_string(),
        };
        assert!(slug.message("oximux/feat").contains("slug is empty"));

        // Git's own text is quoted rather than paraphrased.
        let moved = RenameRefusal::MoveRefused {
            error: "fatal: cannot move a locked working tree".to_string(),
        };
        assert!(moved.message("oximux/feat").contains("locked working tree"));
    }

    #[test]
    fn a_pushed_refusal_names_the_branch_being_renamed() {
        let refusal = RenameRefusal::Pushed {
            upstream: "origin/oximux/fix-lgoin".to_string(),
        };
        let msg = refusal.message("oximux/fix-lgoin");
        assert!(msg.contains("oximux/fix-lgoin"));
        assert!(msg.contains("pull request"), "explains the consequence");
    }

    /// The guard that keeps a live agent from being orphaned: anything running
    /// INSIDE the worktree blocks the move, not only something sitting exactly
    /// at its root. A hand-launched agent's cwd is the raw terminal directory,
    /// which is routinely a subdirectory.
    #[test]
    fn a_holder_inside_the_worktree_counts_as_holding_it() {
        let root = tempfile::tempdir().unwrap();
        let wt = root.path().join("feat");
        std::fs::create_dir_all(wt.join("src/deep")).unwrap();

        assert!(path_is_within(&wt, &wt), "the root itself");
        assert!(path_is_within(&wt.join("src"), &wt));
        assert!(path_is_within(&wt.join("src/deep"), &wt));
    }

    /// The inverse mistake: a sibling worktree whose name merely starts with
    /// this one's must not block the rename. `/wt/feat` is a string prefix of
    /// `/wt/feature` but is not its parent.
    #[test]
    fn a_sibling_with_a_shared_name_prefix_does_not_count_as_a_holder() {
        let root = tempfile::tempdir().unwrap();
        let wt = root.path().join("feat");
        let sibling = root.path().join("feature");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(sibling.join("src")).unwrap();

        assert!(!path_is_within(&sibling, &wt));
        assert!(!path_is_within(&sibling.join("src"), &wt));
        // And a path outside entirely.
        assert!(!path_is_within(root.path(), &wt));
    }
}
