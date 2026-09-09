//! Pure async orchestration for workspace create + delete flows + the
//! `WorkspaceRoot` extension `impl` that drives them from the dialog.
//!
//! The pure `create_workspace_with_rollback` helper lives outside
//! `WorkspaceRoot` so the rollback path can be exercised by an
//! integration test (`apps/desktop/tests/workspace_create_rollback.rs`)
//! without a GPUI context. The extension methods on `WorkspaceRoot`
//! land here too (rather than in `workspace_root.rs`) to keep the
//! root file under the 800-LOC fail cap; they are the only consumers
//! of the helper.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, WeakEntity, Window};
use oximux_core::{AgentAdapter, AgentSession, Project, Workspace};

use crate::shell::agent_presentation::AmbientAgent;
use oximux_git::{Repository, derive_slug, validate_slug};
use oximux_settings::{Density, ScriptKind, Theme, Typography};
use oximux_storage::{AgentSessionRepo, ProjectRepo, WorkspaceRepo};

use crate::shell::left_rail::row_menu::ScriptAvail;

use crate::project_panes_factory::{
    build_project_panes, load_persisted_tabs,
    save_persisted_tabs,
};
use crate::shell::add_project_dialog::{AddProjectDialog, OnPick as OnAddProjectPick};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt};
use crate::shell::left_rail::{LatestStatusMap, RailAgentTarget, WorkspaceAgentList};
use crate::shell::pane_group::FocusedRailAgent;
use crate::shell::workspace_dialog::{WorkspaceDialogMode, WorkspaceDialogSubmit};
use crate::workspace_root::WorkspaceRoot;

/// A back/forward history entry: a workspace identified by its owning project
/// id plus its workspace id. Refs (not full `Workspace` clones) so navigation
/// re-resolves against the live store and a deleted workspace fails gracefully.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceNavRef {
    pub project_id: String,
    pub workspace_id: String,
}

/// Upper bound on the back/forward history depth per window. Oldest entries
/// drop once exceeded.
const MAX_NAV_HISTORY: usize = 64;

/// Pure core of [`WorkspaceRoot::record_nav`]: applies browser-style
/// truncate-forward, append, then cap on `history` at `cursor`, returning the
/// new cursor. Dedupes against the current cursor entry (returns `cursor`
/// unchanged). Pure so the history semantics are unit-testable without GPUI.
fn push_nav_entry(
    history: &mut Vec<WorkspaceNavRef>,
    cursor: usize,
    entry: WorkspaceNavRef,
    max: usize,
) -> usize {
    if history.get(cursor) == Some(&entry) {
        return cursor;
    }
    // Drop any forward history past the current cursor before appending.
    if !history.is_empty() {
        history.truncate(cursor + 1);
    }
    history.push(entry);
    if history.len() > max {
        let excess = history.len() - max;
        history.drain(0..excess);
    }
    history.len() - 1
}

fn workspace_path_for_ambient_terminal(
    terminal_path: &str,
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
) -> Option<String> {
    let terminal_path = Path::new(terminal_path);
    workspaces_by_project
        .values()
        .flat_map(|workspaces| workspaces.iter())
        .filter(|workspace| terminal_path.starts_with(Path::new(&workspace.worktree_path)))
        .max_by_key(|workspace| workspace.worktree_path.len())
        .map(|workspace| workspace.worktree_path.clone())
}

/// Walk indices from `cursor` in the chosen direction, returning the first one
/// for which `live(idx)` holds (skipping stale entries), or `None` at the
/// boundary. Pure so the skip-stale traversal is unit-testable without GPUI.
fn next_live_index(
    len: usize,
    cursor: usize,
    forward: bool,
    live: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut idx = cursor;
    loop {
        if forward {
            if idx + 1 >= len {
                return None;
            }
            idx += 1;
        } else {
            if idx == 0 {
                return None;
            }
            idx -= 1;
        }
        if live(idx) {
            return Some(idx);
        }
    }
}

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

// The worktree lifecycle itself lives in `oximux-worktree-ops`, so
// `oximux serve` creates the same worktree this flow does rather than a
// second implementation of the same rollback ladder. Re-exported here
// because this module is the desktop's door to it.
pub use oximux_worktree_ops::{
    CreateOutcome, Provision, ProvisionEvent, SetupTranscript, create_workspace_with_rollback,
    run_cleanup_before_remove,
};

/// Upper bound on `default_tabs`. The list is repo-controlled and needs no
/// opt-in, so it is bounded at roughly the number of tabs a person would open
/// by hand rather than at what a config file may declare.
const MAX_DEFAULT_TABS: usize = 8;

/// Upper bound on one `default_tabs` title, in characters.
const MAX_TAB_TITLE_CHARS: usize = 64;

/// Provisioning transcripts retained per workspace slug.
const KEEP_TRANSCRIPTS: usize = 3;

/// Where one worktree's provisioning transcript lives:
/// `<data_dir>/projects/<project_id>/provisioning/<slug>-<millis>.log`.
///
/// Outside the worktree deliberately. The transcript matters most when setup
/// failed, and that is exactly when the worktree has been rolled back — a file
/// written inside it would be deleted by the rollback that made it interesting.
///
/// A fresh path per attempt, not one file per slug. Overwriting in place looks
/// tidier and is wrong: the editor activates an already-open tab for a path
/// without re-reading it, so a second failed create would show the user the
/// *first* failure's output while the file on disk said otherwise. Older
/// transcripts for the same slug are pruned so this stays a log and not an
/// archive.
pub(crate) fn provisioning_transcript_path(project_id: &str, slug: &str) -> PathBuf {
    let dir = crate::app_paths::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("projects")
        .join(project_id)
        .join("provisioning");
    prune_transcripts(&dir, slug, KEEP_TRANSCRIPTS);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("{slug}-{stamp}.log"))
}

/// Keep the `keep` newest transcripts for `slug`, delete the rest.
///
/// By name, not by mtime: the name carries the timestamp, and lexicographic
/// order over a fixed-width millisecond stamp is chronological order. Every
/// failure here is ignored — pruning a log must never be able to affect a
/// worktree create.
fn prune_transcripts(dir: &Path, slug: &str, keep: usize) {
    let prefix = format!("{slug}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // first run for this project: nothing to prune
    };
    let mut mine: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "log")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    if mine.len() < keep {
        return;
    }
    mine.sort();
    // `keep - 1`: one slot is about to be taken by the transcript this call is
    // making a path for.
    for stale in mine.iter().take(mine.len().saturating_sub(keep - 1)) {
        let _ = std::fs::remove_file(stale);
    }
}

/// Drain [`ProvisionEvent`]s into the transcript file until the sender drops.
///
/// Flushed per event rather than at the end so `tail -f` shows a long install
/// progressing, and so a hard kill mid-setup still leaves the lines that
/// explain where it got to. Every IO error here is swallowed: failing to write
/// a log must not be able to fail a worktree creation that otherwise worked.
pub(crate) async fn stream_provisioning(
    path: PathBuf,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ProvisionEvent>,
) {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(?err, path = %path.display(), "provisioning transcript unavailable");
            // Still drain, or the unbounded channel grows for the whole run.
            while rx.recv().await.is_some() {}
            return;
        }
    };
    while let Some(event) = rx.recv().await {
        let line = match event {
            ProvisionEvent::IncludeCopied(p) => format!("include: copied {}", p.display()),
            ProvisionEvent::IncludeSkipped(skip) => format!("include: skipped {skip}"),
            ProvisionEvent::FreshenStarted(branch) => format!("fetching {branch}…"),
            ProvisionEvent::FreshenFinished(summary) => format!("== {summary}"),
            ProvisionEvent::SetupStarted(script) => format!("$ {script}"),
            ProvisionEvent::SetupLine(line) => line,
            ProvisionEvent::SetupFinished(outcome) => format!("== {}", outcome.summary()),
        };
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// Body of [`WorkspaceRoot::workspaces_with_primary`] as a free function so
/// the rail gather can run it on the background executor (SQLite + a
/// `.git` stat — never call from a render path).
fn workspaces_with_primary_for(repo: &WorkspaceRepo, project: &Project) -> Vec<Workspace> {
    let mut list = match repo.list_for_project(&project.id) {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!(?err, project_id = %project.id, "list_for_project failed");
            Vec::new()
        }
    };
    let has_root_row = list.iter().any(|w| w.worktree_path == project.root_path);
    if !has_root_row {
        // Git repo → branch-based primary ("main"); plain folder → a single
        // "Folder" row with empty branch. Synthesized for display only;
        // identified later by `worktree_path == project.root_path`.
        let is_git = Path::new(&project.root_path).join(".git").exists();
        let (name, slug, branch) = if is_git {
            (
                project.default_branch.clone(),
                project.default_branch.clone(),
                project.default_branch.clone(),
            )
        } else {
            (project.name.clone(), String::new(), String::new())
        };
        list.insert(
            0,
            Workspace {
                id: format!("primary:{}", project.id),
                project_id: project.id.clone(),
                name,
                slug,
                branch,
                worktree_path: project.root_path.clone(),
                status: "active".to_string(),
                created_at: String::new(),
                archived_at: None,
                linked_issue: None,
                tint: None,
                // Synthesized primary row; always pinned first, never reordered.
                sort_order: 0.0,
                pinned: false,
                // Permanently blank: this row is the project root, synthesized
                // at render time with no `workspaces` row behind it, so there
                // is nowhere for a progress write to land. `worktree ls` omits
                // it for the same reason.
                comment: String::new(),
                phase: String::new(),
            },
        );
    }
    list
}

/// One full pass of the sidebar's DB-backed data across every recent
/// project: workspace rows (incl. synthesized primaries) + the latest
/// agent-session status and adapter per workspace. Runs on the background
/// executor — this is the ONLY place the rail touches SQLite.
///
/// The adapter map (workspace id → adapter slug of the latest session)
/// gates the activity tail: only the primary CLI journals session logs,
/// so other adapters never get a tail attempt.
/// Outputs of one rail DB gather, in the order the `WorkspaceRoot::rail_*`
/// fields consume them: workspaces-by-project, latest status, latest adapter
/// slug, last-active timestamp, and the FULL per-workspace session list
/// (workspace id → all `agent_sessions` rows, `started_at` DESC) the
/// live↔history merge consumes — each keyed as documented on those fields.
type RailDbData = (
    HashMap<String, Vec<Workspace>>,
    HashMap<String, Vec<Workspace>>,
    LatestStatusMap,
    HashMap<String, String>,
    HashMap<String, String>,
    HashMap<String, Vec<AgentSession>>,
);

/// Find the [`Project`] that owns `workspace`, by its `project_id` — NOT from
/// `WorkspaceRoot::active_project`.
///
/// The rail renders every open project's groups at once, so a row the user
/// clicks may belong to a project that is not active. Any handler that opens a
/// git repository for a row must resolve through here: opening the active
/// project's repo instead runs the row's git commands in the wrong repository
/// (a force delete would run `git branch -D <other project's branch>` in this
/// one). Returns `None` when the owning project is not open — the caller must
/// then decline the action rather than fall back to whichever project happens
/// to be active.
pub(crate) fn resolve_project_for_workspace(
    projects: &[Project],
    workspace: &Workspace,
) -> Option<Project> {
    projects
        .iter()
        .find(|p| p.id == workspace.project_id)
        .cloned()
}




/// Everything a workspace delete needs to name, resolved from the row.
///
/// This exists so the wrong-repository guard has something a test can hold.
/// `request_delete_workspace` is GPUI-bound and cannot be driven from a unit
/// test, so without this the only assertion possible would be on
/// [`resolve_project_for_workspace`] — which a revert of the handler back to
/// `self.active_project` would leave passing. Routing the handler through here
/// means such a revert has to delete this function, and the test stops
/// compiling.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceDeleteTarget {
    /// The repository to open — the ROW's project root, never the active one.
    project_root: PathBuf,
    worktree_path: PathBuf,
    branch: String,
}

