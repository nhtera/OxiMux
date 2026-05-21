//! Pure async orchestration for workspace create + delete flows + the
//! `WorkspaceRoot` extension `impl` that drives them from the dialog.
//!
//! The pure `create_workspace_with_rollback` helper lives outside
//! `WorkspaceRoot` so the rollback path can be exercised by an
//! integration test (`crates/app/tests/workspace_create_rollback.rs`)
//! without a GPUI context. The extension methods on `WorkspaceRoot`
//! land here too (rather than in `workspace_root.rs`) to keep the
//! root file under the 800-LOC fail cap; they are the only consumers
//! of the helper.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{AppContext, Context, WeakEntity, Window};
use oximux_core::{Project, Workspace};
use oximux_git::{Repository, derive_slug, validate_slug};
use oximux_storage::{StorageError, WorkspaceRepo};

use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt};
use crate::shell::left_rail::LatestStatusMap;
use crate::shell::workspace_dialog::{WorkspaceDialogMode, WorkspaceDialogSubmit};
use crate::workspace_root::{APP_DATA_SUBDIR, WorkspaceRoot};

/// Outcome of a create flow. Distinguishes user-visible failures (which
/// require explicit handling at the call site) from the silent success
/// path. The `RollbackFailed` variant is reached only when the rollback
/// path itself errors — caller should escalate visibility (e.g. surface
/// a "manual cleanup required" hint).
#[derive(Debug)]
pub enum CreateOutcome {
    /// Workspace row inserted; worktree + branch live on disk.
    Created(Workspace),
    /// The git step failed before any rollback was needed. The repo is
    /// in a clean state.
    GitFailed(String),
    /// Storage insert failed and the rollback (`remove_worktree` +
    /// `delete_branch`) ran cleanly — repo + DB consistent again, but
    /// the user's request failed.
    StorageFailedRollbackClean(StorageError),
    /// Storage insert failed AND the rollback itself failed. The repo
    /// has an orphan worktree or branch; surface the original error and
    /// the rollback error.
    StorageFailedRollbackDirty {
        insert_error: StorageError,
        rollback_error: String,
    },
}

/// Open the project repo, create a new worktree on branch `oximux/<slug>`,
/// and insert the workspace row. On storage failure, runs the rollback
/// (force-remove worktree + force-delete branch) so that the next
/// `list_recent_workspaces` reflects the on-disk truth.
///
/// `name` is the human label (caller has already trimmed); `slug` MUST
/// pre-validate via `validate_slug` upstream — this function assumes the
/// slug is safe to pass to `git worktree add -b oximux/<slug>`.
pub async fn create_workspace_with_rollback(
    project_root: &Path,
    project_id: &str,
    name: &str,
    slug: &str,
    worktree_path: &Path,
    workspace_repo: &WorkspaceRepo,
) -> CreateOutcome {
    let branch = format!("oximux/{slug}");
    let repo = match Repository::open(project_root).await {
        Ok(r) => r,
        Err(err) => return CreateOutcome::GitFailed(format!("open project repo: {err}")),
    };
    if let Err(err) = repo.add_worktree(worktree_path, slug).await {
        return CreateOutcome::GitFailed(format!("add_worktree: {err}"));
    }
    let path_str = worktree_path.to_string_lossy().to_string();
    match workspace_repo.insert(project_id, name, slug, &branch, &path_str) {
        Ok(workspace) => CreateOutcome::Created(workspace),
        Err(insert_error) => {
            // Rollback: best-effort. If either step errors, we surface
            // a dirty-state outcome so the call site can log loudly.
            let mut rollback_err = None;
            if let Err(err) = repo.remove_worktree(worktree_path, true).await {
                rollback_err = Some(format!("remove_worktree: {err}"));
            }
            if let Err(err) = repo.delete_branch(&branch, true).await {
                let chained = match rollback_err {
                    Some(prev) => format!("{prev}; delete_branch: {err}"),
                    None => format!("delete_branch: {err}"),
                };
                rollback_err = Some(chained);
            }
            match rollback_err {
                Some(err) => CreateOutcome::StorageFailedRollbackDirty {
                    insert_error,
                    rollback_error: err,
                },
                None => CreateOutcome::StorageFailedRollbackClean(insert_error),
            }
        }
    }
}

/// Compose the worktree dir path:
/// `<app_data>/dev.nhtera.oximux/projects/<project_id>/worktrees/<slug>`.
/// Returns `None` when `dirs::data_dir()` is unavailable (sandbox or
/// unset `$HOME`) — caller surfaces this as a create failure.
fn worktree_path(project_id: &str, slug: &str) -> Option<PathBuf> {
    Some(
        dirs::data_dir()?
            .join(APP_DATA_SUBDIR)
            .join("projects")
            .join(project_id)
            .join("worktrees")
            .join(slug),
    )
}

impl WorkspaceRoot {
    /// Set the currently active project (called by the project picker's
    /// `on_pick` callback). Step 7's sidebar reads this for its
    /// workspace list. `cx.notify` triggers a re-render so the status
    /// bar / future sidebar reflect the new selection.
    pub(crate) fn set_active_project(&mut self, project: Project, cx: &mut Context<Self>) {
        tracing::info!(project_id = %project.id, name = %project.name, "active project set");
        self.active_project = Some(project);
        cx.notify();
    }

