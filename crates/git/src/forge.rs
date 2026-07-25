//! Which forge backs a repo, and the read-only queries routed to it.
//!
//! The desktop's `ForgeProvider` layer (in the `app` crate) covers the full
//! surface including the mutating calls. This module holds only the read-only
//! subset — plus, importantly, the **host detection** — so a caller that cannot
//! depend on the desktop can still ask a repo about its issues, pull requests
//! and CI checks.
//!
//! The detection lives here rather than being reimplemented per caller because
//! its ordering is load-bearing and easy to get subtly wrong: see [`detect`].
//! One implementation, two callers.
//!
//! Every query keeps the transport's graceful-degradation contract: a missing
//! CLI, an unauthenticated CLI, a repo hosted nowhere relevant, or a network
//! failure all resolve to empty/`None` rather than an error. Callers show a
//! "nothing here" state, never a failure the user cannot act on.

use std::path::Path;

use crate::gh::{CheckRun, ForgeItem, ForgeListFilter, ItemDetail};
use crate::{gh, glab};

/// Which forge hosts a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeHost {
    Github,
    Gitlab,
}

/// Pick the forge backing the repo at `cwd` from its `origin` URL, or `None`
/// when `origin` is neither a GitHub nor a GitLab host (or is absent).
///
/// **GitHub is tested first, and the order matters.** A `github.com` URL can
/// carry `gitlab` in its path (`github.com/gitlab-tools/x`), which the GitLab
/// substring match would otherwise mis-claim — sending every query to the wrong
/// CLI, which then degrades to empty, leaving a repo that looks like it simply
/// has no issues.
///
/// Returning `None` lets callers skip the forge CLIs entirely rather than
/// firing `gh` at, say, a Bitbucket repo.
///
/// Local only: this reads `git remote get-url`, no network.
pub async fn detect(cwd: &Path) -> Option<ForgeHost> {
    if gh::is_github_remote(cwd).await {
        Some(ForgeHost::Github)
    } else if glab::is_gitlab_remote(cwd).await {
        Some(ForgeHost::Gitlab)
    } else {
        None
    }
}

/// Issues or pull requests for the repo at `cwd`.
///
/// Empty when the repo is not forge-hosted, the CLI is absent or
/// unauthenticated, or nothing matches — never an error.
pub async fn list_items(
    cwd: &Path,
    kind: oximux_core::ForgeRefKind,
    filter: ForgeListFilter,
) -> Vec<ForgeItem> {
    match (detect(cwd).await, kind) {
        (Some(ForgeHost::Github), oximux_core::ForgeRefKind::Issue) => {
            gh::issue_list(cwd, filter).await
        }
        (Some(ForgeHost::Github), oximux_core::ForgeRefKind::Pull) => {
            gh::pr_list(cwd, filter).await
        }
        (Some(ForgeHost::Gitlab), oximux_core::ForgeRefKind::Issue) => {
            glab::issue_list(cwd, filter).await
        }
        (Some(ForgeHost::Gitlab), oximux_core::ForgeRefKind::Pull) => {
            glab::mr_list(cwd, filter).await
        }
        (None, _) => Vec::new(),
    }
}

/// CI check runs for the current branch's PR.
///
/// Empty when there is no PR, no checks, or the CLI is unavailable — and always
/// empty for GitLab, which has no pipeline mapping wired. Reporting no checks
/// is the honest answer there: mis-mapping pipeline stages onto
/// GitHub-Actions-shaped rows would show a status nobody can trust.
pub async fn checks(cwd: &Path) -> Vec<CheckRun> {
    match detect(cwd).await {
        Some(ForgeHost::Github) => gh::pr_checks(cwd).await,
        Some(ForgeHost::Gitlab) | None => Vec::new(),
    }
}

/// Body + author of one issue/PR — the lazy companion to [`list_items`], whose
/// rows deliberately omit the body to keep the list query lean.
///
/// `None` when the CLI cannot supply it (absent, no network, item deleted).
pub async fn item_detail(
    cwd: &Path,
    kind: oximux_core::ForgeRefKind,
    number: u64,
) -> Option<ItemDetail> {
    match detect(cwd).await? {
        ForgeHost::Github => gh::item_detail(cwd, kind, number, None).await,
        ForgeHost::Gitlab => glab::item_detail(cwd, kind, number, None).await,
    }
}
