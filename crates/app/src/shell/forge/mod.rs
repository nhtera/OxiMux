//! Forge-provider seam: a thin abstraction over a code-hosting forge's
//! pull-request + CI surface.
//!
//! The `gh`-CLI implementation ([`github_gh::GithubForge`]) is the only one
//! today. The trait exists so a second forge host can slot in later without
//! rewriting the Source Control and Checks call sites — every forge call in
//! the UI goes through [`ForgeProvider`], never the CLI directly.
//!
//! Kept deliberately minimal (YAGNI): one trait, one impl, only the methods
//! the current UI needs. The low-level transport stays in `oximux_git::gh`;
//! this layer is the contract the app depends on.

pub mod github_gh;
pub mod gitlab_glab;

use std::path::Path;

use oximux_git::Result;

/// One CI check run for the current branch's PR. Re-exported from the `gh`
/// transport so call sites depend on the forge layer, not the CLI wrapper.
pub use oximux_git::gh::CheckRun;

/// Options for creating a pull request. Re-exported from the transport so the
/// dialog + call sites depend on the forge layer.
pub use oximux_git::gh::CreatePrOptions;

/// PR merge strategy. Re-exported from the transport so the menu + call sites
/// depend on the forge layer.
pub use oximux_git::gh::MergeMethod;

/// Issue/PR listing types. Re-exported so the Tasks page depends on the forge
/// layer, not the CLI wrapper.
pub use oximux_git::gh::{ForgeAssignee, ForgeItem, ForgeLabel, ForgeListFilter, ForgeState};

pub use github_gh::GithubForge;
pub use gitlab_glab::GitlabForge;

/// A code-hosting forge's PR + CI operations, scoped to one working tree
/// (passed per call as `cwd`).
///
/// Network-backed methods that "can't tell" (forge CLI absent, no PR, parse
/// failure) resolve to a benign default rather than an error, so the UI shows
/// guidance instead of a broken control. Methods that *act* (create PR) return
/// a `Result` carrying the forge's own error text.
///
// Single impl, never stored as a trait object — the dyn-compatibility caveat
// of `async fn` in trait does not apply here.
#[allow(async_fn_in_trait)]
pub trait ForgeProvider {
    /// Whether this forge supports the repo at `cwd` (e.g. `origin` points at
    /// the forge's host). Used to gate the PR affordances. Any failure → false.
    async fn supports_repo(&self, cwd: &Path) -> bool;

    /// Whether the current branch already has an **open** PR. A "can't tell"
    /// result maps to false so the Create-PR control stays usable.
    async fn has_open_pr(&self, cwd: &Path) -> bool;

    /// Full PR lifecycle state for the current branch (open / merged / closed
    /// / none). Distinguishes a **merged** PR from "no PR" so the Create-PR
    /// surface can suppress a duplicate-PR offer and the Publish row can show
    /// its "PR Status" variant. Default derives from [`has_open_pr`] (open vs
    /// none) for providers that don't implement the richer query.
    async fn pr_state(&self, cwd: &Path) -> oximux_core::PrState {
        if self.has_open_pr(cwd).await {
            oximux_core::PrState::Open
        } else {
            oximux_core::PrState::None
        }
    }

    /// CI check runs for the current branch's PR. Empty (never an error) when
    /// there is no PR, no checks, or the forge CLI is unavailable.
    async fn list_checks(&self, cwd: &Path) -> Vec<CheckRun>;

    /// Create a PR for the current branch. With default (empty-title) options
    /// the title + body are filled from the branch's commits; otherwise the
    /// supplied title/body/base/draft apply. Returns the PR URL on success.
    async fn create_pr(&self, cwd: &Path, opts: CreatePrOptions) -> Result<String>;

    /// Peek at the failed-job log for one check run, identified by its web
    /// `link`. `None` when the check has no associated run log (an external
    /// status context with no run id, a run with no failed jobs, or the forge
    /// CLI is unavailable).
    async fn check_log(&self, cwd: &Path, link: &str) -> Option<String>;

    /// Merge the current branch's open PR with the chosen method. Returns the
    /// forge's error text on failure (not mergeable, checks pending, ...).
    async fn merge_pr(&self, cwd: &Path, method: MergeMethod) -> Result<()>;

    /// List the repo's issues for the Tasks page. Empty (never an error) when
    /// the forge CLI is unavailable, the repo isn't hosted there, or nothing
    /// matches — the page then shows an empty/guidance state.
    async fn list_issues(&self, cwd: &Path, filter: ForgeListFilter) -> Vec<ForgeItem>;

