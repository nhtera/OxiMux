//! What a new worktree is cut from — and what that choice implies for
//! provisioning.
//!
//! Before this existed, `git worktree add -b <branch> <path>` ran with no
//! start-point, so every worktree branched off whatever the main checkout's
//! HEAD happened to be. Creating a worktree while sitting on a half-finished
//! feature branch silently based the new work on it, and the user found out
//! at review time.
//!
//! The two modes are an enum rather than two optional strings because the
//! fourth state — "adopt an existing branch, and also cut it from somewhere" —
//! is not a thing, and a pair of `Option`s would let a caller build it.

use oximux_git::Repository;
use oximux_settings::SetupDecision;

/// The base a new worktree is cut from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateBase {
    /// Cut a brand-new branch named `branch`.
    ///
    /// `from: None` means **the repository's default branch**, resolved by the
    /// create path, degrading to the main checkout's current HEAD only when
    /// that default does not resolve locally. It does *not* mean "HEAD": every
    /// unattended caller (the RPC, a team run, the chat pill) wants the default
    /// branch and none of them wants whatever the user happens to be sitting
    /// on, which is the defect this whole enum exists to close.
    ///
    /// `from: Some(r)` names an explicit start point. That one is strict — a
    /// ref the user asked for by name and that does not resolve is an error,
    /// not an invitation to substitute something else.
    NewBranch {
        branch: String,
        from: Option<String>,
    },
    /// Check out an **existing** branch. No branch is created and no prefix
    /// applies: the worktree adopts the branch under the name it already has.
    ExistingBranch { name: String },
}

impl CreateBase {
    /// A new branch cut from the repository's default branch (see the variant
    /// doc for the degradation).
    pub fn new_branch(branch: impl Into<String>) -> Self {
        Self::NewBranch {
            branch: branch.into(),
            from: None,
        }
    }

    /// A new branch cut from an explicit start point.
    pub fn new_branch_from(branch: impl Into<String>, from: impl Into<String>) -> Self {
        Self::NewBranch {
            branch: branch.into(),
            from: Some(from.into()),
        }
    }

    /// Adopt an existing branch.
    pub fn existing(name: impl Into<String>) -> Self {
        Self::ExistingBranch { name: name.into() }
    }

    /// The branch the workspace row will name. In both modes this is the
    /// branch the worktree ends up checked out on.
    pub fn branch(&self) -> &str {
        match self {
            Self::NewBranch { branch, .. } => branch,
            Self::ExistingBranch { name } => name,
        }
    }

    /// Whether creating this worktree *minted* the branch — and therefore
    /// whether a rollback may delete it.
    ///
    /// **This is a data-loss guard, not bookkeeping.** Rollback force-deletes
    /// the branch (`delete_branch(branch, true)`), which is correct for a
    /// branch that did not exist a moment ago and catastrophic for one the user
    /// has been working on for a week. An adopted branch must survive a failed
    /// create exactly as it was.
    pub fn creates_branch(&self) -> bool {
        matches!(self, Self::NewBranch { .. })
    }

    /// The ref this worktree is based on, when the caller named one.
    ///
    /// `None` for `NewBranch { from: None }` — not because the base is unknown
    /// (it is the default branch) but because nobody *chose* it, which is the
    /// distinction [`setup_decision`] turns on: the default branch is by
    /// definition one the user has reviewed.
    pub fn named_base(&self) -> Option<&str> {
        match self {
            Self::NewBranch { from, .. } => from.as_deref(),
            Self::ExistingBranch { name } => Some(name),
        }
    }
}

