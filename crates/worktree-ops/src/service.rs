//! Serving worktree create/list/remove to an authorized client — the host's
//! implementation of remote-host's [`WorktreeService`] seam.
//!
//! Reuses the New-Worktree flow's own pieces rather than paralleling them: the
//! same slug validation, the same host-derived path scheme
//! ([`worktree_path`]), the same [`create_workspace_with_rollback`] (git
//! worktree + branch + DB row, with rollback on storage failure), and the same
//! pre-remove cleanup script. A worktree the CLI creates is therefore exactly
//! the row the desktop sidebar lists, and vice versa.
//!
//! No view state anywhere: everything here is durable data plus git
//! subprocesses, which is what lets `oximux serve` host it unchanged. The
//! desktop sidebar picks up remotely-created rows on its next rebuild (project
//! switch or restart), the same way it absorbs changes from another window.

use std::path::PathBuf;

use oximux_git::{Repository, validate_slug};
use oximux_remote_host::{WorktreeError, WorktreeService};
use oximux_remote_proto::messages::{WorktreeProgressWire, WorktreeWire};
use oximux_storage::{ProjectRepo, WorkspaceRepo};

use crate::{CreateOutcome, create_workspace_with_rollback, run_cleanup_before_remove, worktree_path};

/// Manages worktrees against the same repos and path scheme the desktop uses.
pub struct RepoWorktrees {
    projects: ProjectRepo,
    workspaces: WorkspaceRepo,
    /// The root new worktrees are derived under. The desktop passes its app
    /// data dir; `oximux serve` passes its `--data-dir`, so a server keeps its
    /// worktrees under its own root.
    data_dir: PathBuf,
}

impl RepoWorktrees {
    pub fn new(projects: ProjectRepo, workspaces: WorkspaceRepo, data_dir: PathBuf) -> Self {
        Self { projects, workspaces, data_dir }
    }