    /// List the repo's pull requests for the Tasks page. Same graceful-
    /// degradation contract as [`ForgeProvider::list_issues`].
    async fn list_prs(&self, cwd: &Path, filter: ForgeListFilter) -> Vec<ForgeItem>;
}

/// Host-detecting forge dispatcher: the single entry point the UI constructs
/// instead of naming a concrete provider. [`Forge::detect`] sniffs `origin`
/// (a local `git remote get-url`, no network) and routes every
/// [`ForgeProvider`] call to the matching backend.
///
/// An enum (not a `Box<dyn>`) so the trait's `async fn` methods stay usable —
/// the trait is intentionally not dyn-compatible.
#[derive(Debug, Clone, Copy)]
pub enum Forge {
    Github(GithubForge),
    Gitlab(GitlabForge),
}

/// Which forge backs the repo — the plain-data answer UI surfaces need
/// (brand glyphs, host-specific labels) without holding the provider
/// itself. Mirrors [`Forge`]'s variants 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    Github,
    Gitlab,
}

impl Forge {
    /// The provider's kind, for render-side branching (e.g. the Create-PR
    /// button's brand glyph).
    pub fn kind(&self) -> ForgeKind {
        match self {
            Forge::Github(_) => ForgeKind::Github,
            Forge::Gitlab(_) => ForgeKind::Gitlab,
        }
    }

    /// Pick the provider for the repo at `cwd` from its `origin` URL, or
    /// `None` when `origin` is neither a GitHub nor a GitLab host (or absent).
    ///
    /// GitHub is tested **first**: a `github.com` URL can carry `gitlab` in its
    /// path (e.g. `github.com/gitlab-tools/x`), which the GitLab substring
    /// match would otherwise mis-claim. Returning `None` for an unsupported
    /// remote lets callers skip the forge CLI entirely instead of firing `gh`
    /// against, say, a Bitbucket repo. This single classification also replaces
    /// the old `detect` + `supports_repo` pair (two identical `git remote`
    /// shell-outs) — `detect().is_some()` is the gate.
    pub async fn detect(cwd: &Path) -> Option<Self> {
        if oximux_git::gh::is_github_remote(cwd).await {
            Some(Forge::Github(GithubForge))
        } else if oximux_git::glab::is_gitlab_remote(cwd).await {
            Some(Forge::Gitlab(GitlabForge))
        } else {
            None
        }
    }
}

impl ForgeProvider for Forge {
    async fn supports_repo(&self, cwd: &Path) -> bool {
        match self {
            Forge::Github(f) => f.supports_repo(cwd).await,
            Forge::Gitlab(f) => f.supports_repo(cwd).await,
        }
    }

    async fn has_open_pr(&self, cwd: &Path) -> bool {
        match self {
            Forge::Github(f) => f.has_open_pr(cwd).await,
            Forge::Gitlab(f) => f.has_open_pr(cwd).await,
        }
    }

    async fn pr_state(&self, cwd: &Path) -> oximux_core::PrState {
        match self {
            Forge::Github(f) => f.pr_state(cwd).await,
            Forge::Gitlab(f) => f.pr_state(cwd).await,
        }
    }

    async fn list_checks(&self, cwd: &Path) -> Vec<CheckRun> {
        match self {
            Forge::Github(f) => f.list_checks(cwd).await,
            Forge::Gitlab(f) => f.list_checks(cwd).await,
        }
    }

    async fn create_pr(&self, cwd: &Path, opts: CreatePrOptions) -> Result<String> {
        match self {
            Forge::Github(f) => f.create_pr(cwd, opts).await,
            Forge::Gitlab(f) => f.create_pr(cwd, opts).await,
        }
    }

    async fn check_log(&self, cwd: &Path, link: &str) -> Option<String> {
        match self {
            Forge::Github(f) => f.check_log(cwd, link).await,
            Forge::Gitlab(f) => f.check_log(cwd, link).await,
        }
    }

    async fn merge_pr(&self, cwd: &Path, method: MergeMethod) -> Result<()> {
        match self {
            Forge::Github(f) => f.merge_pr(cwd, method).await,
            Forge::Gitlab(f) => f.merge_pr(cwd, method).await,
        }
    }

    async fn list_issues(&self, cwd: &Path, filter: ForgeListFilter) -> Vec<ForgeItem> {
        match self {
            Forge::Github(f) => f.list_issues(cwd, filter).await,
            Forge::Gitlab(f) => f.list_issues(cwd, filter).await,
        }
    }

    async fn list_prs(&self, cwd: &Path, filter: ForgeListFilter) -> Vec<ForgeItem> {
        match self {
            Forge::Github(f) => f.list_prs(cwd, filter).await,
            Forge::Gitlab(f) => f.list_prs(cwd, filter).await,
        }
    }
}
