//! The worktree-management seam: creating, listing, and removing a project's
//! git worktrees, expressed without depending on the app that does it.
//!
//! The same split as [`SessionLauncher`](crate::SessionLauncher): the real work
//! needs the app's data directory, its project registry, and its workspace
//! storage — none of which this crate holds — so the dispatcher talks to this
//! trait and the app supplies the implementation.
//!
//! **The worktree target path is derived by the implementation, never accepted
//! from a client.** A caller names a project by the root path it was already
//! handed (a `ListProjects` row) and a slug; the implementation resolves the
//! project against its own records — refusing a path it does not know — and
//! composes the worktree directory itself, exactly as the desktop's own
//! New-Worktree flow does. Removal is by the row id a listing carried, so no
//! RPC on this surface ever turns a client-supplied string into a filesystem
//! path.

use oximux_remote_proto::messages::WorktreeWire;

/// Why a worktree operation could not happen.
///
/// Coarse on purpose, exactly like [`LaunchError`](crate::LaunchError): git and
/// filesystem failures routinely embed absolute host paths, so implementations
/// log the detail host-side and answer with one of these curated cases.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// The named project root is not one this host offers.
    #[error("that project is not one this host offers")]
    UnknownProject,
    /// The slug failed validation (empty, path separators, illegal characters).
    #[error("that worktree name is not usable")]
    BadSlug,
    /// A worktree (or branch) with that slug already exists for the project.
    #[error("a worktree with that name already exists")]
    AlreadyExists,
    /// The create failed past validation. Detail is logged host-side.
    #[error("the worktree could not be created")]
    CreateFailed,
    /// The removal failed. Detail is logged host-side.
    #[error("the worktree could not be removed")]
    RemoveFailed,
    /// The host cannot manage worktrees right now (no data directory, storage
    /// unavailable).
    #[error("the host cannot manage worktrees right now")]
    Unavailable,
}

/// Managing a project's git worktrees on the host.
///
/// `create` is a filesystem **and** repository write (a new directory plus a
/// new branch), and `remove` is destructive — the dispatcher gates both on the
/// dedicated full-scope, non-read-only check
/// (`AuthStore::may_manage_worktrees`), never on the session-scoped write gate:
/// these RPCs name no session, so a session-scoped caller has nothing to be
/// narrowed against and is refused outright.
#[async_trait::async_trait]
pub trait WorktreeService: Send + Sync {
    /// Create a worktree under the project rooted at `project_path`, on a fresh
    /// branch derived from `slug`. The implementation validates both arguments:
    /// the project must resolve against its own records, and the slug must pass
    /// the same validation the app's own UI applies.
    ///
    /// Returns the created row, whose `path` the caller may hand straight to a
    /// `CreateSession`.
    async fn create(&self, project_path: &str, slug: &str)
    -> Result<WorktreeWire, WorktreeError>;

    /// The worktrees of one project (or of every project when `None`).
    /// Synthesized primary rows (the project root itself) are **not** listed —
    /// only rows that [`Self::remove`] could act on.
    async fn list(&self, project_path: Option<&str>) -> Result<Vec<WorktreeWire>, WorktreeError>;

    /// Remove the worktree a listing identified as `id` — directory, branch,
    /// and row. Removing one already gone is `Ok`: the caller's goal state is
    /// reached, and racing a desktop-side removal is not an error.
    async fn remove(&self, id: &str) -> Result<(), WorktreeError>;
}
