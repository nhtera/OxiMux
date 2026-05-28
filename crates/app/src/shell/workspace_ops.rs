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

use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, WeakEntity, Window};
use oximux_core::{AgentAdapter, Project, Workspace};
use oximux_git::{Repository, derive_slug, validate_slug};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::{ProjectRepo, StorageError, WorkspaceRepo};

use crate::project_panes_factory::{
    build_project_panes, compute_attach_hints, load_persisted_tabs, save_persisted_tabs,
};
use crate::shell::add_project_dialog::{AddProjectDialog, OnPick as OnAddProjectPick};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt};
use crate::shell::left_rail::LatestStatusMap;
use crate::shell::workspace_dialog::{WorkspaceDialogMode, WorkspaceDialogSubmit};
use crate::workspace_root::{APP_DATA_SUBDIR, WorkspaceRoot};

/// Build the Add-Project dialog entity. Wires the `on_pick` callback to
/// route the chosen project through `WorkspaceRoot::set_active_project`.
/// Lives here (not in `workspace_root.rs`) to keep that file under the
/// 800-LOC fail cap.
pub(crate) fn build_add_project_dialog(
    theme: Theme,
    density: Density,
    typography: Typography,
    project_repo: ProjectRepo,
    cx: &mut Context<WorkspaceRoot>,
) -> Entity<AddProjectDialog> {
    let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
    let on_pick: OnAddProjectPick = Box::new(move |project, window, cx| {
        // Use `update` + the outer window directly — `update_in` does a
        // with_window lookup that returns "entity has no current window"
        // when fired from a deeply-nested async callback (e.g. rfd's
        // NSOpenPanel resolution).
        let _ = weak.update(cx, |this, cx| {
            // Re-pull recents from DB so the just-added project shows up
            // in the sidebar list and in the next Cmd+O picker open.
            this.refresh_recent_projects();
            this.set_active_project(project, window, cx);
        });
    });
    cx.new(|cx| AddProjectDialog::new(theme, density, typography, project_repo, on_pick, cx))
}

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

/// Resolve the static adapter slug used by `start_session` for each
/// built-in agent variant. Inline 4-arm match — KISS over adding a
/// method to `oximux-core`.
fn agent_adapter_id(kind: AgentAdapter) -> &'static str {
    match kind {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Aider => "aider",
        AgentAdapter::Custom => "custom",
    }
}

/// Defer `focus_active` until after GPUI commits the new render tree.
/// Calling focus inline during `set_active_project` lands on whichever
/// surface the project-switch event just relinquished focus from (the
/// left-rail row, the dialog button, etc.), not the freshly-mounted
/// pane. The same two-step race is documented in upstream desktop UIs
/// that use a double-`requestAnimationFrame` pattern for the same fix.
fn defer_focus_active(
    window: &mut Window,
    cx: &mut Context<crate::workspace_root::WorkspaceRoot>,
    panes: Entity<crate::shell::project_panes::ProjectPanes>,
) {
    window.defer(cx, move |window, app| {
        panes.update(app, |p, cx| p.focus_active(window, cx));
    });
}

