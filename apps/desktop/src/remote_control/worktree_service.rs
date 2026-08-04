//! Serving worktree create/list/remove to an authorized client — the desktop's
//! implementation of the host's [`WorktreeService`] seam.
//!
//! Reuses the New-Worktree flow's own pieces rather than paralleling them: the
//! same slug validation, the same host-derived path scheme
//! ([`workspace_ops::worktree_path`]), the same
//! [`create_workspace_with_rollback`] (git worktree + branch + DB row, with
//! rollback on storage failure), and the same pre-remove cleanup script. A
//! worktree the CLI creates is therefore exactly the row the desktop sidebar
//! lists, and vice versa.
//!
//! No GPUI hop: like [`project_provider`](super::project_provider), everything
//! here is durable data plus git subprocesses, not live view state. The sidebar
//! picks up remotely-created rows on its next rebuild (project switch or
//! restart), the same way it absorbs desktop-side changes from another window.
//!
//! [`workspace_ops::worktree_path`]: crate::shell::workspace::workspace_ops
//! [`create_workspace_with_rollback`]: workspace_ops::create_workspace_with_rollback

use oximux_git::{Repository, validate_slug};
use oximux_remote_host::{WorktreeError, WorktreeService};
use oximux_remote_proto::messages::WorktreeWire;
use oximux_storage::{ProjectRepo, WorkspaceRepo};

use crate::shell::workspace::workspace_ops::{
    self, CreateOutcome, run_cleanup_before_remove,
};

/// Manages worktrees against the same repos and path scheme the desktop uses.
pub struct RepoWorktrees {
    projects: ProjectRepo,
    workspaces: WorkspaceRepo,
}

impl RepoWorktrees {
    pub fn new(projects: ProjectRepo, workspaces: WorkspaceRepo) -> Self {
        Self { projects, workspaces }
    }

    /// Resolve a client-named project root against the desktop's own records.
    /// Exact match only — the client is echoing a path a `ListProjects` row
    /// handed it, and anything else is refused rather than guessed at.
    fn project_by_path(&self, project_path: &str) -> Result<oximux_core::Project, WorktreeError> {
        self.projects
            .get_by_root_path(project_path)
            .map_err(|err| {
                tracing::warn!(?err, "worktree service: project lookup failed");
                WorktreeError::Unavailable
            })?
            .ok_or(WorktreeError::UnknownProject)
    }
}

/// One DB row as the wire shows it.
fn wire(row: oximux_core::Workspace, project_path: &str) -> WorktreeWire {
    WorktreeWire {
        id: row.id,
        project_path: project_path.to_string(),
        name: row.name,
        slug: row.slug,
        branch: row.branch,
        path: row.worktree_path,
    }
}

#[async_trait::async_trait]
impl WorktreeService for RepoWorktrees {
    async fn create(&self, project_path: &str, slug: &str)
    -> Result<WorktreeWire, WorktreeError> {
        if validate_slug(slug).is_err() {
            return Err(WorktreeError::BadSlug);
        }
        let project = self.project_by_path(project_path)?;
        // Pre-check for a slug collision so the common conflict answers
        // `AlreadyExists` instead of a generic git failure. The git step still
        // catches the race (an existing branch fails `worktree add -b`).
        let existing = self.workspaces.list_for_project(&project.id).map_err(|err| {
            tracing::warn!(?err, "worktree service: workspace list failed");
            WorktreeError::Unavailable
        })?;
        if existing.iter().any(|w| w.slug == slug) {
            return Err(WorktreeError::AlreadyExists);
        }
        // Host-derived target: `<app_data>/projects/<project_id>/worktrees/<slug>`
        // — the client never supplies a path.
        let target = workspace_ops::worktree_path(&project.id, slug)
            .ok_or(WorktreeError::Unavailable)?;
        let outcome = workspace_ops::create_workspace_with_rollback(
            std::path::Path::new(&project.root_path),
            &project.id,
            slug,
            slug,
            &target,
            None,
            &self.workspaces,
        )
        .await;
        match outcome {
            CreateOutcome::Created(row) => Ok(wire(row, &project.root_path)),
            CreateOutcome::GitFailed(detail) => {
                tracing::warn!(%detail, slug, "remote worktree create: git step failed");
                Err(WorktreeError::CreateFailed)
            }
            CreateOutcome::StorageFailedRollbackClean(err) => {
                tracing::warn!(?err, slug, "remote worktree create: storage failed, rolled back");
                Err(WorktreeError::CreateFailed)
            }
            CreateOutcome::StorageFailedRollbackDirty { insert_error, rollback_error } => {
                tracing::error!(
                    ?insert_error,
                    %rollback_error,
                    slug,
                    "remote worktree create: storage failed AND rollback failed — manual cleanup"
                );
                Err(WorktreeError::CreateFailed)
            }
        }
    }

