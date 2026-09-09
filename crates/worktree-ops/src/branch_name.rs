//! The one place a [`GitSettings`] and a slug become a branch name.
//!
//! Five sites used to write `format!("oximux/{slug}")` — the git layer, this
//! crate's create path, the workspace dialog's preview, the chat pill's
//! preview, and the chat pill's actual create. A sixth arrived with rename.
//! Six copies of a rule is six chances for the preview to promise one branch
//! and the create to make another, and the user only finds out afterwards, in
//! `git branch`.
//!
//! **Why here and not in `oximux-settings`.** Resolving
//! [`BranchPrefixMode::GitUsername`] means running `git config user.name`.
//! `oximux-settings` has no git dependency and must not gain one: it is on
//! `oximux-cli`'s dependency path, and pulling the git process layer through
//! that edge is the coupling `oximux-worktree-ops` was extracted to avoid.
//! Spawning a bare `std::process::Command` from the settings crate instead
//! would dodge `GitCmd` — and with it `no_window`, flashing a console on
//! Windows. So the serde shape lives there and the resolution lives here,
//! in a crate that already depends on both.

use oximux_git::Repository;
use oximux_git::worktree::derive_slug;
use oximux_settings::git::{BranchPrefixMode, GitSettings};

/// Resolve the configured prefix to actual text, or `None` for no prefix.
///
/// **Total by construction.** Every degradation has a defined answer and none
/// of them is an error: this runs on the create path, and a branch that cannot
/// be named is a worktree that cannot be made. An unset `user.name`, a
/// username that slugifies to nothing, a custom prefix someone typed a `~`
/// into — each falls back to [`DEFAULT_PREFIX`](oximux_settings::git::DEFAULT_PREFIX),
/// which is the prefix every existing OxiMux branch already carries.
///
/// [`BranchPrefixMode::None`] is the one case that yields `None`, and it means
/// what it says — the branch is the bare slug. It is a choice, not a failure,
/// so it does not degrade to anything.
pub async fn resolve_prefix(settings: &GitSettings, repo: &Repository) -> Option<String> {
    // Only one mode costs a subprocess, so only one mode pays for it. A git
    // failure and an unset name are the same answer to `resolve_prefix_with`:
    // we do not know the user's name.
    let username = if settings.branch_prefix == BranchPrefixMode::GitUsername {
        repo.user_name().await.ok().flatten()
    } else {
        None
    };
    resolve_prefix_with(settings, username.as_deref())
}

/// The rule itself, with the git username already in hand.
///
/// Split out because the desktop has to answer this question *while painting*
/// — a preview line cannot await a subprocess — and because it is what makes
/// the whole resolver testable without a repository. `None` for `username`
/// means "unset, or we could not ask", which is a degradation with a defined
/// answer, not an error.
pub fn resolve_prefix_with(settings: &GitSettings, username: Option<&str>) -> Option<String> {
    match settings.branch_prefix {
        BranchPrefixMode::None => None,
        BranchPrefixMode::Custom => Some(sanitize_prefix(&settings.custom_prefix)),
        BranchPrefixMode::GitUsername => Some(sanitize_prefix(username.unwrap_or_default())),
    }
}

/// Force `raw` into something usable as a single ref segment, falling back to
/// the shipped prefix when nothing survives.
///
/// `derive_slug` is reused rather than reimplemented: it already lowercases,
/// replaces every byte outside `[a-z0-9]` with `-`, collapses runs and trims —
/// which is exactly what "Ada Lovelace" or "ada.lovelace@corp" has to become
/// before it can sit in front of a `/`. Its own fallback (`"workspace"`) is
/// wrong for a prefix, so an empty input is caught before it is reached.
fn sanitize_prefix(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return oximux_settings::git::DEFAULT_PREFIX.to_string();
    }
    let slug = derive_slug(trimmed);
    // `derive_slug` substitutes `"workspace"` for an input with nothing usable
    // in it (`"!!!"`, `"---"`). As a *prefix* that would silently file the
    // user's branches under a word they never typed, so the shipped prefix is
    // the better failure.
    if slug.is_empty() || slug == "workspace" {
        return oximux_settings::git::DEFAULT_PREFIX.to_string();
    }
    slug
}

/// Join a resolved prefix and a slug into the branch name git will be given.
pub fn branch_name(prefix: Option<&str>, slug: &str) -> String {
    match prefix {
        Some(p) => format!("{p}/{slug}"),
        None => slug.to_string(),
    }
}

/// The prefix segment of an existing branch name, or `None` when it has none.
///
/// The inverse of [`branch_name`], and the answer for any mutation of a branch
/// that already exists: renaming `nhtera/fix-lgoin` must produce
/// `nhtera/fix-login`, not whatever the *current* setting would mint. A rename
/// is a correction to a name, not a decision to re-file someone's work under a
/// different convention — and doing it silently, as part of fixing a typo, is
/// how a repository ends up with one worktree's history split across two
/// prefixes.
pub fn split_prefix(branch: &str) -> Option<&str> {
    branch.split_once('/').map(|(prefix, _)| prefix).filter(|p| !p.is_empty())
}

