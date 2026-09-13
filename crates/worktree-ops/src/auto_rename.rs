//! Auto-rename: a codename workspace takes the name of its work.
//!
//! A workspace created without a name (see [`crate::codename`]) is called
//! `amber` until something knows what it is for. The first thing that does is
//! the chat's generated summary of its first task — a *summary*, never the
//! hook-captured prompt, because a pasted stack trace or "ok now do the thing
//! we discussed" makes a branch name nobody wants, and a bad rename here is
//! permanent (see the one-shot rule below).
//!
//! **What moves.** The branch and the row. Not the directory. The summary
//! arrives while the agent that produced it is running *in* the worktree,
//! which is exactly the condition Phase 3's in-use refusal exists for — a
//! directory move under a live cwd orphans the process on macOS with no error
//! to catch. A branch rename does not: `git branch -m` rewrites the worktree's
//! HEAD in place. So auto-rename asks [`preflight_rename`] for a same-path
//! plan, which the engine treats as "nothing moves", and the directory keeps
//! its codename until a manual `Rename` moves it once nothing is live there.
//!
//! **One-shot, by construction.** There is no "already auto-renamed" column.
//! Once renamed, the slug is no longer a codename, so
//! [`propose_auto_rename`] declines it — and a user-typed name was never a
//! codename in the first place. Both guards are the same check.
//!
//! **Offered, not applied.** Because the guard makes a rename unrepeatable,
//! the host shows the proposal and lets the user decline it. This module
//! decides *whether* and *to what*; the host decides *when*.
//!
//! Every refusal from the engine is the host's to log and forget. The user did
//! not ask for this; a failed optimisation is not an error they need to see.

use std::collections::HashSet;
use std::path::Path;

use oximux_core::Workspace;
use oximux_git::{derive_slug, validate_slug};
use oximux_storage::WorkspaceRepo;

use crate::branch_name;
use crate::codename::is_generated_codename;
use crate::rename::{RenameOutcome, RenamePlan, RenameRefusal, apply_rename, preflight_rename};

/// Words kept from the summary. A branch name is read in `git branch`, in a
/// PR title and in the rail; four words is what fits.
pub const MAX_WORDS: usize = 4;

/// Hard cap on the derived slug, in bytes (the slug is ASCII). Four words can
/// still be forty characters; a codename is better than that.
pub const MAX_SLUG_CHARS: usize = 32;

/// Cap on the row's display label, matching the title generator's own cap so
/// nothing longer can arrive here by accident.
const MAX_NAME_CHARS: usize = 80;

/// What an auto-rename would do, decided from the row and the summary alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRenameProposal {
    /// The row's new display label: the summary, whitespace-collapsed and
    /// capped. Agent-authored prose — untrusted display text, like `comment`.
    pub new_name: String,
    /// The slug the branch will end in. Already through [`validate_slug`].
    pub new_slug: String,
    /// The full branch name, keeping the row's own prefix.
    pub new_branch: String,
}

/// Why a row was not offered a rename. For the debug log; never for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ineligible {
    /// The slug is not a codename: the user named it, or it was already
    /// auto-renamed. The one-shot rule and the "never a typed name" rule are
    /// this single variant.
    NotACodename,
    /// The branch was adopted, not minted. Someone else's branch keeps its
    /// name.
    NotMinted,
    /// A synthesized primary row — the project's own checkout.
    NotAWorktree,
    /// The summary reduced to nothing git would accept as a slug.
    NoUsableSlug,
    /// The summary happens to slugify to the codename itself.
    SameSlug,
}

/// Decide whether `workspace` should be offered a rename from `summary`, and
/// to what. Pure: no git, no filesystem, so a host can call it on the UI
/// thread and again at click time against a fresh row.
///
/// The upstream check is deliberately *not* here — it needs git, and it is
/// [`preflight_auto_rename`]'s job.
pub fn propose_auto_rename(
    workspace: &Workspace,
    summary: &str,
) -> Result<AutoRenameProposal, Ineligible> {
    if workspace.id.starts_with("primary:") {
        return Err(Ineligible::NotAWorktree);
    }
    if !is_generated_codename(&workspace.slug) {
        return Err(Ineligible::NotACodename);
    }
    if !workspace.branch_minted {
        return Err(Ineligible::NotMinted);
    }
    let new_slug = derive_auto_slug(summary).ok_or(Ineligible::NoUsableSlug)?;
    if new_slug == workspace.slug {
        return Err(Ineligible::SameSlug);
    }
    // The row's OWN prefix, for the reason `preflight_rename` gives: this is a
    // correction to a name, not a decision to re-file the branch under
    // whatever the prefix setting says today.
    let new_branch =
        branch_name::branch_name(branch_name::split_prefix(&workspace.branch), &new_slug);
    let new_name: String = summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    Ok(AutoRenameProposal {
        new_name,
        new_slug,
        new_branch,
    })
}

