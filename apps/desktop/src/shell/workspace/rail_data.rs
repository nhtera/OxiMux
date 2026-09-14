//! The sidebar's DB-backed data, gathered in one background pass.
//!
//! Lifted out of `workspace_ops.rs`, which sits at the 3000-LOC hard cap
//! `xtask file-size-lint` enforces. Everything here runs on the background
//! executor — this is the ONLY place the rail touches SQLite — and returns
//! plain maps the root caches and `refresh_left_rail` reads.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use oximux_core::{AgentSession, Project, Workspace};
use oximux_storage::{AgentSessionRepo, ProjectRepo, WorkspaceRepo};

use crate::shell::left_rail::LatestStatusMap;

/// Body of [`WorkspaceRoot::workspaces_with_primary`] as a free function so
/// the rail gather can run it on the background executor (SQLite + a
/// `.git` stat — never call from a render path).
pub(crate) fn workspaces_with_primary_for(repo: &WorkspaceRepo, project: &Project) -> Vec<Workspace> {
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
                // Not a branch OxiMux minted: a synthesized row or a
                // fixture. `false` is the reading that never deletes.
                branch_minted: false,
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
pub(crate) type RailDbData = (
    HashMap<String, Vec<Workspace>>,
    HashMap<String, Vec<Workspace>>,
    LatestStatusMap,
    HashMap<String, String>,
    HashMap<String, String>,
    HashMap<String, Vec<AgentSession>>,
    HashSet<String>,
);

pub(crate) fn gather_rail_db_data(
    workspace_repo: &WorkspaceRepo,
    agent_repo: &AgentSessionRepo,
    project_repo: &ProjectRepo,
    projects: &[Project],
) -> RailDbData {
    // Which projects hide their untracked-worktrees group. One indexed point
    // read per project, beside the archived query below.
    let mut hidden_untracked: HashSet<String> = HashSet::new();
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
        if project_repo.hide_untracked(&project.id).unwrap_or(false) {
            hidden_untracked.insert(project.id.clone());
        }
    }
    (
        workspaces_by_project,
        archived_by_project,
        latest_status,
        latest_adapter,
        last_active,
        workspace_sessions,
        hidden_untracked,
    )
}