/// Resolve what a delete of `workspace` should operate on, or `None` when its
/// owning project is not open (decline rather than fall back).
fn workspace_delete_target(
    projects: &[Project],
    workspace: &Workspace,
) -> Option<WorkspaceDeleteTarget> {
    let project = resolve_project_for_workspace(projects, workspace)?;
    Some(WorkspaceDeleteTarget {
        project_root: PathBuf::from(&project.root_path),
        worktree_path: PathBuf::from(&workspace.worktree_path),
        branch: workspace.branch.clone(),
    })
}

fn gather_rail_db_data(
    workspace_repo: &WorkspaceRepo,
    agent_repo: &AgentSessionRepo,
    projects: &[Project],
) -> RailDbData {
    let mut workspaces_by_project: HashMap<String, Vec<Workspace>> =
        HashMap::with_capacity(projects.len());
    // Archived rows ride along in this same background pass rather than
    // loading lazily on group expansion: the group header shows a count, so
    // they are needed whether or not the group is open, and one extra indexed
    // SELECT per project is noise beside the per-workspace session query below.
    let mut archived_by_project: HashMap<String, Vec<Workspace>> =
        HashMap::with_capacity(projects.len());
    let mut latest_status: LatestStatusMap = HashMap::new();
    let mut latest_adapter: HashMap<String, String> = HashMap::new();
    // Recency key for the dashboard's in-tier sort: a finished session's
    // `ended_at`, else its `started_at` (still running). Raw RFC-3339 string —
    // lexicographic ordering matches chronological for these UTC `Z` stamps.
    let mut last_active: HashMap<String, String> = HashMap::new();
    // Every session per workspace (not just the most-recent) so the rail can
    // list multiple agents; the single-row caches above still derive from the
    // newest (`first()`), preserving today's collapsed-dot behavior.
    let mut workspace_sessions: HashMap<String, Vec<AgentSession>> = HashMap::new();
    for project in projects {
        let list = workspaces_with_primary_for(workspace_repo, project);
        for workspace in &list {
            let sessions = match agent_repo.list_for_workspace(&workspace.id) {
                Ok(sessions) => sessions,
                Err(err) => {
                    tracing::warn!(?err, workspace_id = %workspace.id, "list_for_workspace failed");
                    Vec::new()
                }
            };
            if let Some(session) = sessions.first() {
                latest_adapter.insert(workspace.id.clone(), session.adapter_id.clone());
                if let Some(ts) = session
                    .ended_at
                    .clone()
                    .or_else(|| session.started_at.clone())
                {
                    last_active.insert(workspace.id.clone(), ts);
                }
            }
            latest_status.insert(
                workspace.id.clone(),
                sessions.first().map(|s| s.status.clone()),
            );
            workspace_sessions.insert(workspace.id.clone(), sessions);
        }
        workspaces_by_project.insert(project.id.clone(), list);
        let archived = match workspace_repo.list_archived_for_project(&project.id) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(?err, project_id = %project.id, "list_archived_for_project failed");
                Vec::new()
            }
        };
        archived_by_project.insert(project.id.clone(), archived);
    }
    (
        workspaces_by_project,
        archived_by_project,
        latest_status,
        latest_adapter,
        last_active,
        workspace_sessions,
    )
}

/// Outcome the *New Agent in a fresh worktree* flow hands back to the chat view.
/// The host resolves the worktree create as a first-class `Workspace` (DB row +
/// git worktree via [`create_workspace_with_rollback`]) and maps its richer
/// [`CreateOutcome`] down to this shape, which the chat's
/// `on_worktree_create_outcome` consumes to rebind its cwd / surface an error.
#[derive(Debug, Clone)]
pub enum ChatWorktreeOutcome {
    /// Worktree + branch + `Workspace` row created; the chat binds to `path`.
    Created { path: PathBuf, branch: String },
    /// `slug` failed [`validate_slug`] — surfaced before any IO ran.
    InvalidSlug(String),
    /// The git step OR the storage insert failed (rollback attempted); carries
    /// the underlying message so the chat can offer inline retry or the
    /// "continue without a worktree" fallback.
    GitFailed(String),
}

/// Resolve the static adapter slug used by `start_session` for each
/// built-in agent variant. Inline match — KISS over adding a
/// method to `oximux-core`.
fn agent_adapter_id(kind: AgentAdapter) -> &'static str {
    match kind {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Pi => "pi",
        AgentAdapter::Omp => "omp",
        AgentAdapter::Custom => "custom",
    }
}

