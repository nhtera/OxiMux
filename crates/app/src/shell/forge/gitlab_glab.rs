//! `glab`-CLI implementation of [`ForgeProvider`](super::ForgeProvider), backed
//! by the wrappers in `oximux_git::glab`.
//!
//! GitLab's merge-request surface mapped onto the same forge contract as
//! [`GithubForge`](super::GithubForge): create/merge/list/state route through
//! `glab mr ...`. Stateless unit struct — all `glab`-specific behavior
//! (timeouts, non-interactive env, graceful degradation when `glab` is absent)
//! lives in the transport.
//!
//! CI checks degrade to empty: GitLab pipelines/jobs map differently from
//! GitHub Actions runs (and `check_log` relies on Actions run URLs), so the CI
//! row simply doesn't render for GitLab repos. Wiring `glab ci` is a separate,
//! larger effort — kept out of this provider deliberately (YAGNI) rather than
//! shipping a half-mapped CI surface.

use std::path::Path;

use oximux_git::Result;
use oximux_git::glab;

use super::{CheckRun, CreatePrOptions, ForgeItem, ForgeListFilter, ForgeProvider, MergeMethod};

/// Forge provider backed by the `glab` CLI. Carries no state — construct with
/// `GitlabForge`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitlabForge;

impl ForgeProvider for GitlabForge {
    async fn supports_repo(&self, cwd: &Path) -> bool {
        glab::is_gitlab_remote(cwd).await
    }

    async fn has_open_pr(&self, cwd: &Path) -> bool {
        glab::has_open_mr(cwd).await
    }

    async fn pr_state(&self, cwd: &Path) -> oximux_core::PrState {
        glab::mr_state(cwd).await
    }

    async fn list_checks(&self, _cwd: &Path) -> Vec<CheckRun> {
        // GitLab pipeline integration is a separate effort; degrade to no CI
        // row rather than mis-map GitHub-Actions-shaped checks.
        Vec::new()
    }

    async fn create_pr(&self, cwd: &Path, opts: CreatePrOptions) -> Result<String> {
        glab::mr_create(cwd, opts).await
    }

    async fn check_log(&self, _cwd: &Path, _link: &str) -> Option<String> {
        // No CI checks surfaced for GitLab (see `list_checks`), so there is no
        // run link to peek.
        None
    }

    async fn merge_pr(&self, cwd: &Path, method: MergeMethod) -> Result<()> {
        glab::mr_merge(cwd, method).await
    }

    async fn list_issues(&self, cwd: &Path, filter: ForgeListFilter) -> Vec<ForgeItem> {
        glab::issue_list(cwd, filter).await
    }

    async fn list_prs(&self, cwd: &Path, filter: ForgeListFilter) -> Vec<ForgeItem> {
        glab::mr_list(cwd, filter).await
    }
}