/// Whether the setup script may run for a worktree cut from this base.
///
/// Provisioning runs the *worktree's own committed* `.oximux/scripts.toml` —
/// "the branch's own copy is the one that will actually run". That is safe only
/// while every worktree branches off the user's own HEAD, which is precisely
/// the invariant [`CreateBase`] removes. Basing a worktree on a fetched
/// contributor branch would otherwise run their script as the user, unattended,
/// on any project with `auto_setup = true`.
///
/// So the rule is: **a ref the user has not reviewed never runs a setup
/// script.**
///
/// - No base was named (`NewBranch { from: None }`) → the default branch, or
///   the user's own checkout behind it. Both are the trust premise that already
///   held, so the guard has nothing to add.
/// - The named base **is** the default branch, in any of its spellings
///   (`main`, `origin/main`, `refs/remotes/upstream/main`) → reviewed by
///   definition. See [`is_the_default_branch`] for why this matters more than
///   it looks.
/// - The named base is an ancestor of the default branch → already in the
///   history the user lives on. Provision normally.
/// - Anything else, **including anything we could not check** →
///   [`SetupDecision::Skip`], with a reason to show.
///
/// An explicit [`SetupDecision::Skip`] from the caller stays `Skip` with no
/// reason of ours, and an explicit [`SetupDecision::Run`] is honoured: the
/// guard is a *default*, not a prohibition. `Run setup` from the row menu is
/// how a user opts in after reading the script.
///
/// # This fails CLOSED, and that is the whole point
///
/// An earlier draft treated "we could not establish ancestry" as "provision
/// normally", on the reasoning that inventing a refusal from ignorance is
/// rude. That reasoning is wrong here, and the asymmetry is why: failing
/// closed costs the user one click (`Run setup`), while failing open runs a
/// script from a ref they have not read, as them, unattended. A repository
/// with no discoverable default branch — no `origin/HEAD`, no local
/// `main`/`master` — is not exotic, and it is precisely the case where we know
/// least about what the base is.
///
/// The guard still applies **only to a named base**, so no caller that never
/// asks for one is affected.
pub async fn setup_decision(
    repo: &Repository,
    base: &CreateBase,
    requested: SetupDecision,
    default_branch: Option<&str>,
) -> (SetupDecision, Option<String>) {
    // The caller already decided. `Run` is a deliberate opt-in past this guard;
    // `Skip` is already the answer this could only agree with.
    if requested != SetupDecision::Inherit {
        return (requested, None);
    }
    let Some(named) = base.named_base() else {
        return (requested, None);
    };
    // A malformed ref never reaches `git merge-base`. This runs upstream of
    // `add_worktree_from`'s own validation, so without this the first `git`
    // invocation to see an unvalidated ref would be the ancestry check.
    if oximux_git::worktree::validate_ref_name(named).is_err() {
        return (
            SetupDecision::Skip,
            Some(format!("Setup skipped: `{named}` is not a usable ref name.")),
        );
    }
    let unverified = |detail: &str| {
        (
            SetupDecision::Skip,
            Some(format!(
                "Setup skipped: could not verify `{named}` against the default branch \
                 ({detail}). Review the branch, then Run setup."
            )),
        )
    };
    let Some(default) = default_branch else {
        tracing::info!(base = %named, "setup guard: no default branch to compare against");
        return unverified("this repository has no default branch");
    };
    // The default branch itself, however it was spelled — the case that would
    // otherwise produce the most false positives.
    if is_the_default_branch(repo, named, default).await {
        return (requested, None);
    }
    match repo.is_ancestor(named, default).await {
        Ok(true) => (requested, None),
        Ok(false) => (
            SetupDecision::Skip,
            Some(format!(
                "Setup skipped: `{named}` is not based on `{default}`. \
                 Review the branch, then Run setup."
            )),
        ),
        Err(err) => {
            tracing::warn!(?err, base = %named, %default, "ancestry check failed; skipping setup");
            unverified("the ancestry check failed")
        }
    }
}

/// Whether `named` is the default branch under any of the spellings a user
/// would reasonably type.
///
/// **Why an equality test is not enough.** `is_ancestor(named, default)` asks
/// "is the base contained in the default branch's history". For
/// `--from origin/main` against a local `main` that is even slightly behind the
/// remote, `origin/main` is a *descendant*, so the honest answer is `false` and
/// the guard fires. But basing work on the up-to-date remote default is the
/// single most common reason to reach for `--from` at all — it is exactly what
/// the `keep_default_up_to_date` setting produces automatically — so treating
/// it as an unreviewed contributor branch would fire the warning on the safest
/// thing a user can do, and train them to ignore it.
///
/// **Why the shape test alone is not enough either.** `origin/main` and
/// `feature/main` are structurally identical — one segment, then the default's
/// name — so no amount of string inspection separates a remote from somebody's
/// branch. `feature/main` is a branch a person made and must NOT be waved
/// through. So the shape test only decides *what to ask git*, and the answer
/// comes from whether `refs/remotes/<prefix>/<default>` actually resolves.
/// That is exact, costs one `rev-parse` on a path that already runs several,
/// and needs no list of remote names.
async fn is_the_default_branch(repo: &Repository, named: &str, default: &str) -> bool {
    if named == default {
        return true;
    }
    let Some(prefix) = remote_prefix_of(named, default) else {
        return false;
    };
    repo.sha_of(&format!("refs/remotes/{prefix}/{default}"))
        .await
        .ok()
        .flatten()
        .is_some()
}

