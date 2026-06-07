//! `gh`-CLI implementation of [`ForgeProvider`](super::ForgeProvider), backed
//! by the wrappers in `oximux_git::gh`.
//!
//! Stateless: a unit struct whose methods delegate to the off-thread `gh`
//! runners. All `gh`-specific behavior (timeouts, exit-code semantics, graceful
//! degradation when `gh` is absent) lives in the transport; this type only maps
//! it onto the forge contract.

use std::path::Path;

use oximux_git::Result;
use oximux_git::gh;

use super::{CheckRun, ForgeProvider};

/// Forge provider backed by the `gh` CLI. Carries no state — construct with
/// `GithubForge`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GithubForge;

impl ForgeProvider for GithubForge {
    async fn supports_repo(&self, cwd: &Path) -> bool {
        gh::is_github_remote(cwd).await
    }

    async fn has_open_pr(&self, cwd: &Path) -> bool {
        gh::has_open_pr(cwd).await
    }

    async fn list_checks(&self, cwd: &Path) -> Vec<CheckRun> {
        gh::pr_checks(cwd).await
    }

    async fn create_pr(&self, cwd: &Path) -> Result<String> {
        gh::pr_create(cwd).await
    }

    async fn check_log(&self, cwd: &Path, link: &str) -> Option<String> {
        let run_id = gh::run_id_from_link(link)?;
        gh::run_log(cwd, run_id).await
    }
}