/// Summary prose → a slug git accepts, or `None` when nothing usable
/// survives.
///
/// Through the existing validator, not a bespoke pipeline: [`derive_slug`]
/// normalises shape but its own doc says the result "still must be validated
/// with `validate_slug` before use as a branch component". The input here is
/// agent output that becomes an argument to `git branch -m`, so the
/// validator's answer is final and a rejection is a `None`, never a fallback.
///
/// `derive_slug` substitutes the literal `"workspace"` for an input with
/// nothing usable in it. As an *auto*-rename that would silently name the
/// branch something the summary never said, so an input with no ASCII
/// letters or digits is refused before it can reach that fallback.
pub fn derive_auto_slug(summary: &str) -> Option<String> {
    let summary = summary.trim();
    if !summary.bytes().any(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let full = derive_slug(summary);
    let mut slug = full
        .split('-')
        .filter(|w| !w.is_empty())
        .take(MAX_WORDS)
        .collect::<Vec<_>>()
        .join("-");
    if slug.len() > MAX_SLUG_CHARS {
        // Cut at the last word boundary inside the cap; a single word longer
        // than the cap is cut mid-word rather than kept whole.
        // A word ending exactly at the cap is kept whole.
        let cut = if slug.as_bytes()[MAX_SLUG_CHARS] == b'-' {
            MAX_SLUG_CHARS
        } else {
            slug[..MAX_SLUG_CHARS].rfind('-').unwrap_or(MAX_SLUG_CHARS)
        };
        slug.truncate(cut);
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        return None;
    }
    validate_slug(&slug).ok()?;
    Some(slug)
}

/// Check every engine refusal for a same-path rename. Touches nothing.
///
/// The path passed to the engine is the row's *current* path, so the plan
/// carries `moved == false` and the engine skips both the directory move and
/// the in-use refusal — which is the whole reason this can run while an agent
/// is live in the worktree. No holder set is taken because none can matter.
pub async fn preflight_auto_rename(
    project_root: &Path,
    workspace: &Workspace,
    proposal: &AutoRenameProposal,
) -> Result<RenamePlan, RenameRefusal> {
    preflight_rename(
        project_root,
        workspace,
        &proposal.new_slug,
        Path::new(&workspace.worktree_path),
        &HashSet::new(),
    )
    .await
}

/// Carry out a plan from [`preflight_auto_rename`]: rename the branch, write
/// the row. The directory is untouched, so nothing here can orphan a live
/// process and there is no holder read to repeat.
pub async fn apply_auto_rename(
    project_root: &Path,
    workspace: &Workspace,
    proposal: &AutoRenameProposal,
    plan: &RenamePlan,
    workspace_repo: &WorkspaceRepo,
) -> RenameOutcome {
    apply_rename(
        project_root,
        workspace,
        &proposal.new_name,
        plan,
        &HashSet::new(),
        workspace_repo,
    )
    .await
}

/// Pre-flight and apply in one call, for hosts with no reason to offer the
/// proposal first (tests, a headless host).
pub async fn auto_rename_with_rollback(
    project_root: &Path,
    workspace: &Workspace,
    proposal: &AutoRenameProposal,
    workspace_repo: &WorkspaceRepo,
) -> RenameOutcome {
    match preflight_auto_rename(project_root, workspace, proposal).await {
        Err(refusal) => RenameOutcome::Refused(refusal),
        Ok(plan) => apply_auto_rename(project_root, workspace, proposal, &plan, workspace_repo).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slug: &str, branch: &str, minted: bool) -> Workspace {
        Workspace {
            id: "ws-1".into(),
            project_id: "p-1".into(),
            name: slug.into(),
            slug: slug.into(),
            branch: branch.into(),
            worktree_path: format!("/wt/{slug}"),
            status: "active".into(),
            created_at: String::new(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
            branch_minted: minted,
        }
    }

    // ---- eligibility, one condition per test -----------------------------

    #[test]
    fn a_codename_row_with_a_usable_summary_is_proposed() {
        let p = propose_auto_rename(&row("amber", "oximux/amber", true), "Fix login redirect")
            .unwrap();
        assert_eq!(p.new_slug, "fix-login-redirect");
        assert_eq!(p.new_branch, "oximux/fix-login-redirect");
        assert_eq!(p.new_name, "Fix login redirect");
    }

    #[test]
    fn a_suffixed_codename_is_still_eligible_and_keeps_its_prefix() {
        let p = propose_auto_rename(&row("amber-2", "nhtera/amber-2", true), "Parser edge cases")
            .unwrap();
        assert_eq!(p.new_branch, "nhtera/parser-edge-cases");
    }

    #[test]
    fn a_user_typed_name_is_never_proposed() {
        assert_eq!(
            propose_auto_rename(&row("fix-login", "oximux/fix-login", true), "Auth flow"),
            Err(Ineligible::NotACodename)
        );
    }

    #[test]
    fn an_adopted_branch_is_never_proposed() {
        assert_eq!(
            propose_auto_rename(&row("amber", "amber", false), "Auth flow"),
            Err(Ineligible::NotMinted)
        );
    }

    #[test]
    fn the_primary_row_is_never_proposed() {
        let mut ws = row("amber", "main", true);
        ws.id = "primary:p-1".into();
        assert_eq!(propose_auto_rename(&ws, "Auth flow"), Err(Ineligible::NotAWorktree));
    }

    #[test]
    fn a_summary_that_reduces_to_nothing_is_not_proposed() {
        for summary in ["", "   ", "!!!", "…", "日本語", "--", "..."] {
            assert_eq!(
                propose_auto_rename(&row("amber", "oximux/amber", true), summary),
                Err(Ineligible::NoUsableSlug),
                "{summary:?}"
            );
        }
    }

    #[test]
    fn a_summary_that_is_the_codename_is_not_proposed() {
        assert_eq!(
            propose_auto_rename(&row("amber", "oximux/amber", true), "Amber"),
            Err(Ineligible::SameSlug)
        );
    }

    /// The one-shot rule needs no column: after a rename the slug is the
    /// summary's, which is not a codename, so a second pass declines.
    #[test]
    fn a_second_proposal_after_a_rename_is_declined() {
        let before = row("amber", "oximux/amber", true);
        let p = propose_auto_rename(&before, "Fix login redirect").unwrap();
        let mut after = before.clone();
        after.slug = p.new_slug.clone();
        after.branch = p.new_branch.clone();
        after.name = p.new_name.clone();
        assert_eq!(
            propose_auto_rename(&after, "Fix login redirect, take two"),
            Err(Ineligible::NotACodename)
        );
    }

    #[test]
    fn the_display_name_is_whitespace_collapsed_and_capped() {
        let long = format!("Fix   the\n\tthing {}", "x".repeat(200));
        let p = propose_auto_rename(&row("amber", "oximux/amber", true), &long).unwrap();
        assert!(p.new_name.starts_with("Fix the thing x"));
        assert_eq!(p.new_name.chars().count(), MAX_NAME_CHARS);
    }

    // ---- summary → slug ----------------------------------------------------

    #[test]
    fn punctuation_and_case_are_normalised() {
        assert_eq!(
            derive_auto_slug("Fix: login redirect (again)!").as_deref(),
            Some("fix-login-redirect-again")
        );
    }

    #[test]
    fn non_ascii_is_dropped_and_the_rest_kept() {
        assert_eq!(derive_auto_slug("Café menu rendering").as_deref(), Some("caf-menu-rendering"));
        assert_eq!(derive_auto_slug("日本語 parser"), Some("parser".into()));
    }

    #[test]
    fn a_leading_digit_is_a_legal_slug_and_is_kept() {
        assert_eq!(derive_auto_slug("2fa enrolment flow").as_deref(), Some("2fa-enrolment-flow"));
    }

    #[test]
    fn an_empty_reduction_aborts_rather_than_falling_back_to_workspace() {
        assert_eq!(derive_auto_slug("!!!"), None);
        assert_eq!(derive_auto_slug(""), None);
        // ...but a summary that genuinely says "workspace" is kept faithfully.
        assert_eq!(derive_auto_slug("Workspace").as_deref(), Some("workspace"));
    }

    #[test]
    fn at_most_four_words_survive() {
        assert_eq!(
            derive_auto_slug("one two three four five six").as_deref(),
            Some("one-two-three-four")
        );
    }

    #[test]
    fn the_slug_is_capped_at_a_word_boundary_inside_the_limit() {
        let slug = derive_auto_slug("abcdefghij klmnopqrst uvwxyzabcd efghijklmn").unwrap();
        assert!(slug.len() <= MAX_SLUG_CHARS, "{slug:?}");
        assert_eq!(slug, "abcdefghij-klmnopqrst-uvwxyzabcd");
        // A single word longer than the cap is cut, not kept whole.
        let one = derive_auto_slug(&"a".repeat(60)).unwrap();
        assert_eq!(one.len(), MAX_SLUG_CHARS);
    }

    /// Every shape `validate_slug` rejects, fed through the pipeline: the
    /// result is either refused or a slug the validator accepts — never a
    /// bare-prefix branch and never a ref git would parse as syntax.
    #[test]
    fn every_ref_syntax_hazard_is_neutralised_or_refused() {
        let hazards = [
            "release.lock",
            "@{upstream}",
            "a..b",
            "tilde~1",
            "caret^2",
            "colon:path",
            "-leading-dash",
            ".hidden",
            "trailing.",
            "a/b/c",
            "with space",
        ];
        for input in hazards {
            if let Some(slug) = derive_auto_slug(input) {
                assert!(validate_slug(&slug).is_ok(), "{input:?} → {slug:?}");
            }
        }
        // The raw forms are what the validator refuses, and the pipeline ends
        // in that validator — pinned so a future shortcut past it fails here.
        for raw in ["x.lock", "a@{b", "a..b", "a~b", "a^b", "a:b", "-x", ".x", "x."] {
            assert!(validate_slug(raw).is_err(), "{raw:?} must be refused by the validator");
        }
    }

    #[test]
    fn the_derived_slug_always_passes_the_validator() {
        for input in [
            "Fix login redirect",
            "  spaced   out  ",
            "MixedCase Words Here",
            "numbers 123 and 456",
            "emoji 🎉 party",
        ] {
            if let Some(slug) = derive_auto_slug(input) {
                assert!(validate_slug(&slug).is_ok(), "{input:?} → {slug:?}");
            }
        }
    }
}