/// Focus the active project's active pane group's active tab. Called
/// AFTER the async right-sidebar rebuild completes — without this,
/// the rebuild's `cx.notify` repaint can land focus somewhere other
/// than the user's last-active terminal/editor, leaving the chrome
/// action listeners (`ToggleRightSidebar`, etc.) without a focused
/// element to dispatch through. The post-rebuild defer is the second
/// of the two-step focus restoration, mirroring the per-frame focus
/// pin used in upstream IDE shells.
pub(crate) fn refocus_active_pane(
    this: &crate::workspace_root::WorkspaceRoot,
    window: &mut Window,
    cx: &mut Context<crate::workspace_root::WorkspaceRoot>,
) {
    let Some(panes) = this.active_project_panes() else {
        return;
    };
    window.defer(cx, move |window, app| {
        panes.update(app, |p, cx| p.focus_active(window, cx));
    });
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

impl Focusable for WorkspaceRoot {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl WorkspaceRoot {
    /// Boot-time helper: if the recents snapshot is non-empty, activate
    /// the most-recently-opened project so the sidebar isn't a blank
    /// "Open a project" state after relaunch. No-op when there are no
    /// recents. Public so the bin's `main.rs` can call it after
    /// constructing `WorkspaceRoot`.
    pub fn bootstrap_active_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(boot) = self.app_state.recent_projects.first().cloned() {
            self.set_active_project(boot, window, cx);
        }
    }

    /// Re-pull `app_state.recent_projects` from the DB. Called after a new
    /// project is inserted (add-project dialog) or an existing one is
    /// touched (picker) so the in-memory snapshot stays in sync with the
    /// persisted order.
    pub(crate) fn refresh_recent_projects(&mut self) {
        match self.app_state.project_repo.list_recent(20) {
            Ok(list) => self.app_state.recent_projects = list,
            Err(err) => tracing::warn!(?err, "refresh_recent_projects: list_recent failed"),
        }
    }

    /// Set the currently active project. Stores it on `self`, triggers
    /// a re-render so the left rail picks up the new workspaces, and
    /// asynchronously rebuilds the right sidebar (Explorer / Source
    /// Control / Search) against the new project's root. Repository
    /// open is async so the rebuild is spawned; on success, the old
    /// `right_sidebar` entity is replaced and drops.
    pub(crate) fn set_active_project(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(project_id = %project.id, name = %project.name, "active project set");
        // Capture the outgoing project's pane scrollback before swapping so
        // a project-switch-then-quit-other-window flow doesn't lose data.
        // No-op when no project was previously active.
        // Clone window_id BEFORE any mutable borrows of self so closure
        // captures and borrow checker are both satisfied.
        let window_id = self.window_id.clone();
        if let Some(outgoing) = self.active_project.as_ref().map(|p| p.id.clone())
            && outgoing != project.id
            && let Some(panes) = self.project_panes_by_project.get(&outgoing).cloned()
        {
            let repo = self.app_state.pane_buffer_repo.clone();
            panes.read(cx).capture_pane_buffers(
                &repo,
                &outgoing,
                &window_id,
                crate::project_panes_factory::PANE_BUFFER_MAX_BYTES,
                cx,
            );
            let snap = crate::shell::terminal_view::relay_state_snapshot();
            if let Some(session_id) = snap.session_id {
                let relay_repo = self.app_state.pane_relay_id_repo.clone();
                panes
                    .read(cx)
                    .capture_pane_relay_ids(&relay_repo, &outgoing, &window_id, &session_id, cx);
            }
        }
        self.active_project = Some(project.clone());
        let project_root = PathBuf::from(&project.root_path);
        // Lazy-build the project's panes entity on first activation. Subsequent
        // switches just resolve the existing entity via `active_project_panes()`
        // — pane-group + tab state survives the switch.
        if !self.project_panes_by_project.contains_key(&project.id) {
            let theme = self.theme;
            let density = self.density;
            let typography = self.typography.clone();
            let cli_runtime = self.cli_runtime.clone();
            let notifier = self.notifier.clone();
            let snapshot = load_persisted_tabs(
                &self.app_state.settings_repo,
                &project.id,
                &window_id,
            );
            let pane_buffers = crate::project_panes_factory::load_pane_buffers(
                &self.app_state.pane_buffer_repo,
                &project.id,
                &window_id,
            );
            let pane_relay_ids = self
                .app_state
                .pane_relay_id_repo
                .get_all_for_project(&project.id, &window_id)
                .unwrap_or_else(|err| {
                    tracing::warn!(?err, project_id = %project.id, "load pane_relay_ids failed");
                    Vec::new()
                });
            let relay_snap = crate::shell::terminal_view::relay_state_snapshot();
            let attach_hints = compute_attach_hints(
                pane_relay_ids,
                &relay_snap.live_external_ids,
                relay_snap.session_id.as_deref(),
            );
            let panes = build_project_panes(
                project_root.clone(),
                snapshot,
                pane_buffers,
                attach_hints,
                theme,
                density,
                typography,
                cli_runtime,
                notifier,
                window,
                cx,
            );
            // Install the save sink keyed to this project and window.
            let settings_repo = self.app_state.settings_repo.clone();
            let project_id = project.id.clone();
            let window_id_for_cb = window_id.clone();
            let save_cb: crate::shell::project_panes::SaveCallback =
                std::sync::Arc::new(move |snap| {
                    save_persisted_tabs(&settings_repo, &project_id, &window_id_for_cb, &snap);
                });
            panes.update(cx, |p, _| p.set_save_callback(save_cb));
            self._project_panes_observer = Some(cx.observe(&panes, |_, _, cx| cx.notify()));
            self.project_panes_by_project
                .insert(project.id.clone(), panes.clone());
            defer_focus_active(window, cx, panes);
        } else if let Some(panes) = self.project_panes_by_project.get(&project.id).cloned() {
            // Project already opened once — re-point observer.
            self._project_panes_observer = Some(cx.observe(&panes, |_, _, cx| cx.notify()));
            defer_focus_active(window, cx, panes);
        }
        cx.notify();
        cx.spawn_in(window, async move |weak, cx| {
            // Repo presence is optional now — Repository::open may fail for
            // non-git folders. Build the sidebar in either mode: with git
            // (Source Control + Explorer + Search) or without (Explorer +
            // Search only). The Explorer + Search tabs always work from
            // `root_path` regardless of git status.
            let opened = oximux_git::Repository::open(&project_root).await;
            let repo = match opened {
                Ok(r) => Some(r),
                Err(err) => {
                    tracing::info!(
                        ?err,
                        path = %project_root.display(),
                        "non-git project; building file-explorer-only sidebar"
                    );
                    None
                }
            };
            let _ = weak.update_in(cx, |this, window, cx| {
                let theme = this.theme;
                let density = this.density;
                let typography = this.typography.clone();
                // Carry the previous sidebar's open/collapsed state across
                // the rebuild — the right column must stay where the user
                // left it, not snap back open on every project switch.
                let prior_open = this
                    .right_sidebar
                    .as_ref()
                    .map(|s| s.read(cx).open)
                    .unwrap_or(true);
                let weak = cx.weak_entity();
                let on_open =
                    crate::workspace_root::WorkspaceRoot::build_on_open_file_callback(weak.clone());
                let on_open_diff = repo.as_ref().map(|r| {
                    crate::workspace_root::WorkspaceRoot::build_on_open_diff_callback(
                        weak.clone(),
                        r.clone(),
                    )
                });
                let on_query =
                    crate::workspace_root::WorkspaceRoot::build_on_query_active_path_callback(weak);
                this.right_sidebar = Some(cx.new(|cx| {
                    crate::shell::right_sidebar::RightSidebar::new(
                        repo,
                        project_root.clone(),
                        prior_open,
                        Some(on_open),
                        on_open_diff,
                        Some(on_query),
                        theme,
                        density,
                        typography,
                        window,
                        cx,
                    )
                }));
                // Re-focus the active pane after the right_sidebar
                // rebuild — the rebuild's `cx.notify` triggers a
                // repaint that can land focus on a freshly-mounted
                // sub-element of the sidebar (FileExplorer, etc.)
                // instead of the user's last-active terminal/editor.
                // Mirrors the "open project → cursor in last
                // working terminal" behavior; also keeps the chrome
                // toggle buttons routable since their actions need a
                // focused element inside the workspace_root subtree.
                refocus_active_pane(this, window, cx);
                cx.notify();
            });
        })
        .detach();
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
        self.add_project_dialog.update(cx, |d, cx| d.close(cx));
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

    /// Snapshot the sidebar data (all recent projects, their workspaces,
    /// and the latest agent-session status per workspace) and push it into
    /// `LeftRail`. Called at the top of `WorkspaceRoot::render` — LeftRail
    /// never reads `WorkspaceRoot` directly because doing so re-enters
    /// the entity slot during rendering and panics.
    pub(crate) fn refresh_left_rail(&mut self, cx: &mut Context<Self>) {
        let projects = self.app_state.recent_projects.clone();
        let active_project_id = self.active_project.as_ref().map(|p| p.id.clone());
        let mut workspaces_by_project: HashMap<String, Vec<Workspace>> =
            HashMap::with_capacity(projects.len());
        let mut latest_status: LatestStatusMap = HashMap::new();
        for project in &projects {
            let list = match self.app_state.workspace_repo.list_for_project(&project.id) {
                Ok(list) => list,
                Err(err) => {
                    tracing::warn!(?err, project_id = %project.id, "list_for_project failed");
                    Vec::new()
                }
            };
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
                latest_status.insert(workspace.id.clone(), latest);
            }
            workspaces_by_project.insert(project.id.clone(), list);
        }
        self.left_rail.update(cx, |rail, cx| {
            rail.set_sidebar_data(
                projects,
                active_project_id,
                workspaces_by_project,
                latest_status,
                cx,
            );
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
                let Some(project) = submit.project.or_else(|| self.active_project.clone()) else {
                    tracing::info!("workspace create: no project selected, ignoring");
                    return;
                };
                // Keep sidebar in sync if the user picked a different
                // project from the dialog dropdown than the currently
                // active one.
                let same_active = self
                    .active_project
                    .as_ref()
                    .map(|p| p.id == project.id)
                    .unwrap_or(false);
                if !same_active {
                    self.set_active_project(project.clone(), window, cx);
                }
                self.create_workspace_async(project, submit.name, submit.agent, window, cx);
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
        agent: Option<AgentAdapter>,
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
                    let cwd = PathBuf::from(&workspace.worktree_path);
                    let _ = weak.update_in(cx, |this, window, cx| {
                        cx.notify();
                        if let Some(kind) = agent {
                            this.spawn_agent_tab(kind, agent_adapter_id(kind), cwd, window, cx);
                        }
                    });
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