/// The single segment in front of `default` in `named`, when `named` has the
/// shape `<one-segment>/<default>` or `refs/remotes/<one-segment>/<default>`.
///
/// Pure, and separated from the git question so the shape rule can be stated in
/// tests without a repository. A `None` here means "not even shaped like a
/// remote-tracking ref", which is a definite answer; a `Some` only means "worth
/// asking git about".
fn remote_prefix_of<'a>(named: &'a str, default: &str) -> Option<&'a str> {
    let prefix = named.strip_suffix(default)?.strip_suffix('/')?;
    let prefix = prefix.strip_prefix("refs/remotes/").unwrap_or(prefix);
    // Exactly one segment. `a/b/main` is not `<remote>/main`.
    (!prefix.is_empty() && !prefix.contains('/')).then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_names_the_branch_in_both_modes() {
        assert_eq!(CreateBase::new_branch("oximux/x").branch(), "oximux/x");
        assert_eq!(
            CreateBase::new_branch_from("oximux/x", "main").branch(),
            "oximux/x"
        );
        assert_eq!(CreateBase::existing("side").branch(), "side");
    }

    #[test]
    fn only_a_minted_branch_may_be_rolled_back() {
        assert!(CreateBase::new_branch("oximux/x").creates_branch());
        assert!(CreateBase::new_branch_from("oximux/x", "main").creates_branch());
        assert!(
            !CreateBase::existing("side").creates_branch(),
            "rollback would force-delete the user's own branch"
        );
    }

    #[test]
    fn only_a_chosen_base_is_a_named_one() {
        assert_eq!(CreateBase::new_branch("oximux/x").named_base(), None);
        assert_eq!(
            CreateBase::new_branch_from("oximux/x", "origin/pr").named_base(),
            Some("origin/pr")
        );
        assert_eq!(CreateBase::existing("side").named_base(), Some("side"));
    }
}

#[cfg(test)]
mod remote_shape_tests {
    use super::remote_prefix_of;

    /// These are the names worth asking git about. Whether they ARE the remote
    /// default is then settled by `refs/remotes/<prefix>/<default>` resolving —
    /// which is what keeps `feature/main` (same shape, real branch) out.
    #[test]
    fn a_single_segment_in_front_of_the_default_is_a_candidate() {
        assert_eq!(remote_prefix_of("origin/main", "main"), Some("origin"));
        assert_eq!(remote_prefix_of("upstream/main", "main"), Some("upstream"));
        assert_eq!(remote_prefix_of("refs/remotes/origin/main", "main"), Some("origin"));
        // Same shape, and deliberately still a candidate: only git can say that
        // no remote is called `feature`.
        assert_eq!(remote_prefix_of("feature/main", "main"), Some("feature"));
    }

    #[test]
    fn anything_that_is_not_that_shape_is_refused_outright() {
        assert_eq!(remote_prefix_of("main", "main"), None, "handled by the equality arm");
        assert_eq!(remote_prefix_of("mymain", "main"), None);
        assert_eq!(remote_prefix_of("wip/api/main", "main"), None, "two segments");
        assert_eq!(remote_prefix_of("origin/feature/main", "main"), None);
        assert_eq!(remote_prefix_of("side", "main"), None);
    }

    #[test]
    fn a_slashed_default_keeps_the_one_segment_rule() {
        assert_eq!(remote_prefix_of("origin/release/4.x", "release/4.x"), Some("origin"));
        assert_eq!(remote_prefix_of("a/b/release/4.x", "release/4.x"), None);
    }
}