    /// Close every full-window modal overlay. Callers invoke this before
    /// opening a new overlay so two inset-0 dismiss regions never compete.
    pub(crate) fn close_modal_overlays(&mut self, cx: &mut Context<Self>) {
        self.palette.update(cx, |p, cx| p.close(cx));
        self.pane_actions.update(cx, |p, cx| p.close(cx));
        self.adapter_picker.update(cx, |p, cx| p.close(cx));
        self.project_picker.update(cx, |p, cx| p.close(cx));
        self.workspace_dialog.update(cx, |d, cx| d.close(cx));
        self.row_menu.update(cx, |m, cx| m.close(cx));
    }

    /// Open the per-row action popover at the given screen coordinates.
    /// Closes any other overlays first so backdrops don't compete.
    pub(crate) fn open_row_menu(
        &mut self,
        workspace: oximux_core::Workspace,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.close_modal_overlays(cx);
        self.row_menu
            .update(cx, |m, cx| m.open(workspace, x, y, cx));
    }

    /// Snapshot the sidebar data (active project, its workspaces, and
    /// the latest agent-session status per workspace) and push it into
    /// `LeftRail`. Called at the top of `WorkspaceRoot::render` — LeftRail
    /// never reads `WorkspaceRoot` directly because doing so re-enters
    /// the entity slot during rendering and panics.
    pub(crate) fn refresh_left_rail(&mut self, cx: &mut Context<Self>) {
        let active_project = self.active_project.clone();
        let (workspaces, latest_status) = match &active_project {
            Some(project) => match self.app_state.workspace_repo.list_for_project(&project.id) {
                Ok(list) => {
                    let mut status_map: LatestStatusMap = HashMap::with_capacity(list.len());
                    for workspace in &list {
                        let latest = match self
                            .app_state
                            .agent_session_repo
                            .list_for_workspace(&workspace.id)
                        {
                            Ok(mut sessions) => sessions.drain(..).next().map(|s| s.status),
                            Err(err) => {
                                tracing::warn!(?err, workspace_id = %workspace.id, "list_for_workspace failed");
                                None
                            }
                        };
                        status_map.insert(workspace.id.clone(), latest);
                    }
                    (list, status_map)
                }
                Err(err) => {
                    tracing::warn!(?err, project_id = %project.id, "list_for_project failed");
                    (Vec::new(), HashMap::new())
                }
            },
            None => (Vec::new(), HashMap::new()),
        };
        self.left_rail.update(cx, |rail, cx| {
            rail.set_sidebar_data(active_project, workspaces, latest_status, cx);
        });
    }

    /// Route a workspace-dialog submission to the right backend flow.
    /// Mode dispatch lives here (not in the dialog) so the dialog stays
    /// UI-only and the create-with-rollback orchestration stays close
    /// to `app_state` + the active project.
    pub(crate) fn dispatch_workspace_submit(
        &mut self,
        submit: WorkspaceDialogSubmit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match submit.mode {
            WorkspaceDialogMode::Create => {
                let Some(project) = self.active_project.clone() else {
                    tracing::info!("workspace create: no active project, ignoring");
                    return;
                };
                self.create_workspace_async(project, submit.name, window, cx);
            }
            WorkspaceDialogMode::Rename(workspace) => {
                self.rename_workspace_now(*workspace, submit.name, cx);
            }
        }
    }

    /// Create-workspace flow: derive slug → orchestrate via
    /// [`create_workspace_with_rollback`]. The helper is pure-async so
    /// the rollback path can be unit-tested without a GPUI context.
    pub(crate) fn create_workspace_async(
        &mut self,
        project: Project,
        name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slug = derive_slug(name.trim());
        // Fail fast on invalid slugs (e.g. `"workspace.lock"`) so the
        // user sees a tracing log before any IO; otherwise add_worktree
        // would error inside the spawn task with the dialog already
        // closed (H4 — code-review 260521-1306).
        if let Err(err) = validate_slug(&slug) {
            tracing::warn!(
                ?err,
                slug = %slug,
                name = %name,
                "workspace create: derived slug failed validate_slug"
            );
            return;
        }
        let Some(worktree_path) = worktree_path(&project.id, &slug) else {
            tracing::warn!("workspace create: cannot resolve data dir");
            return;
        };
        let workspace_repo = self.app_state.workspace_repo.clone();
        let project_root = PathBuf::from(&project.root_path);
        let project_id = project.id.clone();
        let name_trimmed = name.trim().to_string();

        cx.spawn(async move |weak, cx| {
            if let Some(parent) = worktree_path.parent()
                && let Err(err) = std::fs::create_dir_all(parent)
            {
                tracing::warn!(
                    ?err,
                    path = %parent.display(),
                    "create_dir_all worktree parent failed"
                );
                return;
            }
            let outcome = create_workspace_with_rollback(
                &project_root,
                &project_id,
                &name_trimmed,
                &slug,
                &worktree_path,
                &workspace_repo,
            )
            .await;
            match outcome {
                CreateOutcome::Created(workspace) => {
                    tracing::info!(
                        workspace_id = %workspace.id,
                        slug = %slug,
                        "workspace created"
                    );
                    // Notify WorkspaceRoot so step 7's sidebar picks up
                    // the new row without waiting for an unrelated
                    // re-render (M3 — code-review 260521-1306).
                    let _ = weak.update(cx, |_this, cx| cx.notify());
                }
                CreateOutcome::GitFailed(msg) => {
                    tracing::warn!(slug = %slug, error = %msg, "workspace create: git step failed");
                }
                CreateOutcome::StorageFailedRollbackClean(err) => {
                    tracing::warn!(
                        ?err,
                        slug = %slug,
                        "workspace create: insert failed, rollback clean"
                    );
                }
                CreateOutcome::StorageFailedRollbackDirty {
                    insert_error,
                    rollback_error,
                } => {
                    tracing::warn!(
                        ?insert_error,
                        rollback = %rollback_error,
                        slug = %slug,
                        "workspace create: insert failed AND rollback failed; manual cleanup required"
                    );
                }
            }
        })
        .detach();
    }