/// Resolve settings + slug straight to the branch name, for callers that have
/// a repository in hand and no reason to hold the prefix separately.
pub async fn resolve_branch_name(settings: &GitSettings, repo: &Repository, slug: &str) -> String {
    branch_name(resolve_prefix(settings, repo).await.as_deref(), slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::git::DEFAULT_PREFIX;

    fn with(mode: BranchPrefixMode, custom: &str) -> GitSettings {
        GitSettings {
            branch_prefix: mode,
            custom_prefix: custom.to_string(),
            ..GitSettings::shipped()
        }
    }

    #[test]
    fn a_plain_username_becomes_a_plain_prefix() {
        assert_eq!(sanitize_prefix("nhtera"), "nhtera");
    }

    #[test]
    fn spaces_and_capitals_are_slugified() {
        assert_eq!(sanitize_prefix("Ada Lovelace"), "ada-lovelace");
    }

    /// The characters that make a ref ambiguous — the same set `validate_slug`
    /// screens for — must not survive into the prefix half, where nothing
    /// downstream would look for them.
    #[test]
    fn ref_syntax_in_a_name_is_replaced_not_carried_through() {
        for raw in ["ada~1", "ada^", "a:b", "ada@{1}", "a..b", "a/b"] {
            let out = sanitize_prefix(raw);
            for bad in ['~', '^', ':', '@', '{', '}', '/'] {
                assert!(!out.contains(bad), "{raw:?} produced {out:?}");
            }
            assert!(oximux_git::worktree::validate_slug(&out).is_ok(), "{raw:?} → {out:?}");
        }
    }

    #[test]
    fn an_empty_or_unusable_name_falls_back_to_the_shipped_prefix() {
        for raw in ["", "   ", "!!!", "---", "@@@"] {
            assert_eq!(sanitize_prefix(raw), DEFAULT_PREFIX, "{raw:?}");
        }
    }

    /// `derive_slug` answers `"workspace"` for an input with nothing usable in
    /// it. That is a sane *slug* fallback and a bad *prefix* — branches filed
    /// under a word the user never typed, with no way to tell it from someone
    /// who genuinely set `workspace` as their prefix. Named because it is the
    /// one place this function deliberately disagrees with the helper it wraps.
    #[test]
    fn the_slug_fallback_word_is_not_borrowed_as_a_prefix() {
        assert_eq!(derive_slug("!!!"), "workspace");
        assert_eq!(sanitize_prefix("!!!"), DEFAULT_PREFIX);
    }

    /// A rename has to keep the branch under the prefix it is already on. The
    /// round trip is what guarantees it: whatever `branch_name` joined,
    /// `split_prefix` gives back, so re-slugging the tail cannot move the
    /// branch to another prefix.
    #[test]
    fn a_prefix_survives_the_round_trip_that_a_rename_makes() {
        for prefix in [Some("oximux"), Some("nhtera"), None] {
            let joined = branch_name(prefix, "fix-lgoin");
            assert_eq!(split_prefix(&joined), prefix, "{joined:?}");
            let renamed = branch_name(split_prefix(&joined), "fix-login");
            assert_eq!(renamed, branch_name(prefix, "fix-login"));
        }
    }

    /// A row whose branch is malformed — empty, or a leading slash from some
    /// older write — must read as "no prefix" rather than as an empty one,
    /// which would join back into the `/slug` that `validate_branch_name`
    /// rejects.
    #[test]
    fn a_malformed_branch_reads_as_no_prefix_rather_than_an_empty_one() {
        for raw in ["", "feat", "/feat"] {
            assert_eq!(split_prefix(raw), None, "{raw:?}");
            let rejoined = branch_name(split_prefix(raw), "feat");
            assert!(oximux_git::worktree::validate_branch_name(&rejoined).is_ok(), "{raw:?}");
        }
    }

    #[test]
    fn an_unset_git_username_degrades_to_the_shipped_prefix() {
        let s = with(BranchPrefixMode::GitUsername, "ignored");
        assert_eq!(resolve_prefix_with(&s, None).as_deref(), Some(DEFAULT_PREFIX));
        assert_eq!(resolve_prefix_with(&s, Some("  ")).as_deref(), Some(DEFAULT_PREFIX));
        assert_eq!(resolve_prefix_with(&s, Some("Ada Lovelace")).as_deref(), Some("ada-lovelace"));
    }

    /// `custom_prefix` is kept across mode switches so the user's text is not
    /// destroyed by a control they were only looking at — which means the
    /// other two modes have to ignore it rather than fall back to it.
    #[test]
    fn the_kept_custom_text_is_ignored_by_the_other_modes() {
        let none = with(BranchPrefixMode::None, "kept");
        assert_eq!(resolve_prefix_with(&none, Some("ada")), None);
        let username = with(BranchPrefixMode::GitUsername, "kept");
        assert_eq!(resolve_prefix_with(&username, Some("ada")).as_deref(), Some("ada"));
    }

    #[test]
    fn none_means_a_bare_slug_not_a_degraded_prefix() {
        assert_eq!(branch_name(None, "feat"), "feat");
        assert_eq!(branch_name(Some("oximux"), "feat"), "oximux/feat");
    }

    /// Whatever the mode and whatever the input, the joined name has to be
    /// something `add_worktree` will accept — that call is the only consumer,
    /// and a name it rejects is a create that fails with a git error rather
    /// than a settings problem the user could act on.
    #[test]
    fn every_resolvable_prefix_produces_a_valid_branch_name() {
        for custom in ["oximux", "Ada Lovelace", "", "!!!", "a~b", "team/sub"] {
            let s = with(BranchPrefixMode::Custom, custom);
            let name = branch_name(Some(&sanitize_prefix(&s.custom_prefix)), "feat");
            assert!(
                oximux_git::worktree::validate_branch_name(&name).is_ok(),
                "custom {custom:?} produced {name:?}"
            );
        }
        assert!(oximux_git::worktree::validate_branch_name(&branch_name(None, "feat")).is_ok());
    }
}
