//! Serving worktree create/list/remove to an authorized client — the host's
//! implementation of remote-host's [`WorktreeService`] seam.
//!
//! Reuses the New-Worktree flow's own pieces rather than paralleling them: the
//! same slug validation, the same [`create_workspace_with_rollback`] (git
//! worktree + branch + DB row, with rollback on storage failure), and the same
//! pre-remove cleanup script. A worktree the CLI creates is therefore exactly
//! the row the desktop sidebar lists, and vice versa.
//!
//! **The path scheme here is host-derived, unconditionally.** This service
//! constructs its own [`HostDerivedLocator`] and takes no locator from a
//! caller, so a client names a project and a slug and never a location — the
//! property the remote surface promises, kept even now that the desktop's own
//! creates resolve a configured root.
//!
//! No view state anywhere: everything here is durable data plus git
//! subprocesses, which is what lets `oximux serve` host it unchanged. The
//! desktop sidebar picks up remotely-created rows on its next rebuild (project
//! switch or restart), the same way it absorbs changes from another window.

use std::path::PathBuf;

use oximux_git::{Repository, validate_slug};
use oximux_git::worktree::{derive_slug, validate_ref_name};
use oximux_remote_host::{WorktreeError, WorktreeService};
use oximux_remote_proto::messages::{CreateBaseWire, WorktreeProgressWire, WorktreeWire};
use oximux_storage::{ProjectRepo, WorkspaceRepo};

use crate::branch_name;
use crate::{
    CreateBase, CreateOutcome, HostDerivedLocator, Provision, WorktreeLocator,
    create_workspace_with_rollback, run_cleanup_before_remove,
};

/// Manages worktrees against the same repos and path scheme the desktop uses.
pub struct RepoWorktrees {
    projects: ProjectRepo,
    workspaces: WorkspaceRepo,
    /// The root new worktrees are derived under, and where `git.toml` is
    /// read from. The desktop passes its app data dir; `oximux serve` passes
    /// its `--data-dir`, so a server keeps its worktrees under its own root.
    data_dir: PathBuf,
    /// Host-derived, built here from `data_dir` and nowhere else.
    locator: HostDerivedLocator,
}

impl RepoWorktrees {
    pub fn new(projects: ProjectRepo, workspaces: WorkspaceRepo, data_dir: PathBuf) -> Self {
        let locator = HostDerivedLocator::new(data_dir.clone());
        Self { projects, workspaces, data_dir, locator }
    }