    /// Resolve a client-named project root against the host's own records.
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
        // Host-derived target: `<data_dir>/projects/<project_id>/worktrees/<slug>`
        // — the client never supplies a path.
        let target = worktree_path(&self.data_dir, &project.id, slug);
        let outcome = create_workspace_with_rollback(
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

    async fn set_progress(
        &self,
        id: &str,
        comment: Option<&str>,
        phase: Option<&str>,
    ) -> Result<(), WorktreeError> {
        // Two statements rather than one dynamic UPDATE: `None` means "leave
        // this alone", and composing that into SQL costs more than it saves
        // for two columns. A caller setting both is one extra round trip
        // against a local SQLite file.
        //
        // The id is checked by whichever write runs first, so a request that
        // sets nothing at all (both `None`) is a no-op rather than a validated
        // one — harmless, and the CLI refuses that shape before it gets here.
        let mut matched = None;
        if let Some(text) = comment {
            matched = Some(self.workspaces.set_comment(id, text).map_err(|err| {
                tracing::warn!(?err, "worktree service: comment write failed");
                WorktreeError::Unavailable
            })?);
        }
        if let Some(text) = phase {
            let hit = self.workspaces.set_phase(id, text).map_err(|err| {
                tracing::warn!(?err, "worktree service: phase write failed");
                WorktreeError::Unavailable
            })?;
            matched = Some(matched.unwrap_or(true) && hit);
        }
        match matched {
            Some(false) => Err(WorktreeError::UnknownWorktree),
            _ => Ok(()),
        }
    }

    async fn list_progress(
        &self,
        project_path: Option<&str>,
    ) -> Result<Vec<WorktreeProgressWire>, WorktreeError> {
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
            // Silent rows are omitted: a worktree nobody has described yet has
            // nothing to say, and shipping a row of empty strings for each one
            // would make "no progress reported" indistinguishable from "every
            // worktree reported emptiness".
            rows.extend(
                list.into_iter()
                    .filter(|w| !w.comment.is_empty() || !w.phase.is_empty())
                    .map(|w| WorktreeProgressWire {
                        id: w.id,
                        comment: w.comment,
                        phase: w.phase,
                    }),
            );
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
        let worktree_dir = std::path::PathBuf::from(&row.worktree_path);
        // The project's cleanup script runs to completion (bounded) before the
        // directory goes, exactly as the desktop's own delete flow does.
        run_cleanup_before_remove(&worktree_dir).await;
        let repo = Repository::open(std::path::Path::new(&project.root_path))
            .await
            .map_err(|err| {
                tracing::warn!(?err, "remote worktree remove: open repo failed");
                WorktreeError::RemoveFailed
            })?;
        // Non-force, like the desktop's first attempt: a dirty worktree is
        // preserved (row and branch intact) rather than silently destroyed.
        // Force-removal stays a desktop act with its own confirmation.
        if let Err(err) = repo.remove_worktree(&worktree_dir, false).await {
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


#[cfg(test)]
mod progress_tests {
    use super::*;
    use oximux_storage::db::open_memory;
    use oximux_storage::repositories::{ProjectRepo, WorkspaceRepo};

    /// A service over an in-memory database with one project and two
    /// worktrees. Returns the service, the project root, and both ids.
    fn fixture() -> (RepoWorktrees, String, String, String) {
        let db = open_memory().expect("db");
        let projects = ProjectRepo::new(db.clone());
        let workspaces = WorkspaceRepo::new(db.clone());
        let project = projects.insert("p", "/p", "main").expect("project");
        let a = workspaces.insert(&project.id, "a", "a", "oximux/a", "/p/a").expect("a");
        let b = workspaces.insert(&project.id, "b", "b", "oximux/b", "/p/b").expect("b");
        let service = RepoWorktrees::new(projects, workspaces, "/data".into());
        (service, project.root_path, a.id, b.id)
    }

    /// Only worktrees that have said something appear. A silent worktree must
    /// not ship a row of empty strings, or "nobody reported" and "everybody
    /// reported nothing" become the same answer.
    #[tokio::test]
    async fn the_board_lists_only_worktrees_that_have_spoken() {
        let (service, root, a, _b) = fixture();
        assert!(service.list_progress(None).await.expect("list").is_empty());

        service.set_progress(&a, Some("rebasing"), Some("in-progress")).await.expect("set");
        let rows = service.list_progress(Some(&root)).await.expect("list");
        assert_eq!(rows.len(), 1, "the silent worktree must not appear");
        assert_eq!(rows[0].id, a);
        assert_eq!(rows[0].comment, "rebasing");
        assert_eq!(rows[0].phase, "in-progress");
    }

    /// `None` means "leave this alone". An agent advancing its phase must not
    /// blank the sentence it wrote earlier — the reason both fields are
    /// `Option` rather than plain strings.
    #[tokio::test]
    async fn setting_one_field_leaves_the_other_standing() {
        let (service, _root, a, _b) = fixture();
        service.set_progress(&a, Some("running the suite"), Some("in-progress")).await.expect("a");
        service.set_progress(&a, None, Some("in-review")).await.expect("b");

        let rows = service.list_progress(None).await.expect("list");
        assert_eq!(rows[0].comment, "running the suite", "the phase write clobbered the comment");
        assert_eq!(rows[0].phase, "in-review");
    }

    /// An empty string is a clear, distinct from `None`'s "leave it".
    #[tokio::test]
    async fn an_empty_string_clears_and_drops_the_row() {
        let (service, _root, a, _b) = fixture();
        service.set_progress(&a, Some("done here"), None).await.expect("set");
        service.set_progress(&a, Some(""), None).await.expect("clear");
        assert!(
            service.list_progress(None).await.expect("list").is_empty(),
            "a cleared worktree has nothing to say and leaves the board"
        );
    }

    /// Naming a row that does not exist is an error, not a silent success —
    /// otherwise `worktree set` on a typo'd id reports that it worked.
    #[tokio::test]
    async fn writing_an_unknown_id_is_refused() {
        let (service, _root, _a, _b) = fixture();
        assert!(matches!(
            service.set_progress("no-such-id", Some("hello"), None).await,
            Err(WorktreeError::UnknownWorktree)
        ));
    }

    /// The store keeps a phase this build does not know. Validation lives at
    /// the write edges; if the store rejected unknown values, a newer peer's
    /// phase would be erased by an older one that merely read and rewrote it.
    #[tokio::test]
    async fn a_phase_from_a_newer_peer_survives_this_build() {
        let (service, _root, a, _b) = fixture();
        service.set_progress(&a, None, Some("shipped")).await.expect("set");
        let rows = service.list_progress(None).await.expect("list");
        assert_eq!(rows[0].phase, "shipped");
    }
}