    /// Rename a workspace (DB only — no git or filesystem changes).
    fn rename_workspace_now(
        &mut self,
        workspace: Workspace,
        new_name: String,
        cx: &mut Context<Self>,
    ) {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return;
        }
        if let Err(err) = self
            .app_state
            .workspace_repo
            .rename(&workspace.id, new_name)
        {
            tracing::warn!(?err, workspace_id = %workspace.id, "rename failed");
        }
        cx.notify();
    }

    /// Open the rename dialog pre-filled with the workspace's current
    /// name. Step 7's sidebar context menu will route here.
    #[allow(dead_code)]
    pub(crate) fn request_rename_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_dialog
            .update(cx, |d, cx| d.open_rename(workspace, window, cx));
    }

    /// Archive a workspace — `archived_at` + status='archived'. The
    /// sidebar will hide archived workspaces by default.
    #[allow(dead_code)]
    pub(crate) fn archive_workspace(&mut self, workspace: Workspace, cx: &mut Context<Self>) {
        if let Err(err) = self.app_state.workspace_repo.mark_archived(&workspace.id) {
            tracing::warn!(?err, workspace_id = %workspace.id, "mark_archived failed");
        }
        cx.notify();
    }

    /// Open the type-to-confirm dialog for workspace deletion. Reuses
    /// `ConfirmDialog`; expected string is the slug. On confirm: removes
    /// worktree + branch + DB row (FK cascade clears pane/agent
    /// sessions).
    #[allow(dead_code)]
    pub(crate) fn request_delete_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.active_project.clone() else {
            tracing::info!("request_delete_workspace: no active project, ignoring");
            return;
        };
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let slug = workspace.slug.clone();
        let workspace_for_cb = workspace.clone();
        let project_root = PathBuf::from(&project.root_path);
        let workspace_repo = self.app_state.workspace_repo.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |_window, cx| {
            let project_root = project_root.clone();
            let workspace_repo = workspace_repo.clone();
            let workspace = workspace_for_cb.clone();
            let branch = workspace.branch.clone();
            let worktree_path = PathBuf::from(&workspace.worktree_path);
            let weak = weak.clone();
            // Clear the confirm dialog up-front so the user can never get
            // stuck behind it on an early-return failure path (C1 — code-
            // review 260521-1306). The destructive intent has already
            // fired; subsequent errors surface via tracing for now (a
            // future status-bar surface is the right home for user-visible
            // failure reporting).
            let _ = weak.update(cx, |this, cx| {
                this.confirm_dialog = None;
                cx.notify();
            });
            cx.spawn(async move |cx| {
                let repo = match Repository::open(&project_root).await {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::warn!(?err, "delete_workspace: open repo failed");
                        return;
                    }
                };
                if let Err(err) = repo.remove_worktree(&worktree_path, false).await {
                    tracing::warn!(?err, slug = %workspace.slug, "remove_worktree failed; workspace row + branch preserved for retry");
                    return;
                }
                if let Err(err) = repo.delete_branch(&branch, false).await {
                    tracing::warn!(?err, branch = %branch, "delete_branch failed");
                    // Don't bail — DB cleanup still wanted to keep
                    // state in sync.
                }
                if let Err(err) = workspace_repo.delete(&workspace.id) {
                    tracing::warn!(?err, workspace_id = %workspace.id, "delete row failed");
                }
                let _ = weak.update(cx, |_this, cx| cx.notify());
            })
            .detach();
        });
        let prompt = ConfirmPrompt {
            title: "Delete workspace".into(),
            body: format!(
                "Removes the worktree at {} and deletes branch {}. This cannot be undone.",
                workspace.worktree_path, workspace.branch
            )
            .into(),
            expected: slug.into(),
            on_confirm,
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        self.confirm_dialog =
            Some(cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx)));
        cx.notify();
    }
}
