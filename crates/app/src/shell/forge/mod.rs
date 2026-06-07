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

use std::path::Path;

use oximux_git::Result;

/// One CI check run for the current branch's PR. Re-exported from the `gh`
/// transport so call sites depend on the forge layer, not the CLI wrapper.
pub use oximux_git::gh::CheckRun;

pub use github_gh::GithubForge;

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

    /// CI check runs for the current branch's PR. Empty (never an error) when
    /// there is no PR, no checks, or the forge CLI is unavailable.
    async fn list_checks(&self, cwd: &Path) -> Vec<CheckRun>;

    /// Create a PR for the current branch, title + body filled from the
    /// branch's commits. Returns the PR URL on success.
    async fn create_pr(&self, cwd: &Path) -> Result<String>;

    /// Peek at the failed-job log for one check run, identified by its web
    /// `link`. `None` when the check has no associated run log (an external
    /// status context with no run id, a run with no failed jobs, or the forge
    /// CLI is unavailable).
    async fn check_log(&self, cwd: &Path, link: &str) -> Option<String>;
}
