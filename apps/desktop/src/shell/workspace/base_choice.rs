//! What the create dialog's **From** control means, as data.
//!
//! Split from the dialog for the reason `workspace_list_render` is split from
//! its painter: the interesting part is a decision — which of three
//! [`CreateBase`] shapes a user's two dropdowns add up to — and a decision is
//! testable where a GPUI render is not.

use oximux_worktree_ops::CreateBase;

/// Which of the two creation modes the segmented control is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BaseMode {
    /// Cut a new branch, named by the configured prefix and the slug.
    #[default]
    NewBranch,
    /// Adopt a branch that already exists. No branch is created and no prefix
    /// applies — the name is the user's, not ours.
    ExistingBranch,
}

/// The dialog's base selection, before it knows the branch name it will mint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseChoice {
    pub mode: BaseMode,
    /// The chosen start point. `None` means "the project's default branch" —
    /// the default the control opens on, kept as `None` rather than eagerly
    /// resolved so a project switch re-reads it instead of carrying the
    /// previous project's answer.
    pub from: Option<String>,
    /// The chosen existing branch. `None` in `ExistingBranch` mode means the
    /// user has not picked one yet, which is why [`Self::resolve`] can fail.
    pub existing: Option<String>,
}

impl BaseChoice {
    /// The label the **From** control shows: the explicit choice, else the
    /// project's default branch, else a plain statement of the fallback rather
    /// than a branch name we cannot name.
    pub fn from_label<'a>(&'a self, default_branch: &'a str) -> &'a str {
        match self.from.as_deref() {
            Some(chosen) => chosen,
            None if !default_branch.is_empty() => default_branch,
            None => "current checkout",
        }
    }

    /// Whether this choice is complete enough to submit.
    ///
    /// Only `ExistingBranch` can be incomplete: adopting a branch requires
    /// naming one, where a new branch always has a name (the slug) and always
    /// has a base (the default, or the checkout behind it).
    pub fn is_complete(&self) -> bool {
        match self.mode {
            BaseMode::NewBranch => true,
            BaseMode::ExistingBranch => self.existing.is_some(),
        }
    }

    /// Fold the choice and the branch name the resolver minted into the base
    /// the create path takes.
    ///
    /// `minted_branch` is ignored in `ExistingBranch` mode — deliberately, and
    /// this is the whole reason the modes are one enum rather than a pair of
    /// fields. Adopting a branch and *also* naming a new one is not a state a
    /// caller should be able to build.
    ///
    /// **"Default branch" stays unresolved here.** `CreateBase::new_branch`
    /// already means "the repository's default branch, or HEAD if it does not
    /// resolve", and answering that question needs `git`. Resolving it in this
    /// pure function would mean either a stale stored value or a second
    /// resolution free to disagree with the create path's own.
    ///
    /// `None` when the choice is incomplete; the caller has already disabled
    /// submit, so reaching this is a bug rather than a user error.
    pub fn resolve(&self, minted_branch: String) -> Option<CreateBase> {
        match self.mode {
            BaseMode::ExistingBranch => self.existing.clone().map(CreateBase::existing),
            BaseMode::NewBranch => Some(match self.from.as_deref() {
                Some(from) => CreateBase::new_branch_from(minted_branch, from),
                None => CreateBase::new_branch(minted_branch),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_branch_defers_the_default_to_the_create_path() {
        let choice = BaseChoice::default();
        assert_eq!(
            choice.resolve("oximux/x".into()),
            Some(CreateBase::new_branch("oximux/x")),
            "the default branch is resolved with git at create time, not here"
        );
    }

    #[test]
    fn an_explicit_from_wins_over_the_default() {
        let choice = BaseChoice {
            from: Some("release/1.4".into()),
            ..Default::default()
        };
        assert_eq!(
            choice.resolve("oximux/x".into()),
            Some(CreateBase::new_branch_from("oximux/x", "release/1.4"))
        );
    }

    #[test]
    fn existing_mode_adopts_the_branch_and_discards_the_minted_name() {
        let choice = BaseChoice {
            mode: BaseMode::ExistingBranch,
            existing: Some("feature/api/retry".into()),
            // Left set on purpose: switching modes must not carry the other
            // mode's answer into the result.
            from: Some("release/1.4".into()),
        };
        assert_eq!(
            choice.resolve("oximux/x".into()),
            Some(CreateBase::existing("feature/api/retry"))
        );
    }

    #[test]
    fn existing_mode_without_a_branch_is_incomplete() {
        let choice = BaseChoice {
            mode: BaseMode::ExistingBranch,
            ..Default::default()
        };
        assert!(!choice.is_complete());
        assert_eq!(choice.resolve("oximux/x".into()), None);
        assert!(BaseChoice::default().is_complete());
    }

    #[test]
    fn the_from_label_names_what_the_create_will_actually_do() {
        let mut choice = BaseChoice::default();
        assert_eq!(choice.from_label("main"), "main");
        assert_eq!(choice.from_label(""), "current checkout");
        choice.from = Some("dev".into());
        assert_eq!(choice.from_label("main"), "dev");
    }
}