/// Defer `focus_active` until after GPUI commits the new render tree.
/// Calling focus inline during `set_active_project` lands on whichever
/// surface the project-switch event just relinquished focus from (the
/// left-rail row, the dialog button, etc.), not the freshly-mounted
/// pane. The same two-step race is documented in upstream desktop UIs
/// that use a double-`requestAnimationFrame` pattern for the same fix.
pub(crate) fn defer_focus_active(
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

/// Compose the worktree dir path under the desktop's own data root.
/// Returns `None` when the app data directory is unavailable (sandbox or
/// unset `$HOME`) — caller surfaces this as a create failure.
///
/// The derivation itself is shared with `oximux serve`, which supplies its
/// own `--data-dir` instead; this wrapper is only the desktop's answer to
/// "which root?".
pub(crate) fn worktree_path(project_id: &str, slug: &str) -> Option<PathBuf> {
    Some(oximux_worktree_ops::worktree_path(
        &crate::app_paths::data_dir()?,
        project_id,
        slug,
    ))
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

    /// Boot-time helper for multi-window restore: activate the SPECIFIC
    /// project this window had open at the last quit (looked up in the
    /// recents snapshot by id). No-op when the project is no longer in
    /// recents (e.g. deleted) — the window opens on the welcome view.
    /// Public so the bin's `main.rs` can call it per restored window.
    pub fn restore_active_project(
        &mut self,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(project) = self
            .app_state
            .recent_projects
            .iter()
            .find(|p| p.id == project_id)
            .cloned()
        {
            self.set_active_project(project, window, cx);
        } else {
            tracing::info!(
                project_id,
                "restore_active_project: project not in recents; opening welcome view"
            );
        }
    }

    /// Re-pull `app_state.recent_projects` from the DB. Called after a new
    /// project is inserted (add-project dialog) or an existing one is
    /// touched (picker) so the in-memory snapshot stays in sync with the
    /// persisted order.
    pub(crate) fn refresh_recent_projects(&mut self) {
        // Manual (sort_order) order, not recency — opening/adding a project
        // appends or leaves it in place rather than floating it to the top.
        match self.app_state.project_repo.list_ordered(20) {
            Ok(list) => self.app_state.recent_projects = list,
            Err(err) => tracing::warn!(?err, "refresh_recent_projects: list_ordered failed"),
        }
    }

    /// Persist a drag-reorder of the project list: write `moved_id`'s new rank
    /// adjacent to the project at `target_index`, then re-pull the ordered list
    /// and re-render the rail. A drop in place is already filtered by the drop
    /// handler; an unknown id is a benign no-op at the repo layer.
    pub(crate) fn reorder_project(
        &mut self,
        moved_id: String,
        target_index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self
            .app_state
            .project_repo
            .reorder_to(&moved_id, target_index)
        {
            tracing::warn!(?err, project_id = %moved_id, "reorder_project: persist failed");
            return;
        }
        self.refresh_recent_projects();
        // The rail snapshots `recent_projects` at the top of render; notify so
        // the new order paints this frame.
        cx.notify();
    }

    /// Persist a drag-reorder of a workspace row within its project group:
    /// write `moved_id`'s new rank next to `target_id`, then re-gather the
    /// rail's row cache so the new order paints. Cross-group drops are already
    /// rejected at the card; a missing id is a benign no-op at the repo layer.
    pub(crate) fn reorder_workspace(
        &mut self,
        moved_id: String,
        target_id: String,
        project_id: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.app_state.workspace_repo.reorder_to_target(
            &moved_id,
            &target_id,
            &project_id,
        ) {
            tracing::warn!(?err, %moved_id, "reorder_workspace: persist failed");
            return;
        }
        // Workspace rows live in the rail's background-gathered cache; mark it
        // dirty so the new sort_order is re-read, then notify to repaint.
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Register a folder dropped onto the left rail and make it the active
    /// project. Same registration path as the add-project dialog —
    /// `insert_or_touch` is idempotent, so dropping an already-known root
    /// just re-activates it.
    pub(crate) fn add_project_from_drop(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_str = path.to_string_lossy().to_string();
        let name = crate::shell::add_project_dialog::name_from_path(&path);
        match self.app_state.project_repo.insert_or_touch(
            &name,
            &path_str,
            crate::shell::add_project_dialog::DEFAULT_BRANCH_PLACEHOLDER,
        ) {
            Ok(project) => {
                self.refresh_recent_projects();
                self.set_active_project(project, window, cx);
            }
            Err(err) => {
                tracing::warn!(?err, path = %path_str, "dropped-folder registration failed");
            }
        }
    }

    /// Push hidden-state to every project's terminals except `active_id`, so a
    /// background project's tabs throttle their PTY poll even though their
    /// `ProjectPanes` is no longer in the render tree (the per-render
    /// visibility sweep can't reach them). The active project re-syncs itself
    /// on its next render.
    ///
    /// Invariant: every terminal-spawn path routes through
    /// `active_project_panes()`, so a freshly spawned terminal always lands in
    /// the active (visible) project. Any future path that spawns directly into
    /// a non-active project must call this afterwards to keep it throttled.
    fn hide_inactive_project_terminals(&self, active_id: &str, cx: &mut Context<Self>) {
        let inactive: Vec<_> = self
            .project_panes_by_project
            .iter()
            .filter(|(id, _)| id.as_str() != active_id)
            .map(|(_, panes)| panes.clone())
            .collect();
        for panes in inactive {
            panes.update(cx, |p, pcx| p.hide_all_terminals(pcx));
        }
    }

    /// Build and cache a project's panes if they do not exist yet, returning the
    /// entity either way.
    ///
    /// Deliberately independent of which project is *active*. Building a
    /// project's panes is what constructs its chat views, and constructing those
    /// is what registers their sessions for remote control — so a remote client
    /// reaching for a session in a project the user has not opened needs this to
    /// run without the desktop's own view changing under them.
    pub(crate) fn build_project_panes_if_absent(
        &mut self,
        project_id: &str,
        project_root: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<crate::shell::project_panes::ProjectPanes> {
        if let Some(panes) = self.project_panes_by_project.get(project_id) {
            return panes.clone();
        }
        let window_id = self.window_id.clone();
        let project_root = project_root.to_path_buf();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cli_runtime = self.cli_runtime.clone();
        let notifier = self.notifier.clone();
        let snapshot = match load_persisted_tabs(
            &self.app_state.settings_repo,
            project_id,
            &window_id,
        ) {
            crate::project_panes_factory::LoadedTabs::Snapshot(s) => Some(s),
            crate::project_panes_factory::LoadedTabs::Absent => None,
            crate::project_panes_factory::LoadedTabs::Corrupt => {
                // Payload preserved aside by the loader; boot continues
                // on the default layout so a damaged blob can never
                // keep the app from reaching a usable window.
                self.push_toast(
                    crate::shell::toast::ToastKind::Error,
                    "Layout could not be restored — reset to default",
                    cx,
                );
                None
            }
        };
        let pane_buffers = crate::project_panes_factory::load_pane_buffers(
            &self.app_state.pane_buffer_repo,
            project_id,
            &window_id,
        );
        let pane_relay_ids = self
            .app_state
            .pane_relay_id_repo
            .get_all_for_project(project_id, &window_id)
            .unwrap_or_else(|err| {
                tracing::warn!(?err, project_id = %project_id, "load pane_relay_ids failed");
                Vec::new()
            });
        // NO daemon round-trips on this path: the panes build mounts
        // pending placeholders only, so first paint isn't gated behind
        // per-tab attach/spawn RPCs. The reconcile spawned below does
        // the relay work after paint and swaps live sessions in.
        let build_started = std::time::Instant::now();
        let (panes, pending_attaches) = build_project_panes(
            project_root.clone(),
            snapshot,
            pane_buffers,
            pane_relay_ids.clone(),
            theme,
            density,
            typography,
            cli_runtime,
            notifier,
            window,
            cx,
        );
        tracing::info!(
            project_id = %project_id,
            pending = pending_attaches.len(),
            elapsed_ms = build_started.elapsed().as_millis() as u64,
            "project panes built (pre-paint, no relay RPCs)"
        );
        crate::project_panes_factory::spawn_attach_reconcile(
            pane_relay_ids,
            pending_attaches,
            window,
            cx,
        );
        // Install the save sink keyed to this project and window.
        let settings_repo = self.app_state.settings_repo.clone();
        let project_id_for_cb = project_id.to_string();
        let window_id_for_cb = window_id.clone();
        let save_cb: crate::shell::project_panes::SaveCallback =
            std::sync::Arc::new(move |snap| {
                save_persisted_tabs(&settings_repo, &project_id_for_cb, &window_id_for_cb, &snap)
            });
        panes.update(cx, |p, _| p.set_save_callback(save_cb));
        self.project_panes_by_project
            .insert(project_id.to_string(), panes.clone());
        panes
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
        // The incoming project may be brand new (add-project flows route
        // here after refresh_recent_projects) — pull its workspace rows
        // into the rail cache.
        self.mark_rail_dirty(cx);
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
            // Cached session id only — capturing relay ids needs the session
            // id, not the live PTY list, so skip the ListPtys daemon RPC on
            // this main-thread project-switch path.
            if let Some(session_id) = crate::shell::terminal_view::relay_session_id_cached() {
                let relay_repo = self.app_state.pane_relay_id_repo.clone();
                panes.read(cx).capture_pane_relay_ids(
                    &relay_repo,
                    &outgoing,
                    &window_id,
                    &session_id,
                    cx,
                );
            }
        }
        // Carry the sidebar open/collapsed state across the switch (it's a
        // global UI preference, not per-project) and pause the outgoing
        // sidebar's status poller so only the active project polls git —
        // otherwise every cached sidebar would keep ticking its own
        // `git status` in the background.
        //
        // No sidebar yet means this is the window's FIRST activation (boot
        // builds no sidebar — see `WorkspaceRoot::new`), so the default is the
        // long-standing "default-collapsed on app boot", not open.
        let prior_open = self
            .right_sidebar
            .as_ref()
            .map(|s| s.read(cx).open)
            .unwrap_or(false);
        if let Some(outgoing_sidebar) = self.right_sidebar.as_ref() {
            outgoing_sidebar.read(cx).set_polling_focused(false);
        }
        self.active_project = Some(project.clone());
        // Throttle every other project's terminals: only the active project's
        // ProjectPanes renders, so inactive projects' PaneGroups never run the
        // visibility sweep and would otherwise keep polling at the foreground
        // cadence. Push hidden-state to them here on the switch; the incoming
        // project self-corrects on its own next render.
        self.hide_inactive_project_terminals(&project.id, cx);
        // Reload custom commands for the new project so the palette reflects
        // the incoming project's `.oximux/commands.toml` immediately.
        self.reload_custom_commands(cx);
        // Drop the Quick Open file index so the next open re-scans the new
        // project (prevents the previous project's files leaking through).
        self.palette
            .update(cx, |p, cx| p.invalidate_file_index(cx));
        let project_root = PathBuf::from(&project.root_path);
        // `git config user.name` can be set per repository, so the branch
        // prefix is a property of the project, not of the app. Re-resolve it
        // on the switch; until it lands the previous project's answer stands,
        // which is the same value the previews were already showing.
        crate::git_settings::refresh_username(Some(project_root.clone()), cx);
        // ...and tell the settings modal which repository its Git pane should
        // preview against, so its branch line agrees with the dialog's.
        self.settings_modal.update(cx, |m, _| m.set_project_root(Some(project_root.clone())));
        // Lazy-build the project's panes entity on first activation. Subsequent
        // switches just resolve the existing entity via `active_project_panes()`
        // — pane-group + tab state survives the switch.
        // Both arms of the old branch ended the same way, so building and
        // activating are now separate concerns: build (or resolve) the entity,
        // then point the observer and focus at it.
        let panes = self.build_project_panes_if_absent(&project.id, &project_root, window, cx);
        self._project_panes_observer = Some(cx.observe(&panes, |_, _, cx| cx.notify()));
        defer_focus_active(window, cx, panes);
        cx.notify();

        // A merge in this project may have left the user's uncommitted work in
        // the stash stack. Re-offer to restore it until they act on it — the
        // toast that first said so is long gone, and possibly so is the app
        // session it appeared in. Deferred so it mounts after activation has
        // settled rather than into the middle of it.
        let weak_for_notices: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let project_for_notices = project.clone();
        window.defer(cx, move |window, cx| {
            let _ = weak_for_notices.update(cx, |this, cx| {
                this.offer_pending_stash_notices(&project_for_notices, window, cx);
                // Replace the `"main"` placeholder written at project-add with
                // what the repository actually says, so the `Merge into <x>`
                // row is labelled — and gated — on a branch that exists.
                this.heal_default_branch(&project_for_notices, window, cx);
            });
        });

        // Fast path: reuse a sidebar already built for this project this
        // session (cache-and-revalidate, like the terminal panes above)
        // instead of tearing it down and rebuilding. Carry the global open
        // state, resume its poller (kicks an immediate git revalidation),
        // re-point the SCM subscriptions at its live panels, refocus — done.
        // No repo re-open, no commit-graph reload, no file-tree rescan, no
        // "Loading…" flash. First activation of a project falls through to the
        // async build below, which inserts the new sidebar into the cache.
        if let Some(cached) = self.right_sidebar_by_project.get(&project.id).cloned() {
            cached.update(cx, |s, _| s.open = prior_open);
            cached.read(cx).set_polling_focused(true);
            // Re-assert the window's ports panel. A sidebar cached before the
            // panel existed (or one restored into a different window) would
            // otherwise render the Ports tab empty forever.
            let ports_panel = self.ports_panel.clone();
            cached.update(cx, |s, cx| s.set_ports_panel(ports_panel, cx));
            self.right_sidebar = Some(cached);
            self.rewire_scm_subscriptions(window, cx);
            // RT-3: forward the new project to any open Tasks tab so the list
            // updates without requiring a manual Refresh.
            self.refresh_tasks_tab_for_active_project(Some(project), cx);
            refocus_active_pane(self, window, cx);
            cx.notify();
            return;
        }

        let project_id_for_cache = project.id.clone();
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
                // left it, not snap back open on every project switch. No
                // sidebar yet = first activation of this window, which starts
                // collapsed (the "default-collapsed on app boot" behavior).
                let prior_open = this
                    .right_sidebar
                    .as_ref()
                    .map(|s| s.read(cx).open)
                    .unwrap_or(false);
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
                let worktree_settings_repo =
                    Some(this.app_state.worktree_settings_repo.clone());
                // Phase 13: load persisted panel width clamped against
                // the current window so a too-large persisted value
                // can't overflow a newly-smaller window. The settings
                // repo is shared app-wide via the same DB handle.
                let window_width = f32::from(window.bounds().size.width);
                let settings_repo = this.app_state.settings_repo.clone();
                let initial_width = gpui::px(
                    crate::scm_layout_settings::load_panel_width(&settings_repo, window_width),
                );
                let layout_boot = crate::shell::right_sidebar::SidebarLayoutBoot {
                    initial_width: Some(initial_width),
                    settings_repo: Some(settings_repo),
                };
                let built = cx.new(|cx| {
                    crate::shell::right_sidebar::RightSidebar::new(
                        repo,
                        project_root.clone(),
                        prior_open,
                        Some(on_open),
                        on_open_diff,
                        Some(on_query),
                        worktree_settings_repo,
                        layout_boot,
                        theme,
                        density,
                        typography,
                        window,
                        cx,
                    )
                });
                // Cache the freshly built sidebar so a later switch back to
                // this project reuses it (fast path above) instead of
                // rebuilding from scratch.
                let ports_panel = this.ports_panel.clone();
                built.update(cx, |s, cx| s.set_ports_panel(ports_panel, cx));
                this.right_sidebar_by_project
                    .insert(project_id_for_cache, built.clone());
                this.right_sidebar = Some(built);
                // The rebuild minted fresh SCM panel entities — re-point
                // every source-control event subscription at them, or the
                // "View all" / commit / branch / discard / stash actions
                // would silently stop firing after a project switch.
                this.rewire_scm_subscriptions(window, cx);
                // RT-3: forward the new project to any open Tasks tab.
                let active_proj = this.active_project.clone();
                this.refresh_tasks_tab_for_active_project(active_proj, cx);
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

    /// Activate the workspace clicked in the left rail: switch to its
    /// owning project, record the selection (drives the active-row
    /// highlight), then focus the agent tab already running in its
    /// worktree if one exists.
    ///
    /// When no matching tab is found the project + selection still
    /// update, but we stop short of spawning: launching a fresh agent
    /// for an empty worktree needs an adapter choice, so that path is a
    /// deliberate follow-up rather than an arbitrary auto-start.
    pub(crate) fn activate_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An archived row is hidden, not open: it renders only inside a
        // collapsed disclosure, it is absent from the flat list's live section,
        // and the tab-strip tint resolves against active rows only. Activating
        // one would leave the rail pointing at a workspace it cannot show as
        // selected. Say so and make `Unarchive` the way back in, rather than
        // swallowing the click.
        if workspace.archived_at.is_some() {
            crate::shell::toast::toast(
                cx,
                crate::shell::toast::ToastKind::Info,
                format!(
                    "\u{201c}{}\u{201d} is archived \u{2014} restore it first to open its worktree",
                    workspace.name,
                ),
            );
            return;
        }
        // Switch to the owning project first so the correct ProjectPanes
        // is live before we search it for the worktree's tab.
        let already_active =
            self.active_project.as_ref().map(|p| p.id.as_str()) == Some(workspace.project_id.as_str());
        if !already_active {
            match self
                .app_state
                .recent_projects
                .iter()
                .find(|p| p.id == workspace.project_id)
                .cloned()
            {
                Some(project) => self.set_active_project(project, window, cx),
                // The owning project is gone (e.g. removed mid-session, or a
                // stale jump-list entry). Without the switch the activation
                // can't focus the tab — surface it rather than no-op silently.
                None => {
                    tracing::warn!(
                        project_id = %workspace.project_id,
                        workspace_id = %workspace.id,
                        "activate_workspace: owning project not in recent_projects; skipping"
                    );
                    return;
                }
            }
        }

        self.active_workspace_id = Some(workspace.id.clone());
        self.record_nav(&workspace.project_id, &workspace.id);

        let worktree_path = PathBuf::from(&workspace.worktree_path);
        if let Some(panes) = self.active_project_panes() {
            let focused =
                panes.update(cx, |p, cx| p.focus_workspace_tab(&worktree_path, window, cx));
            if !focused {
                tracing::info!(
                    workspace_id = %workspace.id,
                    path = %worktree_path.display(),
                    "activate_workspace: no agent tab for this worktree; selection set, spawn deferred"
                );
            }
            // Re-assert focus on the next frame: focusing synchronously inside a
            // sidebar-row mouse-down handler gets clobbered by GPUI's post-click
            // focus dispatch, leaving the terminal unfocused. The deferred pass
            // lands keyboard focus on the tab so it's ready for input.
            defer_focus_active(window, cx, panes);
        }
        cx.notify();
    }

    /// Open an existing workspace from a create/start action: activate it
    /// (focusing its agent tab), return the rail to its home list, and close
    /// the Tasks tab that may have launched the action. Shared by the
    /// create-success path and the "workspace already exists" short-circuit so
    /// both land identically. `go_home` runs AFTER `activate_workspace` (which
    /// switches project but never touches `active_nav`) so the rail reliably
    /// ends on the home view.
    pub(crate) fn open_existing_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_workspace(workspace, window, cx);
        self.left_rail.update(cx, |rail, cx| rail.go_home(cx));
        if let Some(panes) = self.active_project_panes() {
            panes.update(cx, |p, cx| p.close_tasks_tab_in_active_group(window, cx));
        }
    }

    /// Land a notification click on its agent tab: activate the owning
    /// project (cross-project included), select + locate the owning
    /// workspace in the rail, focus the exact tab, and raise the window.
    /// A stale click (tab closed since) does nothing.
    pub(crate) fn navigate_to_agent_tab(
        &mut self,
        tab_id: crate::notifier::TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve the owning project by searching every cached panes
        // entity — agent tabs survive project switches inside them, and a
        // project that was never activated cannot own a live agent.
        let owner = self.project_panes_by_project.iter().find_map(|(id, panes)| {
            panes
                .read(cx)
                .agent_worktree_for_tab_id(tab_id, cx)
                .map(|wt| (id.clone(), panes.clone(), wt))
        });
        let Some((project_id, panes, worktree_path)) = owner else {
            tracing::info!(tab_id = tab_id.0, "notification click for a closed agent tab; ignoring");
            return;
        };
        if self.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str()) {
            let Some(project) = self
                .app_state
                .recent_projects
                .iter()
                .find(|p| p.id == project_id)
                .cloned()
            else {
                tracing::warn!(%project_id, "notification click: owning project not in recent_projects");
                return;
            };
            self.set_active_project(project, window, cx);
        }
        // Rail selection + locate affordance for the owning workspace
        // (match by worktree path; the synthesized primary row covers
        // agents running at the project root).
        let worktree_str = worktree_path.to_string_lossy().into_owned();
        let workspace = self
            .rail_workspaces_by_project
            .get(&project_id)
            .and_then(|rows| rows.iter().find(|w| w.worktree_path == worktree_str))
            .cloned();
        if let Some(w) = workspace {
            self.active_workspace_id = Some(w.id.clone());
            self.record_nav(&w.project_id, &w.id);
        }
        self.left_rail.update(cx, |rail, cx| rail.scroll_to_active(cx));
        panes.update(cx, |p, cx| {
            p.set_active_by_tab_id(tab_id, window, cx);
        });
        window.activate_window();
        cx.notify();
    }

    /// Notification-click navigation for a terminal-bell banner: find the
    /// pane group owning the ringing session (cross-project included),
    /// select + locate the owning workspace in the rail by the group's
    /// cwd, activate the exact tab, and raise the window. A stale click
    /// (tab closed since) does nothing.
    pub(crate) fn navigate_to_terminal_session(
        &mut self,
        session: oximux_pty::TerminalSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = self.project_panes_by_project.iter().find_map(|(id, panes)| {
            panes
                .read(cx)
                .group_cwd_for_terminal_session(session, cx)
                .map(|cwd| (id.clone(), panes.clone(), cwd))
        });
        let Some((project_id, panes, group_cwd)) = owner else {
            tracing::info!(
                session = session.0,
                "bell notification click for a closed terminal; ignoring"
            );
            return;
        };
        if self.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str()) {
            let Some(project) = self
                .app_state
                .recent_projects
                .iter()
                .find(|p| p.id == project_id)
                .cloned()
            else {
                tracing::warn!(%project_id, "bell click: owning project not in recent_projects");
                return;
            };
            self.set_active_project(project, window, cx);
        }
        // Rail selection by the owning group's cwd (best-effort -- a
        // terminal can cd anywhere, but the group cwd is its workspace).
        let cwd_str = group_cwd.to_string_lossy().into_owned();
        let workspace = self
            .rail_workspaces_by_project
            .get(&project_id)
            .and_then(|rows| rows.iter().find(|w| w.worktree_path == cwd_str))
            .cloned();
        if let Some(w) = workspace {
            self.active_workspace_id = Some(w.id.clone());
            self.record_nav(&w.project_id, &w.id);
        }
        self.left_rail.update(cx, |rail, cx| rail.scroll_to_active(cx));
        panes.update(cx, |p, cx| {
            p.activate_terminal_session(session, window, cx);
        });
        window.activate_window();
        cx.notify();
    }

    /// A project's workspace rows, always including the synthesized "primary"
    /// (repo-root) row as the first entry when no real workspace occupies the
    /// root. Single source of the "a project is never an empty group"
    /// invariant — shared by the left-rail snapshot and the Cmd+J jump list so
    /// both surface the same rows (incl. the primary that is not a DB row).
    ///
    /// Does SQLite + a filesystem stat — event-driven callers only. The
    /// render path reads the cache the rail gather builds with the free
    /// function below instead.
    pub(crate) fn workspaces_with_primary(&self, project: &Project) -> Vec<Workspace> {
        workspaces_with_primary_for(&self.app_state.workspace_repo, project)
    }

    /// Build the Cmd+J jump candidates: every workspace across all projects
    /// (incl. synthesized primaries), labeled `Project · branch/slug` and
    /// tagged with its `attention_rank` so action-needing ones float to the
    /// top of the browse order.
    pub(crate) fn build_workspace_jump_items(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<crate::shell::command_palette::entry::WorkspaceJumpItem> {
        use crate::shell::agents_dashboard::model::attention_rank;
        use crate::shell::command_palette::entry::WorkspaceJumpItem;

        let mut live: HashSet<String> = HashSet::new();
        for panes in self.project_panes_by_project.values() {
            live.extend(panes.read(cx).live_worktree_paths(cx));
        }

        let mut items = Vec::new();
        for project in &self.app_state.recent_projects {
            for w in self.workspaces_with_primary(project) {
                let status = self
                    .app_state
                    .agent_session_repo
                    .list_for_workspace(&w.id)
                    .ok()
                    .and_then(|mut s| s.drain(..).next().map(|s| s.status));
                let is_live = live.contains(&w.worktree_path);
                // A workspace with no agent session is ready-to-jump, not an
                // error — keep dormant rows above the failed/interrupted tier
                // (which is where `attention_rank(None, false)` would put them)
                // so a fresh project's primary row never sinks below errors.
                let attention = match status.as_ref() {
                    Some(s) => attention_rank(Some(s), is_live),
                    None if is_live => 2,
                    None => 3,
                };
                let branch_or_slug = if !w.branch.is_empty() {
                    w.branch.as_str()
                } else {
                    w.slug.as_str()
                };
                let label = if branch_or_slug.is_empty() {
                    project.name.clone()
                } else {
                    format!("{} · {}", project.name, branch_or_slug)
                };
                items.push(WorkspaceJumpItem {
                    workspace_id: w.id.clone(),
                    project_id: w.project_id.clone(),
                    worktree_path: w.worktree_path.clone(),
                    label,
                    attention,
                });
            }
        }
        items
    }

    /// Resolve a Cmd+J jump activation: build a minimal `Workspace` from the
    /// carried identity (the only fields `activate_workspace` reads) and
    /// activate it. Works for synthesized primary rows too, since their
    /// `worktree_path` is the project root.
    pub(crate) fn activate_workspace_from_jump(
        &mut self,
        workspace_id: String,
        project_id: String,
        worktree_path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = Workspace {
            id: workspace_id,
            project_id,
            name: String::new(),
            slug: String::new(),
            branch: String::new(),
            worktree_path,
            status: String::new(),
            created_at: String::new(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        };
        self.activate_workspace(workspace, window, cx);
    }

    /// Record a workspace activation in this window's back/forward history.
    /// Browser semantics: a fresh activation truncates any forward entries,
    /// then appends. Skipped while replaying a back/forward step, and
    /// deduped against the current cursor entry so re-activating the same
    /// workspace doesn't grow the stack.
    pub(crate) fn record_nav(&mut self, project_id: &str, workspace_id: &str) {
        if self.nav_replaying {
            return;
        }
        let entry = WorkspaceNavRef {
            project_id: project_id.to_string(),
            workspace_id: workspace_id.to_string(),
        };
        self.nav_cursor =
            push_nav_entry(&mut self.nav_history, self.nav_cursor, entry, MAX_NAV_HISTORY);
    }

    /// Mark `workspace_key` as the selected rail workspace and push it onto
    /// the nav history. The rail's active-row highlight is driven entirely by
    /// `active_workspace_id`, so every path that lands focus on a workspace's
    /// tab must call this — otherwise the highlight stays on the previously
    /// selected workspace while the panes show another. `workspace_key` is the
    /// rail row id: a real workspace UUID, or `primary:<project_id>` for a
    /// repo-root row (the same key carried by a live agent's `workspace_key`).
    /// Mirrors the selection writes in `activate_workspace`; the caller issues
    /// `cx.notify()` so the next render re-reads the field into the rail.
    pub(crate) fn select_rail_workspace(&mut self, project_id: &str, workspace_key: &str) {
        self.active_workspace_id = Some(workspace_key.to_string());
        self.record_nav(project_id, workspace_key);
    }

    /// Step back one entry in the workspace-activation history. No-op at the
    /// oldest entry. If the target entry is stale (workspace deleted), the
    /// cursor is reverted so it stays anchored to the displayed workspace.
    pub(crate) fn nav_workspace_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.nav_step(false, window, cx);
    }

    /// Step forward to the next live entry. No-op at the newest live entry.
    pub(crate) fn nav_workspace_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.nav_step(true, window, cx);
    }

    /// Walk the history in `forward` direction from the cursor to the first
    /// entry that still resolves to a live workspace, skipping stale entries
    /// (workspaces deleted since they were recorded). Lands the cursor on that
    /// entry and activates it without re-recording; no-op (cursor unchanged) if
    /// only stale entries or the boundary lie ahead. Skipping (vs reverting on
    /// the first stale hit) is what keeps a deleted mid-history workspace from
    /// permanently walling off everything beyond it.
    fn nav_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((idx, workspace)) = self.find_live_nav_target(forward) else {
            return;
        };
        self.nav_cursor = idx;
        self.nav_replaying = true;
        self.activate_workspace(workspace, window, cx);
        self.nav_replaying = false;
    }

    /// Find the first live history entry from the cursor in `forward` direction,
    /// returning its index + resolved workspace. `None` at the boundary.
    fn find_live_nav_target(&self, forward: bool) -> Option<(usize, Workspace)> {
        let idx = next_live_index(self.nav_history.len(), self.nav_cursor, forward, |i| {
            self.nav_history
                .get(i)
                .is_some_and(|e| self.workspace_by_nav_ref(e).is_some())
        })?;
        let workspace = self.workspace_by_nav_ref(self.nav_history.get(idx)?)?;
        Some((idx, workspace))
    }

    /// Resolve a history ref to a live `Workspace`, including the synthesized
    /// "primary" row. `None` when the project or workspace is gone.
    fn workspace_by_nav_ref(&self, entry: &WorkspaceNavRef) -> Option<Workspace> {
        let project = self
            .app_state
            .recent_projects
            .iter()
            .find(|p| p.id == entry.project_id)
            .cloned()?;
        self.workspaces_with_primary(&project)
            .into_iter()
            .find(|w| w.id == entry.workspace_id)
    }

    /// Close every full-window modal overlay. Callers invoke this before
    /// opening a new overlay so two inset-0 dismiss regions never compete.
    pub(crate) fn close_modal_overlays(&mut self, cx: &mut Context<Self>) {
        self.palette.update(cx, |p, cx| p.close(cx));
        self.pane_actions.update(cx, |p, cx| p.close(cx));
        self.adapter_picker.update(cx, |p, cx| p.close(cx));
        self.project_picker.update(cx, |p, cx| p.close(cx));
        self.settings_modal.update(cx, |m, cx| m.close(cx));
        self.workspace_dialog.update(cx, |d, cx| d.close(cx));
        self.row_menu.update(cx, |m, cx| m.close(cx));
        self.project_menu.update(cx, |m, cx| m.close(cx));
        self.dashboard_status_menu.update(cx, |m, cx| m.close(cx));
        self.options_menu.update(cx, |m, cx| m.close(cx));
        self.add_project_dialog.update(cx, |d, cx| d.close(cx));
        self.session_history.update(cx, |m, cx| m.close(cx));
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
        // Load lifecycle scripts so the menu only surfaces Run-* rows for
        // scripts the project actually defines. The worktree carries the
        // committed `.oximux/scripts.toml`, so load from its path.
        // `run_workspace_script` re-reads the file on click so an edit made
        // while the menu was open is still respected.
        let scripts =
            crate::project_scripts_loader::load_for_project(Path::new(&workspace.worktree_path));
        let avail = ScriptAvail {
            setup: scripts.script(ScriptKind::Setup).is_some(),
            run: scripts.script(ScriptKind::Run).is_some(),
            cleanup: scripts.script(ScriptKind::Cleanup).is_some(),
        };
        // The `Merge into <x>` row is labelled with the row's OWN project's
        // default branch, not the active project's — the rail shows every
        // project's workspaces at once, and merging into the wrong repository's
        // main is the mistake Phase 1 already had to fix once for Delete.
        let default_branch =
            resolve_project_for_workspace(&self.app_state.recent_projects, &workspace)
                .map(|p| p.default_branch)
                .unwrap_or_default();
        self.row_menu
            .update(cx, |m, cx| m.open(workspace, avail, default_branch, x, y, cx));
        // The rail suppresses the `…` trigger's tooltip while this is up. An
        // already-visible tooltip is sticky — `occlude` stops new hovers, but
        // clearing a live one needs a hover-out, which needs a mouse move, and
        // the pointer is parked on the trigger the user just clicked.
        self.left_rail
            .update(cx, |rail, cx| rail.set_row_menu_open(true, cx));
    }

    /// Run a per-project lifecycle script (setup/run/cleanup) for `workspace`
    /// in a real, interactive terminal tab rooted at its worktree. No-ops
    /// (with a log) when the script is undefined — the menu shouldn't have
    /// offered it, but stay defensive against a config that changed on disk
    /// between menu-open and click.
    pub(crate) fn run_workspace_script(
        &mut self,
        workspace: Workspace,
        kind: ScriptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = PathBuf::from(&workspace.worktree_path);
        let scripts = crate::project_scripts_loader::load_for_project(&cwd);
        let Some(script) = scripts.script(kind) else {
            tracing::info!(
                kind = kind.as_str(),
                worktree = %workspace.worktree_path,
                "run_workspace_script: script not defined; ignoring"
            );
            return;
        };
        let title = format!("{}: {}", kind.as_str(), workspace.name);
        let script = script.to_string();
        let Some(panes) = self.active_project_panes() else {
            tracing::warn!("run_workspace_script: no active project panes");
            return;
        };
        panes.update(cx, |p, cx| {
            p.open_script_terminal_tab_in_active_group(cwd, title.into(), &script, window, cx);
        });
    }

    /// Open the per-project-header action popover at the given screen
    /// coordinates. Closes any other overlays first so backdrops don't
    /// compete.
    pub(crate) fn open_project_menu(
        &mut self,
        project: oximux_core::Project,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.close_modal_overlays(cx);
        self.project_menu
            .update(cx, |m, cx| m.open(project, x, y, cx));
    }

    /// Open the Agents-page status-filter dropdown at the given screen
    /// coordinates, with `active` shown as checked. Closes any other overlays
    /// first so backdrops don't compete.
    pub(crate) fn open_dashboard_status_menu(
        &mut self,
        active: crate::shell::agents_dashboard::filter::StatusFilter,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.close_modal_overlays(cx);
        self.dashboard_status_menu
            .update(cx, |m, cx| m.open(active, x, y, cx));
    }

    /// Open the Projects-header display-options dropdown at the given screen
    /// coordinates, seeded with the rail's current sort / group / card-layout
    /// state. Closes any other overlays first so backdrops don't compete.
    pub(crate) fn open_options_menu(
        &mut self,
        sort_mode: crate::shell::left_rail::workspace_list_render::WorkspaceSortMode,
        group_mode: crate::shell::left_rail::workspace_list_render::WorkspaceGroupMode,
        compact: bool,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.close_modal_overlays(cx);
        self.options_menu
            .update(cx, |m, cx| m.open(sort_mode, group_mode, compact, x, y, cx));
    }

    /// Snapshot the sidebar data (all recent projects, their workspaces,
    /// and the latest agent-session status per workspace) and push it into
    /// `LeftRail`. Called at the top of `WorkspaceRoot::render` — LeftRail
    /// never reads `WorkspaceRoot` directly because doing so re-enters
    /// the entity slot during rendering and panics.
    /// Worktree directories that currently have an open PTY tab (terminal or
    /// agent), across every project whose panes have been built.
    ///
    /// Lifted out of [`Self::refresh_left_rail`] so it is callable from an
    /// action handler, not only from the render path. The rail's green "live"
    /// dot and `rename`'s in-use refusal must agree about what "live" means,
    /// and they only do so by reading the same function — a second
    /// implementation would drift and let the app refuse a rename for a
    /// worktree it is simultaneously drawing as idle.
    ///
    /// Ambient (hand-launched) agents are NOT included here: they are detected
    /// from terminal titles during the rail refresh and folded in there. A
    /// caller that needs them too must add them, which
    /// [`Self::rename_holders`] does.
    pub(crate) fn live_worktree_paths(
        &self,
        cx: &mut Context<Self>,
    ) -> std::collections::HashSet<String> {
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        for panes in self.project_panes_by_project.values() {
            live.extend(panes.read(cx).live_worktree_paths(cx));
        }
        live
    }


    pub(crate) fn refresh_left_rail(&mut self, cx: &mut Context<Self>) {
        let projects = self.app_state.recent_projects.clone();
        let active_project_id = self.active_project.as_ref().map(|p| p.id.clone());
        let active_workspace_id = self.active_workspace_id.clone();
        // Worktree paths with an open PTY tab (terminal or agent) — drives
        // the live (green) status dot. Aggregated across every project whose
        // panes have been built, so a worktree stays green while its session
        // lives even after switching to another project. Pure entity reads.
        let mut live_worktrees: std::collections::HashSet<String> =
            self.live_worktree_paths(cx);
        // Workspace rows + latest agent statuses come from the rail caches
        // (gathered on the background executor by `mark_rail_dirty`) —
        // render never touches SQLite or stats the filesystem.
        let mut workspaces_by_project: HashMap<String, Vec<Workspace>> =
            HashMap::with_capacity(projects.len());
        let mut archived_by_project: HashMap<String, Vec<Workspace>> =
            HashMap::with_capacity(projects.len());
        for project in &projects {
            workspaces_by_project.insert(
                project.id.clone(),
                self.rail_workspaces_by_project
                    .get(&project.id)
                    .cloned()
                    .unwrap_or_default(),
            );
            archived_by_project.insert(
                project.id.clone(),
                self.rail_archived_by_project
                    .get(&project.id)
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        // Ambient agent statuses inferred live from plain-terminal OSC titles
        // (a hand-launched `claude`/`codex`/… with no tracked session). Raw
        // terminal cwd is normalized to the owning workspace root, so a shell
        // that has `cd`'d into a subdirectory still groups under the worktree.
        // Per-PTY ambient agents: each hand-launched terminal is its own rail
        // row (the reference cockpit's per-pane identity), grouped under its
        // workspace by resolving the terminal's cwd to a worktree root. The
        // collapsed worktree→strongest-status map still drives the single-agent
        // card dot and the live (green) worktree set.
        let mut ambient_rows: Vec<crate::shell::session_merge::AmbientRow> = Vec::new();
        let mut ambient_status: HashMap<String, AmbientAgent> = HashMap::new();
        for panes in self.project_panes_by_project.values() {
            for entry in panes.read(cx).ambient_agents(cx) {
                let Some(worktree_path) = workspace_path_for_ambient_terminal(
                    &entry.cwd.to_string_lossy(),
                    &workspaces_by_project,
                ) else {
                    continue;
                };
                let replace = ambient_status.get(&worktree_path).is_none_or(|cur| {
                    crate::shell::agent_presentation::ambient_status_rank(&entry.agent.status)
                        > crate::shell::agent_presentation::ambient_status_rank(&cur.status)
                });
                if replace {
                    ambient_status.insert(worktree_path.clone(), entry.agent.clone());
                }
                ambient_rows.push(crate::shell::session_merge::AmbientRow {
                    pty_id: entry.pty_id,
                    worktree_path,
                    agent: entry.agent,
                });
            }
        }
        live_worktrees.extend(ambient_status.keys().cloned());
        let latest_status = self.rail_latest_status.clone();
        let latest_adapter = self.rail_latest_adapter.clone();
        let last_active = self.rail_last_active.clone();
        // Diff counts are refreshed out-of-band by the periodic, focus-gated
        // refresh loop (see `WorkspaceRoot::run_diff_refresh_round`); here we
        // only read the latest cached snapshot. Render never shells out to git.
        let diff_counts_snapshot = self.diff_counts.clone();
        let agent_activity_snapshot = self.agent_activity.clone();
        let agent_sideband_snapshot = self.agent_sideband.clone();
        // Merge live runtime sessions (`live_agents`) with each workspace's DB
        // history into per-workspace agent lists. Live entries win on the
        // shared UUID; terminal history older than 24h is culled (live rows are
        // always kept). The single-row caches above are untouched, so the
        // collapsed dot is unchanged until the disclosure UI lands.
        // Rebuild the DB+live merge only when a session/live-agent changed; on a
        // plain output frame reuse the cache so streaming doesn't rebuild 150+
        // rows. The cheap maps above are still refreshed every frame, so live
        // worktree/diff changes still surface via the rail dirty-check.
        if self.rail_agents_dirty {
            let now = chrono::Utc::now();
            let history_cutoff = (now - chrono::Duration::hours(24)).to_rfc3339();
            let now_rfc3339 = now.to_rfc3339();
            self.rail_agents_cache = crate::shell::session_merge::build_workspace_agent_lists(
                &workspaces_by_project,
                &self.rail_workspace_sessions,
                &self.live_agents,
                Some(&history_cutoff),
                &now_rfc3339,
            );
            self.rail_agents_dirty = false;
        }
        // Ambient (plain-terminal) agents are cheap to detect and change with
        // their hook status, so they are appended to a CLONE of the cached merge
        // every frame — one row per PTY. Their visible fields (status, prompt)
        // are compared by `agents_display_equal`, so the rail dirty-check
        // repaints when one appears or changes.
        let mut workspace_agents: WorkspaceAgentList = self.rail_agents_cache.clone();
        crate::shell::session_merge::append_ambient_agent_rows(
            &workspaces_by_project,
            &ambient_rows,
            &mut workspace_agents,
        );
        // The agent whose tab is the active pane keeps its disclosure row lit.
        // Resolve it from the active project's panes, then map to the rail's row
        // identity (`RailAgentTarget`): a tracked session by its DB id, an
        // ambient terminal by its PTY id (the same per-pane key the rows use).
        let focused_agent: Option<RailAgentTarget> = self
            .active_project_panes()
            .and_then(|panes| panes.read(cx).focused_rail_agent(cx))
            .and_then(|focused| match focused {
                FocusedRailAgent::Session(sid) => self
                    .live_agents
                    .iter()
                    .find_map(|(db, e)| (e.session_id == sid).then(|| db.clone()))
                    .map(|db_id| RailAgentTarget::AgentSession { db_id }),
                FocusedRailAgent::AmbientTerminal { pty_id } => {
                    Some(RailAgentTarget::AmbientTerminal { pty_id })
                }
            });
        self.left_rail.update(cx, |rail, cx| {
            rail.set_sidebar_data(
                projects,
                active_project_id,
                active_workspace_id,
                workspaces_by_project,
                archived_by_project,
                latest_status,
                live_worktrees,
                ambient_status,
                latest_adapter,
                diff_counts_snapshot,
                agent_activity_snapshot,
                agent_sideband_snapshot,
                last_active,
                workspace_agents,
                focused_agent,
                cx,
            );
        });
    }

    /// Note that the sidebar's DB-backed data (workspace rows, latest
    /// agent-session statuses) may be stale and schedule ONE background
    /// gather. Bursts coalesce: while a gather is in flight, further calls
    /// only re-set the dirty flag and the running task loops once more.
    /// Call after every workspace/agent-session write, on project switch,
    /// and from the periodic diff tick (reconciliation net).
    pub(crate) fn mark_rail_dirty(&mut self, cx: &mut Context<Self>) {
        self.rail_dirty = true;
        // A rail-dirty signal means a session/live-agent change too, so the
        // cached agent merge must be rebuilt on the next render.
        self.rail_agents_dirty = true;
        if self.rail_refresh_inflight {
            return;
        }
        self.rail_refresh_inflight = true;
        cx.spawn(async move |weak, cx| {
            loop {
                let Ok((workspace_repo, agent_repo, projects)) = weak.update(cx, |this, _| {
                    this.rail_dirty = false;
                    (
                        this.app_state.workspace_repo.clone(),
                        this.app_state.agent_session_repo.clone(),
                        this.app_state.recent_projects.clone(),
                    )
                }) else {
                    return;
                };
                // All SQLite + the per-project `.git` stat run off the main
                // thread (cx.spawn itself stays on the main thread — known
                // footgun).
                let (workspaces, archived, statuses, adapters, last_active, sessions) = cx
                    .background_executor()
                    .spawn(async move {
                        gather_rail_db_data(&workspace_repo, &agent_repo, &projects)
                    })
                    .await;
                let run_again = weak.update(cx, |this, cx| {
                    this.rail_workspaces_by_project = workspaces;
                    this.rail_archived_by_project = archived;
                    this.rail_latest_status = statuses;
                    this.rail_latest_adapter = adapters;
                    this.rail_last_active = last_active;
                    this.rail_workspace_sessions = sessions;
                    // Fresh DB sessions landed — rebuild the cached agent merge.
                    this.rail_agents_dirty = true;
                    cx.notify();
                    if this.rail_dirty {
                        true
                    } else {
                        this.rail_refresh_inflight = false;
                        false
                    }
                });
                if !matches!(run_again, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    /// Record the live sideband detail for one workspace's agent, fed from its
    /// status watch channel by the persistence watcher. Stores `detail` only
    /// while the agent is `Running`; any other status clears the entry so a
    /// stale tool step never lingers on the card. Marks the rail dirty (one
    /// repaint) only on a visible change so a steady-state `Running` tick that
    /// carries the same tool doesn't churn the rail.
    pub(crate) fn note_agent_sideband(
        &mut self,
        workspace_key: &str,
        status: &oximux_core::AgentStatus,
        detail: Option<oximux_core::SidebandDetail>,
        cx: &mut Context<Self>,
    ) {
        let next = if matches!(status, oximux_core::AgentStatus::Running) {
            detail.filter(|d| d.tool_name.is_some() || d.last_message.is_some())
        } else {
            None
        };
        if self.agent_sideband.get(workspace_key) == next.as_ref() {
            return;
        }
        match next {
            Some(d) => {
                self.agent_sideband.insert(workspace_key.to_string(), d);
            }
            None => {
                self.agent_sideband.remove(workspace_key);
            }
        }
        self.mark_rail_dirty(cx);
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
                self.create_workspace_async(
                    project,
                    submit.name,
                    submit.agent,
                    // The manual dialog doesn't carry the issue URL, so no
                    // prompt prefill (the linked-issue badge still records it).
                    None,
                    submit.linked_issue,
                    submit.setup,
                    false,
                    window,
                    cx,
                );
            }
            WorkspaceDialogMode::Rename(workspace) => {
                self.rename_workspace_now(*workspace, submit.name, window, cx);
            }
        }
    }

    /// Create-workspace flow: derive slug → orchestrate via
    /// [`create_workspace_with_rollback`]. The helper is pure-async so
    /// the rollback path can be unit-tested without a GPUI context.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_workspace_async(
        &mut self,
        project: Project,
        name: String,
        agent: Option<AgentAdapter>,
        agent_prompt: Option<String>,
        linked_issue: Option<String>,
        // Per-request override for the project's `setup` script. `Inherit` —
        // every caller but the create dialog — defers to the project's
        // committed `auto_setup`.
        setup_decision: oximux_settings::SetupDecision,
        activate_after: bool,
        window: &mut Window,
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
        // Detect an existing workspace for this slug and OPEN it instead of
        // erroring on a duplicate worktree. The worktree path is deterministic
        // from (project, slug), so a re-create from the same issue resolves to
        // the same stored row — clicking "+ Workspace" twice should land on the
        // existing workspace's agent, not fail with `add_worktree` "already
        // exists".
        let worktree_path_str = worktree_path.to_string_lossy().to_string();
        if let Ok(Some(existing)) = self
            .app_state
            .workspace_repo
            .get_by_worktree_path(&worktree_path_str)
        {
            tracing::info!(
                workspace_id = %existing.id,
                slug = %slug,
                "workspace already exists; opening it instead of recreating"
            );
            if activate_after {
                self.open_existing_workspace(existing, window, cx);
            }
            return;
        }
        let workspace_repo = self.app_state.workspace_repo.clone();
        let project_root = PathBuf::from(&project.root_path);
        let project_id = project.id.clone();
        let name_trimmed = name.trim().to_string();
        // Resolved HERE, synchronously, from the same global the dialog's
        // preview line read — so the branch the user was shown is the branch
        // that gets made. Re-resolving inside the spawn would reintroduce the
        // gap this phase closed.
        let branch = crate::git_settings::branch_for_slug(&slug, Some(&project_root), cx);
        let freshen_default = crate::git_settings::settings(cx).keep_default_up_to_date;

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
            // Provisioning transcript. Written to the data dir rather than
            // into the worktree because the case that most needs reading is
            // the one where the worktree no longer exists — a failed setup
            // rolls it back, and a transcript inside it would go with it.
            let transcript_path = provisioning_transcript_path(&project_id, &slug);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ProvisionEvent>();
            // Drained on the background executor so the file grows while setup
            // runs: a 10-minute `pnpm install` is watchable with `tail -f`
            // instead of appearing as a frozen window.
            let writer = {
                let transcript_path = transcript_path.clone();
                cx.background_spawn(async move { stream_provisioning(transcript_path, rx).await })
            };
            let outcome = create_workspace_with_rollback(
                &project_root,
                &project_id,
                &name_trimmed,
                &slug,
                &branch,
                &worktree_path,
                linked_issue.as_deref(),
                &workspace_repo,
                // No per-request override from this path yet: the create dialog
                // has no setup toggle, so the project's `auto_setup` decides.
                // Reclaim opted into here and nowhere else in the desktop:
                // this path is host-derived under the data dir, and the flow
                // above has already looked for a workspace row naming it.
                &Provision::new(setup_decision, tx)
                    .reclaiming_orphans()
                    .freshening_default(freshen_default),
            )
            .await;
            // The sender is gone with `Provision`, so the drain has ended or is
            // about to; awaiting it means the file is complete before anything
            // below offers to open it.
            writer.await;
            match outcome {
                CreateOutcome::Created(workspace) => {
                    tracing::info!(
                        workspace_id = %workspace.id,
                        slug = %slug,
                        "workspace created"
                    );
                    let cwd = PathBuf::from(&workspace.worktree_path);
                    let _ = weak.update_in(cx, |this, window, cx| {
                        this.mark_rail_dirty(cx);
                        cx.notify();
                        // Land on the new workspace (e.g. created from a task):
                        // select it and return the rail to the home list so it's
                        // Land on the new workspace (e.g. created from a task):
                        // select it, return the rail home, and close the Tasks
                        // tab that launched the create. Manual dialog creates
                        // pass `false` to keep the prior behavior. The Tasks path
                        // always passes its currently-active project, so
                        // activate_workspace's recent-projects lookup resolves.
                        if activate_after {
                            this.open_existing_workspace(workspace.clone(), window, cx);
                        }
                        if let Some(kind) = agent {
                            // Auto-spawn from the create dialog launches the
                            // agent with its default settings — same one-click
                            // behavior as the `+` adapter picker.
                            let id = agent_adapter_id(kind);
                            this.spawn_agent_tab(
                                kind,
                                id,
                                cwd.clone(),
                                None,
                                None,
                                agent_prompt.clone(),
                                oximux_core::SessionResumption::None,
                                None,
                                // The create dialog offers no profile picker —
                                // an auto-spawn takes the adapter's default.
                                None,
                                window,
                                cx,
                            );
                        }
                        // `auto_setup` no longer opens a terminal tab here.
                        // Setup now runs inside `create_workspace_with_rollback`
                        // as provisioning, so a failure rolls the worktree back
                        // instead of leaving a red tab beside a worktree that
                        // reported success. By the time this runs, setup has
                        // already succeeded — running it again would repeat a
                        // `pnpm install`. The manual "Run setup" row is
                        // unchanged and remains the way to re-run it.
                        //
                        // `default_tabs` is what opens here instead: the shell
                        // layout a project declares every new worktree starts
                        // in. Opened after the agent tab so the agent is not
                        // buried, and each is a plain terminal at the worktree.
                        // Capped because `default_tabs` comes from a committed
                        // file and, unlike `setup`, needs no opt-in — cloning a
                        // repo and creating a worktree is enough to act on it.
                        // One PTY per entry with no bound would let a checked-in
                        // list of any length spawn that many processes.
                        let mut tabs =
                            crate::project_scripts_loader::load_for_project(&cwd).default_tabs;
                        if tabs.len() > MAX_DEFAULT_TABS {
                            tracing::warn!(
                                declared = tabs.len(),
                                cap = MAX_DEFAULT_TABS,
                                "default_tabs exceeds the cap; opening the first {MAX_DEFAULT_TABS}"
                            );
                            tabs.truncate(MAX_DEFAULT_TABS);
                        }
                        if !tabs.is_empty()
                            && let Some(panes) = this.active_project_panes()
                        {
                            panes.update(cx, |p, cx| {
                                for title in tabs {
                                    // Titles are repo-controlled too; a tab
                                    // label is not a place to render a kilobyte.
                                    let title: String =
                                        title.chars().take(MAX_TAB_TITLE_CHARS).collect();
                                    p.open_script_terminal_tab_in_active_group(
                                        cwd.clone(),
                                        title.into(),
                                        // No command: a declared tab is a shell
                                        // to work in, not a script to run. The
                                        // scripts are `.oximux/scripts.toml`.
                                        "",
                                        window,
                                        cx,
                                    );
                                }
                            });
                        }
                    });
                }
                CreateOutcome::SetupFailed {
                    transcript,
                    rollback_error,
                } => {
                    tracing::warn!(
                        slug = %slug,
                        outcome = %transcript.outcome.summary(),
                        ?rollback_error,
                        transcript = %transcript_path.display(),
                        "workspace create: setup failed, worktree rolled back"
                    );
                    let _ = weak.update_in(cx, |this, window, cx| {
                        this.mark_rail_dirty(cx);
                        // The transcript, not the summary, is what the user
                        // needs — the compiler error or the missing binary is
                        // in the script's own output. Opened in the editor so
                        // it stays reachable after the toast goes. The path is
                        // unique per attempt, so this is always this attempt's
                        // output and never a cached tab from the last one.
                        if let Some(panes) = this.active_project_panes() {
                            panes.update(cx, |p, cx| {
                                p.open_or_activate_editor_tab(transcript_path.clone(), window, cx);
                            });
                        }
                        let detail = match &rollback_error {
                            Some(err) => format!(
                                "{}. Rollback also failed ({err}) — manual cleanup required.",
                                transcript.outcome.summary()
                            ),
                            None => transcript.outcome.summary(),
                        };
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Set up workspace \u{201c}{slug}\u{201d}"),
                            &detail,
                        );
                    });
                }
                CreateOutcome::GitFailed(msg) => {
                    tracing::warn!(slug = %slug, error = %msg, "workspace create: git step failed");
                    let _ = weak.update(cx, |_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Create workspace \u{201c}{slug}\u{201d}"),
                            &msg,
                        );
                    });
                }
                CreateOutcome::StorageFailedRollbackClean(err) => {
                    tracing::warn!(
                        ?err,
                        slug = %slug,
                        "workspace create: insert failed, rollback clean"
                    );
                    let _ = weak.update(cx, |_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Create workspace \u{201c}{slug}\u{201d}"),
                            &err.to_string(),
                        );
                    });
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
                    let _ = weak.update(cx, |_, cx| {
                        crate::shell::toast::toast(
                            cx,
                            crate::shell::toast::ToastKind::Error,
                            format!(
                                "Create workspace \u{201c}{slug}\u{201d} failed and rollback left a partial worktree — clean up manually"
                            ),
                        );
                    });
                }
            }
        })
        .detach();
    }




    /// Mount `prompt` as the modal confirm dialog and arrange its teardown.
    ///
    /// The observer, not the callbacks, clears `confirm_dialog`: a callback
    /// that forgets leaves a dialog the user cannot dismiss, and every
    /// resolution path (confirm, secondary, Escape, click-outside) passes
    /// through the entity's own state.
    pub(crate) fn mount_confirm_dialog(
        &mut self,
        prompt: ConfirmPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));
        self._discard_dialog_observer = None;
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));
        self.confirm_dialog = Some(dialog);
        cx.notify();
    }




    /// Restore an archived workspace to its project's active group.
    ///
    /// The worktree directory was never removed by `Archive`, so restoring is
    /// a DB flip — but the directory may have been deleted by hand in the
    /// meantime. Stat it first and say so, rather than producing a row that
    /// looks live and cannot be activated.
    pub(crate) fn unarchive_workspace(&mut self, workspace: Workspace, cx: &mut Context<Self>) {
        if let Err(err) = self.app_state.workspace_repo.unarchive(&workspace.id) {
            tracing::warn!(?err, workspace_id = %workspace.id, "unarchive failed");
            crate::shell::toast::toast_op_error(
                cx,
                &format!("Restore workspace \u{201c}{}\u{201d}", workspace.slug),
                &err.to_string(),
            );
            return;
        }
        // The row is back either way; the toast tells the user the directory
        // behind it is gone so they can Delete rather than wonder why nothing
        // opens.
        if !Path::new(&workspace.worktree_path).is_dir() {
            crate::shell::toast::toast(
                cx,
                crate::shell::toast::ToastKind::Error,
                format!(
                    "Restored \u{201c}{}\u{201d}, but its worktree at {} is missing \u{2014} delete the row or recreate the worktree",
                    workspace.slug, workspace.worktree_path,
                ),
            );
        }
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Archive a workspace — `archived_at` + status='archived'. The
    /// sidebar will hide archived workspaces by default.
    pub(crate) fn archive_workspace(&mut self, workspace: Workspace, cx: &mut Context<Self>) {
        if let Err(err) = self.app_state.workspace_repo.mark_archived(&workspace.id) {
            tracing::warn!(?err, workspace_id = %workspace.id, "mark_archived failed");
        }
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Set (or clear) a workspace's identifier-hue swatch. Persists the slug;
    /// `cx.notify()` drives the next render's `refresh_left_rail` to re-read it.
    pub(crate) fn set_workspace_tint(
        &mut self,
        workspace_id: &str,
        tint: Option<crate::shell::pane_group::TabColor>,
        cx: &mut Context<Self>,
    ) {
        let slug = tint.map(|c| c.slug());
        if let Err(err) = self.app_state.workspace_repo.set_tint(workspace_id, slug) {
            tracing::warn!(?err, workspace_id, "set_workspace_tint failed");
            return;
        }
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Toggle a workspace's pin flag. Pinned rows float to the top of their
    /// project group in every sort mode. Persists the new flag, then re-reads
    /// the rail so the row re-sorts. A pin is a structural change and applies
    /// immediately (it is not subject to the Smart sort-settle window).
    pub(crate) fn toggle_workspace_pin(&mut self, workspace: Workspace, cx: &mut Context<Self>) {
        // Synthesized primary rows aren't real DB rows — pinning is meaningless
        // there (the primary is already anchored first).
        if workspace.id.starts_with("primary:") {
            return;
        }
        let next = !workspace.pinned;
        if let Err(err) = self.app_state.workspace_repo.set_pinned(&workspace.id, next) {
            tracing::warn!(?err, workspace_id = %workspace.id, "toggle_workspace_pin failed");
            return;
        }
        // A pin restructures the group order — clear any pending sort-settle so
        // the change is not held back by the debounce window.
        self.left_rail.update(cx, |rail, _| rail.clear_sort_settle());
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Open the confirm dialog for workspace deletion. On confirm:
    /// removes worktree + branch + DB row (FK cascade clears
    /// pane/agent sessions).
    ///
    /// When the previous delete of THIS workspace failed at worktree
    /// removal, the dialog becomes the Force Delete variant. The armed
    /// offer is consumed at request time — cancelling the force dialog
    /// therefore returns the NEXT attempt to a normal delete, which is
    /// intentional: the row still exists, so normal-first stays the
    /// default and force remains a deliberate two-step escalation.
    pub(crate) fn request_delete_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve the repository from the ROW's project, not the active one:
        // the rail shows every project's rows, so deleting project `api`'s row
        // while `web` is active would otherwise run `remove_worktree` — and on
        // the force retry `git branch -D <api's branch>` — inside `web`.
        let Some(target) = workspace_delete_target(&self.app_state.recent_projects, &workspace)
        else {
            tracing::info!(
                workspace_id = %workspace.id,
                project_id = %workspace.project_id,
                "request_delete_workspace: workspace's project not open, ignoring"
            );
            return;
        };
        // Second attempt after a failed worktree removal → the dialog
        // becomes the Force Delete variant: force-remove the worktree and
        // branch, ALWAYS drop the DB row, and report anything left behind.
        // Requesting delete on a different workspace drops the offer.
        let force = self.force_delete_offer.as_deref() == Some(workspace.id.as_str());
        self.force_delete_offer = None;
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let workspace_for_cb = workspace.clone();
        let target_for_cb = target.clone();
        let workspace_repo = self.app_state.workspace_repo.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |_window, cx| {
            let WorkspaceDeleteTarget {
                project_root,
                worktree_path,
                branch,
            } = target_for_cb.clone();
            let workspace_repo = workspace_repo.clone();
            let workspace = workspace_for_cb.clone();
            let weak = weak.clone();
            // Clear the confirm dialog up-front so the user can never get
            // stuck behind it on an early-return failure path (C1 — code-
            // review 260521-1306). The destructive intent has already
            // fired; subsequent errors surface as toasts below.
            let _ = weak.update(cx, |this, cx| {
                this.confirm_dialog = None;
                cx.notify();
            });
            cx.spawn(async move |cx| {
                // Run the project's cleanup script (if any) to completion BEFORE
                // touching the worktree, bounded by a timeout so a hung teardown
                // can't trap the user — on timeout the child is killed and the
                // removal proceeds regardless (the force-remove escape).
                run_cleanup_before_remove(&worktree_path).await;
                let repo = match Repository::open(&project_root).await {
                    Ok(r) => r,
                    Err(err) => {
                        tracing::warn!(?err, "delete_workspace: open repo failed");
                        let _ = weak.update(cx, |_, cx| {
                            crate::shell::toast::toast_op_error(
                                cx,
                                &format!("Delete workspace \u{201c}{}\u{201d}", workspace.slug),
                                &err.to_string(),
                            );
                        });
                        return;
                    }
                };
                let mut leftovers: Vec<String> = Vec::new();
                if let Err(err) = repo.remove_worktree(&worktree_path, force).await {
                    if !force {
                        // Preserve the row + branch for retry, surface the
                        // reason, and arm the Force Delete variant for the
                        // next attempt on this workspace.
                        tracing::warn!(?err, slug = %workspace.slug, "remove_worktree failed; workspace row + branch preserved for retry");
                        let _ = weak.update(cx, |this, cx| {
                            this.force_delete_offer = Some(workspace.id.clone());
                            crate::shell::toast::toast(
                                cx,
                                crate::shell::toast::ToastKind::Error,
                                format!(
                                    "Couldn't delete workspace \u{201c}{}\u{201d}: {} — choose Delete again to force-remove",
                                    workspace.slug,
                                    err.to_string().lines().next().unwrap_or("unknown error").trim(),
                                ),
                            );
                        });
                        return;
                    }
                    // Force path: keep going; report what stayed behind.
                    tracing::warn!(?err, slug = %workspace.slug, "force delete: remove_worktree still failed");
                    leftovers.push(format!("worktree at {}", workspace.worktree_path));
                }
                if let Err(err) = repo.delete_branch(&branch, force).await {
                    tracing::warn!(?err, branch = %branch, "delete_branch failed");
                    // Don't bail — DB cleanup still wanted to keep
                    // state in sync.
                    leftovers.push(format!("branch {branch}"));
                }
                let row_deleted = match workspace_repo.delete(&workspace.id) {
                    Ok(()) => true,
                    Err(err) => {
                        tracing::warn!(?err, workspace_id = %workspace.id, "delete row failed");
                        let _ = weak.update(cx, |_, cx| {
                            crate::shell::toast::toast_op_error(
                                cx,
                                &format!("Delete workspace \u{201c}{}\u{201d}", workspace.slug),
                                &err.to_string(),
                            );
                        });
                        false
                    }
                };
                let _ = weak.update(cx, |this, cx| {
                    this.force_delete_offer = None;
                    // Leftover report only when the workspace entry itself
                    // is gone — a failed row delete already toasted above,
                    // and "removed; not deleted: …" would contradict it.
                    if row_deleted && !leftovers.is_empty() {
                        crate::shell::toast::toast(
                            cx,
                            crate::shell::toast::ToastKind::Error,
                            format!(
                                "Workspace \u{201c}{}\u{201d} removed; not deleted: {} — clean up manually",
                                workspace.slug,
                                leftovers.join(", "),
                            ),
                        );
                    }
                    this.mark_rail_dirty(cx);
                });
            })
            .detach();
        });
        // Force delete `-D`'s the branch (real data loss), so it says so in
        // the body and on the button — the danger-styled button plus that
        // warning carries the weight.
        let prompt = if force {
            ConfirmPrompt {
                title: "Force delete workspace".into(),
                body: format!(
                    "The worktree at {} could not be removed normally. Force delete removes the workspace entry anyway and force-removes the worktree and branch {}; anything that still fails is reported and left on disk.",
                    workspace.worktree_path, workspace.branch
                )
                .into(),
                on_confirm,
                confirm_label: Some("Force Delete".into()),
                on_cancel: None,
                secondary: None,
            }
        } else {
            ConfirmPrompt {
                title: "Delete workspace".into(),
                body: format!(
                    "Removes the worktree at {} and deletes branch {}. This cannot be undone.",
                    workspace.worktree_path, workspace.branch
                )
                .into(),
                on_confirm,
                confirm_label: None,
                on_cancel: None,
                secondary: None,
            }
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));
        // Drop any per-mount observer the SCM discard path installed before
        // reusing the confirm_dialog slot, then watch THIS dialog so both
        // confirm AND cancel free the slot. The on-confirm callback drops the
        // dialog on its own path, but Cancel only flips the dialog's `cancelled`
        // flag — without this observer nothing would remove it from the overlay.
        self._discard_dialog_observer = None;
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));
        self.confirm_dialog = Some(dialog);
        cx.notify();
    }

    /// Open the project's root directory in the system file manager.
    /// Best-effort: a spawn failure is logged, never surfaced as a hard error,
    /// because "reveal" is a convenience action with no state to keep
    /// consistent.
    pub(crate) fn reveal_project_in_finder(&self, project: &Project) {
        #[cfg(target_os = "macos")]
        let launcher = "open";
        #[cfg(windows)]
        let launcher = "explorer";

        #[cfg(any(target_os = "macos", windows))]
        if let Err(err) = std::process::Command::new(launcher)
            .arg(&project.root_path)
            .spawn()
        {
            tracing::warn!(?err, path = %project.root_path, "reveal_project_in_finder: {launcher} failed");
        }
    }

    /// Copy the project's root path to the system clipboard.
    pub(crate) fn copy_project_path(&self, project: &Project, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(project.root_path.clone()));
        self.push_toast(
            crate::shell::toast::ToastKind::Success,
            "Copied project path",
            cx,
        );
    }

    /// Opt `project` in or out of the computer-use tools — the per-project half
    /// of the two switches that decide whether its agents get them.
    ///
    /// Offered from the project's own menu rather than the settings pane
    /// because this is where the user is looking at one specific project. The
    /// pane lists what is on and takes things off; choosing a path out of a
    /// global list is how the wrong repository gets enabled.
    ///
    /// Writes the TOML only. The settings watcher reparses and swaps the
    /// global, and calling `set_global` here would race its debouncer — the
    /// same contract the pane's own toggles keep.
    pub(crate) fn set_computer_use_for_project(
        &mut self,
        project: &Project,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        let mut settings = cx
            .try_global::<oximux_settings::ComputerUseSettings>()
            .cloned()
            .unwrap_or_default();
        let root = std::path::Path::new(&project.root_path);

        if on {
            settings.enable_project(root);
        } else if let Some(covering) = settings.covering_root(root)
            && covering != root
        {
            // Enabled by an ancestor the user opted in separately. Removing
            // this project's own path would take nothing away, so say where it
            // actually comes from rather than appearing to have done something.
            let covering = covering.display().to_string();
            self.push_toast(
                crate::shell::toast::ToastKind::Info,
                format!("Computer use here comes from {covering} — turn it off there"),
                cx,
            );
            return;
        } else {
            settings.disable_project(root);
        }

        if let Err(err) = crate::app_settings::computer_use_settings::save(&settings) {
            tracing::warn!(%err, "could not write computer_use.toml");
            self.push_toast(
                crate::shell::toast::ToastKind::Error,
                "Could not save the computer-use setting",
                cx,
            );
            return;
        }
        self.push_toast(
            crate::shell::toast::ToastKind::Success,
            if on {
                "Computer use on for this project"
            } else {
                "Computer use off for this project"
            },
            cx,
        );
    }

    /// Open the confirm dialog for removing a project from the
    /// cockpit. Removal is reversible (re-open the folder) and leaves
    /// every file on disk in place. On confirm: deletes
    /// the `projects` row — the `ON DELETE CASCADE` FK then drops the
    /// project's workspaces, pane buffers, and relay-id rows. If the removed
    /// project was active, the active selection is cleared.
    pub(crate) fn request_remove_project(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let project_repo = self.app_state.project_repo.clone();
        let project_id = project.id.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |_window, cx| {
            let project_repo = project_repo.clone();
            let project_id = project_id.clone();
            let weak = weak.clone();
            let _ = weak.update(cx, |this, cx| {
                if let Err(err) = project_repo.delete(&project_id) {
                    tracing::warn!(?err, project_id = %project_id, "remove_project: delete failed");
                    cx.notify();
                    return;
                }
                // Drop the in-memory panes + observer + cached sidebar for the
                // gone project so a stale entity can't keep rendering, saving,
                // or polling git in the background.
                this.project_panes_by_project.remove(&project_id);
                this.right_sidebar_by_project.remove(&project_id);
                if this.active_project.as_ref().map(|p| p.id.as_str()) == Some(project_id.as_str())
                {
                    this.active_project = None;
                    this.active_workspace_id = None;
                    this._project_panes_observer = None;
                    this.right_sidebar = None;
                }
                this.refresh_recent_projects();
                cx.notify();
            });
        });
        let prompt = ConfirmPrompt {
            title: "Remove Project".into(),
            body: format!(
                "Removes {} from OxiMux and forgets its workspaces. Files on disk are not deleted.",
                project.name
            )
            .into(),
            on_confirm,
            confirm_label: Some("Remove".into()),
            on_cancel: None,
            secondary: None,
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));
        // Drop the dialog the moment the user confirms or cancels. Reusing
        // `_discard_dialog_observer` cancels any stale observer first; the
        // SCM discard / workspace-delete paths share the same slot.
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));
        self.confirm_dialog = Some(dialog);
        cx.notify();
    }
}