    /// Where a create for `slug` in `project` lands. Host-derived by
    /// construction; crate-visible so a test can pin that it stays so. The
    /// `expect` is the contract: [`HostDerivedLocator::locate`] validates
    /// nothing and cannot refuse.
    pub(crate) fn target_path(&self, project: &oximux_core::Project, slug: &str) -> PathBuf {
        self.locator
            .locate(project, slug)
            .expect("the host-derived locator validates nothing and cannot refuse")
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

/// The slug that will name the directory and the row.
///
/// The slug names both in **every** mode — only the branch changes — so
/// adopting a branch still needs one. An empty slug in that mode means "derive
/// it from the branch": a client asking for `--branch feature/api/retry` should
/// not have to say the directory twice.
///
/// **The derivation lives here, on the host, deliberately.** `derive_slug` is
/// in `oximux-git`, which is kept off `oximux-cli`'s dependency path — the same
/// edge this crate was extracted to protect. A client-side copy of the rule
/// would be a second naming implementation free to disagree with this one, and
/// the disagreement would only show up as a directory named something the user
/// did not expect.
///
/// The result is validated by the caller exactly as an explicit slug is, so a
/// branch that derives to something `validate_slug` refuses is refused too,
/// rather than reaching git.
fn effective_slug(slug: &str, base: &CreateBaseWire) -> String {
    match (slug, base) {
        ("", CreateBaseWire::Existing(branch)) => derive_slug(branch),
        (slug, _) => slug.to_string(),
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
    async fn create(
        &self,
        project_path: &str,
        slug: &str,
        base: &CreateBaseWire,
    ) -> Result<WorktreeWire, WorktreeError> {
        // A base ref is validated here as well as at the git layer: `BadSlug`
        // is a refusal the client can act on, where a git failure two layers
        // down arrives as the opaque `CreateFailed`.
        match base {
            CreateBaseWire::Default => {}
            CreateBaseWire::From(r) | CreateBaseWire::Existing(r) => {
                if validate_ref_name(r).is_err() {
                    return Err(WorktreeError::BadSlug);
                }
            }
        }
        let slug = effective_slug(slug, base);
        let slug = slug.as_str();
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
        // Adoption needs a local branch, and saying so HERE is what makes the
        // refusal readable. `add_worktree_existing` enforces the same rule, but
        // its error arrives as `CreateFailed` — i.e. `Internal("the worktree
        // could not be created")` — which tells a user who typed
        // `--branch origin/main` nothing. Same reasoning as the slug pre-check
        // above: classify the common mistake so the client can act on it.
        if let CreateBaseWire::Existing(name) = base {
            let root = std::path::Path::new(&project.root_path);
            let local = match Repository::open(root).await {
                Ok(repo) => repo
                    .sha_of(&format!("refs/heads/{name}"))
                    .await
                    .ok()
                    .flatten()
                    .is_some(),
                // Cannot open the repo: let the create below report the real
                // failure rather than blaming the branch name for it.
                Err(_) => true,
            };
            if !local {
                return Err(WorktreeError::NoSuchLocalBranch);
            }
        }
        // Host-derived target: `<data_dir>/projects/<project_id>/worktrees/<slug>`
        // — the client never supplies a path.
        let target = self.target_path(&project, slug);
        // Same branch name the desktop would mint for this slug: one resolver,
        // reading the same `git.toml`. A remote-created worktree that carried
        // the hardcoded `oximux/` prefix while the sidebar minted the
        // configured one would put two conventions in one rail — and
        // `reclaim_orphan` would stop recognising its own debris.
        let root = std::path::Path::new(&project.root_path);
        let git_settings = oximux_settings::git::GitSettings::load_from_dir(&self.data_dir);
        // Existing-branch mode mints no name at all — it adopts one — so the
        // resolver runs only for the two modes that create a branch.
        let create_base = match base {
            CreateBaseWire::Existing(name) => CreateBase::existing(name.clone()),
            CreateBaseWire::Default | CreateBaseWire::From(_) => {
                let branch = match Repository::open(root).await {
                    Ok(repo) => branch_name::resolve_branch_name(&git_settings, &repo, slug).await,
                    // The create below opens the same repository and reports the
                    // real failure; naming the branch the shipped way here keeps
                    // that the error the caller sees.
                    Err(_) => {
                        branch_name::branch_name(Some(oximux_settings::git::DEFAULT_PREFIX), slug)
                    }
                };
                match base {
                    CreateBaseWire::From(r) => CreateBase::new_branch_from(branch, r.clone()),
                    _ => CreateBase::new_branch(branch),
                }
            }
        };
        let outcome = create_workspace_with_rollback(
            &project,
            slug,
            slug,
            &create_base,
            &target,
            // The locator that minted `target`, so an interrupted create's
            // debris there is reclaimed on retry — the slug collision check
            // above has already established no row claims it.
            &self.locator,
            None,
            &self.workspaces,
            // Headless: no override (the project's `auto_setup` decides) and
            // nowhere to stream a transcript. A remote client asking for a
            // worktree gets the same provisioning the desktop does; it just
            // sees the outcome instead of watching it.
            &Provision::default()
                .freshening_default(git_settings.keep_default_up_to_date),
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
            CreateOutcome::SetupFailed { transcript, rollback_error } => {
                tracing::warn!(
                    slug,
                    outcome = %transcript.outcome.summary(),
                    ?rollback_error,
                    "remote worktree create: setup script failed, worktree rolled back"
                );
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
        // Only a branch this worktree's create minted — the same guard the
        // desktop delete applies, for the same reason: an adopted branch is
        // the user's, and removing a worktree is not permission to delete it.
        if row.branch_minted {
            if let Err(err) = repo.delete_branch(&row.branch, false).await {
                tracing::warn!(?err, branch = %row.branch, "remote worktree remove: branch survives");
            }
        } else {
            tracing::info!(
                branch = %row.branch,
                "keeping an adopted branch: this worktree checked it out, it did not create it"
            );
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
        let a = workspaces.insert(&project.id, "a", "a", "oximux/a", "/p/a", true).expect("a");
        let b = workspaces.insert(&project.id, "b", "b", "oximux/b", "/p/b", true).expect("b");
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

#[cfg(test)]
mod locator_tests {
    use super::*;
    use oximux_storage::open_memory;

    /// The remote surface's guarantee, pinned: whatever the desktop configures
    /// for its own creates, a worktree created through this service lands
    /// under the host's data directory. There is no constructor that takes a
    /// locator, and this is the test that notices if one appears.
    #[test]
    fn the_service_locator_is_host_derived_and_ignores_any_configured_root() {
        let db = open_memory().expect("db");
        let projects = ProjectRepo::new(db.clone());
        let workspaces = WorkspaceRepo::new(db);
        let project = projects.insert("api", "/repos/api", "main").expect("project");
        let data_dir = tempfile::tempdir().expect("tempdir");
        // A configured root the desktop would honour; the service must not.
        std::fs::write(
            data_dir.path().join(oximux_settings::git::GitSettings::FILE_NAME),
            "worktree_dir = \"/somewhere/the/user/chose\"\n",
        )
        .expect("write git.toml");
        let service = RepoWorktrees::new(projects, workspaces, data_dir.path().to_path_buf());
        assert_eq!(
            service.target_path(&project, "feat"),
            crate::worktree_path(data_dir.path(), &project.id, "feat")
        );
    }
}

#[cfg(test)]
mod slug_tests {
    use super::*;

    #[test]
    fn an_explicit_slug_is_used_verbatim_in_every_mode() {
        for base in [
            CreateBaseWire::Default,
            CreateBaseWire::From("main".into()),
            CreateBaseWire::Existing("feature/api/retry".into()),
        ] {
            assert_eq!(effective_slug("retry", &base), "retry", "base {base:?}");
        }
    }

    #[test]
    fn an_omitted_slug_is_derived_from_the_adopted_branch() {
        let base = CreateBaseWire::Existing("feature/api/retry".into());
        // `derive_slug` flattens the slashes the branch is allowed to carry —
        // a slug is one path component, and `validate_slug` would refuse the
        // branch name as written.
        assert_eq!(effective_slug("", &base), "feature-api-retry");
    }

    #[test]
    fn an_omitted_slug_is_not_derived_in_a_mode_that_mints_a_branch() {
        // Nothing to derive from: these modes name the branch after the slug,
        // so an empty one stays empty and `validate_slug` refuses it upstream.
        // Deriving `main` from `--from main` would silently name the worktree
        // after its base.
        assert_eq!(effective_slug("", &CreateBaseWire::Default), "");
        assert_eq!(effective_slug("", &CreateBaseWire::From("main".into())), "");
    }
}