    async fn list(&self, project_path: Option<&str>) -> Result<Vec<WorktreeWire>, WorktreeError> {
        let projects = match project_path {
            Some(path) => vec![self.project_by_path(path)?],
            None => self.projects.list_ordered(usize::MAX).map_err(|err| {
                tracing::warn!(?err, "worktree service: project list failed");
                WorktreeError::Unavailable
            })?,
        };
        let mut rows = Vec::new();
        for project in projects {
            let list = self.workspaces.list_for_project(&project.id).map_err(|err| {
                tracing::warn!(?err, "worktree service: workspace list failed");
                WorktreeError::Unavailable
            })?;
            // DB rows only — the sidebar's synthesized primary row (the project
            // root itself) has no DB identity and must never be removable here.
            rows.extend(list.into_iter().map(|w| wire(w, &project.root_path)));
        }
        Ok(rows)
    }

    async fn remove(&self, id: &str) -> Result<(), WorktreeError> {
        let row = self.workspaces.get_by_id(id).map_err(|err| {
            tracing::warn!(?err, "worktree service: workspace lookup failed");
            WorktreeError::Unavailable
        })?;
        // Already gone: the caller's goal state is reached.
        let Some(row) = row else { return Ok(()) };
        let project = self
            .projects
            .get_by_id(&row.project_id)
            .map_err(|err| {
                tracing::warn!(?err, "worktree service: project lookup failed");
                WorktreeError::Unavailable
            })?
            .ok_or(WorktreeError::RemoveFailed)?;
        // A row whose path IS the project root would make "remove worktree"
        // delete the user's repository. No such row is ever minted by the
        // create path; refuse defensively rather than trust that forever.
        if row.worktree_path == project.root_path {
            tracing::warn!(id, "remote worktree remove refused: row points at the project root");
            return Err(WorktreeError::RemoveFailed);
        }
        let worktree_path = std::path::PathBuf::from(&row.worktree_path);
        // The project's cleanup script runs to completion (bounded) before the
        // directory goes, exactly as the desktop's own delete flow does.
        run_cleanup_before_remove(&worktree_path).await;
        let repo = Repository::open(std::path::Path::new(&project.root_path))
            .await
            .map_err(|err| {
                tracing::warn!(?err, "remote worktree remove: open repo failed");
                WorktreeError::RemoveFailed
            })?;
        // Non-force, like the desktop's first attempt: a dirty worktree is
        // preserved (row and branch intact) rather than silently destroyed.
        // Force-removal stays a desktop act with its own confirmation.
        if let Err(err) = repo.remove_worktree(&worktree_path, false).await {
            tracing::warn!(?err, slug = %row.slug, "remote worktree remove failed; row preserved");
            return Err(WorktreeError::RemoveFailed);
        }
        // Best-effort, like the desktop flow: a surviving branch is reported in
        // logs but must not strand the row, or the listing would keep showing a
        // worktree whose directory is gone.
        if let Err(err) = repo.delete_branch(&row.branch, false).await {
            tracing::warn!(?err, branch = %row.branch, "remote worktree remove: branch survives");
        }
        self.workspaces.delete(&row.id).map_err(|err| {
            tracing::warn!(?err, id, "remote worktree remove: row delete failed");
            WorktreeError::RemoveFailed
        })
    }
}