#[cfg(test)]
mod nav_history_tests {
    use super::{
        WorkspaceNavRef, push_nav_entry, resolve_project_for_workspace, workspace_delete_target,
        workspace_path_for_ambient_terminal,
    };
    use oximux_core::{Project, Workspace};
    use std::collections::HashMap;

    fn r(id: &str) -> WorkspaceNavRef {
        WorkspaceNavRef {
            project_id: "p".to_string(),
            workspace_id: id.to_string(),
        }
    }

    fn workspace(id: &str, path: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: "p".to_string(),
            name: id.to_string(),
            slug: id.to_string(),
            branch: "main".to_string(),
            worktree_path: path.to_string(),
            status: "active".to_string(),
            created_at: "2026-06-24T00:00:00Z".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    fn project(id: &str, root: &str) -> Project {
        Project {
            id: id.to_string(),
            name: id.to_string(),
            root_path: root.to_string(),
            default_branch: "main".to_string(),
            created_at: String::new(),
            last_opened_at: None,
            sort_order: 0.0,
        }
    }

    fn workspace_in(project_id: &str, branch: &str) -> Workspace {
        Workspace {
            id: format!("ws-{project_id}"),
            project_id: project_id.to_string(),
            name: "w".to_string(),
            slug: "w".to_string(),
            branch: branch.to_string(),
            worktree_path: format!("/wt/{project_id}"),
            status: "active".to_string(),
            created_at: String::new(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    /// The rail renders every open project's rows at once, so a destructive row
    /// action can be reached while a DIFFERENT project is active. Resolution
    /// must follow the row, not the active project: otherwise deleting `api`'s
    /// row while `web` is active opens `web`'s repository and the force retry
    /// runs `git branch -D <api's branch>` inside `web`.
    ///
    /// This asserts on `workspace_delete_target` specifically because that is
    /// what `WorkspaceRoot::request_delete_workspace` calls. The handler is
    /// GPUI-bound and cannot be driven from here, so binding the test to the
    /// function it actually uses is what makes a revert to `self.active_project`
    /// fail rather than pass — that revert has to delete this function.
    #[test]
    fn delete_target_resolves_the_rows_own_project_not_the_active_one() {
        let api = project("api", "/repos/api");
        let web = project("web", "/repos/web");
        // `web` is first — an "active project" fallback would pick it.
        let open = vec![web.clone(), api.clone()];

        let row = workspace_in("api", "oximux/api-fix");
        let target = workspace_delete_target(&open, &row).expect("delete target");
        assert_eq!(
            target.project_root,
            std::path::PathBuf::from("/repos/api"),
            "the repository opened for this row must be its OWN project's"
        );
        assert_ne!(
            target.project_root,
            std::path::PathBuf::from(&web.root_path),
            "never the active project's"
        );
        // The branch that `git branch -D` would take on the force retry, and
        // the directory `remove_worktree` would take, both come from the row.
        assert_eq!(target.branch, "oximux/api-fix");
        assert_eq!(
            target.worktree_path,
            std::path::PathBuf::from("/wt/api")
        );
        assert_eq!(
            resolve_project_for_workspace(&open, &row).map(|p| p.id),
            Some(api.id)
        );
    }

    /// When the owning project is not open, the action must decline rather than
    /// silently fall through to another repository.
    #[test]
    fn unknown_project_yields_no_delete_target_rather_than_a_fallback() {
        let open = vec![project("web", "/repos/web")];
        let orphan = workspace_in("api", "oximux/api-fix");
        assert!(workspace_delete_target(&open, &orphan).is_none());
        assert!(resolve_project_for_workspace(&open, &orphan).is_none());
    }








    #[test]
    fn append_advances_cursor() {
        let mut h = Vec::new();
        let c = push_nav_entry(&mut h, 0, r("a"), 64);
        assert_eq!(c, 0);
        let c = push_nav_entry(&mut h, c, r("b"), 64);
        assert_eq!(c, 1);
        assert_eq!(h, vec![r("a"), r("b")]);
    }

    #[test]
    fn dedupes_current_entry() {
        let mut h = vec![r("a"), r("b")];
        let c = push_nav_entry(&mut h, 1, r("b"), 64);
        assert_eq!(c, 1);
        assert_eq!(h, vec![r("a"), r("b")]);
    }

    #[test]
    fn truncates_forward_on_new_activation() {
        // At cursor 0 of [a,b,c], activating d discards the forward b,c.
        let mut h = vec![r("a"), r("b"), r("c")];
        let c = push_nav_entry(&mut h, 0, r("d"), 64);
        assert_eq!(c, 1);
        assert_eq!(h, vec![r("a"), r("d")]);
    }

    #[test]
    fn caps_at_max_dropping_oldest() {
        let mut h = Vec::new();
        let mut c = 0;
        for i in 0..5 {
            c = push_nav_entry(&mut h, c, r(&i.to_string()), 3);
        }
        // Only the 3 newest survive; cursor pins to the last.
        assert_eq!(h, vec![r("2"), r("3"), r("4")]);
        assert_eq!(c, 2);
    }

    #[test]
    fn next_live_index_skips_stale_mid_history() {
        use super::next_live_index;
        // History [A, B(stale), C], cursor at C(2). Back must SKIP B and reach
        // A — not wall at B.
        let live = |i: usize| i != 1; // index 1 = B is deleted
        assert_eq!(next_live_index(3, 2, false, live), Some(0));
        // From A(0), forward skips B and reaches C(2).
        assert_eq!(next_live_index(3, 0, true, live), Some(2));
    }

    #[test]
    fn next_live_index_boundaries_and_all_stale() {
        use super::next_live_index;
        let all_live = |_: usize| true;
        assert_eq!(next_live_index(3, 0, false, all_live), None); // at oldest, back
        assert_eq!(next_live_index(3, 2, true, all_live), None); // at newest, forward
        assert_eq!(next_live_index(0, 0, false, all_live), None); // empty history
        // Everything ahead is stale → no live target, no move.
        let none_live = |_: usize| false;
        assert_eq!(next_live_index(3, 2, false, none_live), None);
    }

    #[test]
    fn ambient_terminal_path_resolves_to_deepest_workspace_root() {
        let workspaces = HashMap::from([(
            "p".to_string(),
            vec![
                workspace("main", "/repo"),
                workspace("feature", "/repo/worktrees/feature"),
            ],
        )]);

        assert_eq!(
            workspace_path_for_ambient_terminal("/repo/worktrees/feature/src", &workspaces),
            Some("/repo/worktrees/feature".to_string())
        );
        assert_eq!(
            workspace_path_for_ambient_terminal("/outside", &workspaces),
            None
        );
    }
}
