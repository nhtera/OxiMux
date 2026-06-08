//! Row types — direct mirror of V001 schema columns. Each `from_row`
//! reads positional columns matching the `SELECT … FROM …` clauses used
//! by repositories; the `From<XxxRow> for Xxx` conversions translate to
//! the `oximux-core` domain types that callers see.
//!
//! Living storage-side keeps `oximux-core` from learning about
//! `rusqlite::Row`; storage already depends on core, so the conversion
//! direction is the natural one.

use oximux_core::{AgentSession, AgentStatus, PaneSession, Project, Workspace, WorktreeSettings};
use rusqlite::Row;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub default_branch: String,
    pub created_at: String,
    pub last_opened_at: Option<String>,
}

impl ProjectRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            root_path: row.get("root_path")?,
            default_branch: row.get("default_branch")?,
            created_at: row.get("created_at")?,
            last_opened_at: row.get("last_opened_at")?,
        })
    }
}

impl From<ProjectRow> for Project {
    fn from(r: ProjectRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            root_path: r.root_path,
            default_branch: r.default_branch,
            created_at: r.created_at,
            last_opened_at: r.last_opened_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub slug: String,
    pub branch: String,
    pub worktree_path: String,
    pub status: String,
    pub created_at: String,
    pub archived_at: Option<String>,
    pub linked_issue: Option<String>,
    pub tint: Option<String>,
}

impl WorkspaceRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            name: row.get("name")?,
            slug: row.get("slug")?,
            branch: row.get("branch")?,
            worktree_path: row.get("worktree_path")?,
            status: row.get("status")?,
            created_at: row.get("created_at")?,
            archived_at: row.get("archived_at")?,
            linked_issue: row.get("linked_issue")?,
            tint: row.get("tint")?,
        })
    }
}

impl From<WorkspaceRow> for Workspace {
    fn from(r: WorkspaceRow) -> Self {
        Self {
            id: r.id,
            project_id: r.project_id,
            name: r.name,
            slug: r.slug,
            branch: r.branch,
            worktree_path: r.worktree_path,
            status: r.status,
            created_at: r.created_at,
            archived_at: r.archived_at,
            linked_issue: r.linked_issue,
            tint: r.tint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRow {
    pub id: String,
    pub workspace_id: String,
    pub adapter_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub status_detail: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

impl AgentSessionRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            workspace_id: row.get("workspace_id")?,
            adapter_id: row.get("adapter_id")?,
            model: row.get("model")?,
            effort: row.get("effort")?,
            status: row.get("status")?,
            exit_code: row.get("exit_code")?,
            status_detail: row.get("status_detail")?,
            started_at: row.get("started_at")?,
            ended_at: row.get("ended_at")?,
        })
    }
}

impl From<AgentSessionRow> for AgentSession {
    fn from(r: AgentSessionRow) -> Self {
        // Unknown status slug degrades to Interrupted rather than panicking —
        // a forward-compat hedge if a future binary writes a variant this
        // build does not understand.
        let status = AgentStatus::from_row(&r.status, r.exit_code, r.status_detail)
            .unwrap_or(AgentStatus::Interrupted);
        Self {
            id: r.id,
            workspace_id: r.workspace_id,
            adapter_id: r.adapter_id,
            model: r.model,
            effort: r.effort,
            status,
            started_at: r.started_at,
            ended_at: r.ended_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSessionRow {
    pub id: String,
    pub workspace_id: String,
    pub agent_session_id: Option<String>,
    pub shell_command: String,
    pub grid_position: String,
    pub log_path: Option<String>,
    pub created_at: String,
}

impl PaneSessionRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            workspace_id: row.get("workspace_id")?,
            agent_session_id: row.get("agent_session_id")?,
            shell_command: row.get("shell_command")?,
            grid_position: row.get("grid_position")?,
            log_path: row.get("log_path")?,
            created_at: row.get("created_at")?,
        })
    }
}

impl From<PaneSessionRow> for PaneSession {
    fn from(r: PaneSessionRow) -> Self {
        Self {
            id: r.id,
            workspace_id: r.workspace_id,
            agent_session_id: r.agent_session_id,
            shell_command: r.shell_command,
            grid_position: r.grid_position,
            log_path: r.log_path,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSettingsRow {
    pub workspace_id: String,
    pub base_ref: Option<String>,
    pub commit_draft: Option<String>,
    pub view_mode_override: Option<String>,
    pub updated_at: String,
}

impl WorktreeSettingsRow {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            workspace_id: row.get("workspace_id")?,
            base_ref: row.get("base_ref")?,
            commit_draft: row.get("commit_draft")?,
            view_mode_override: row.get("view_mode_override")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

impl From<WorktreeSettingsRow> for WorktreeSettings {
    fn from(r: WorktreeSettingsRow) -> Self {
        Self {
            base_ref: r.base_ref,
            commit_draft: r.commit_draft,
            view_mode_override: r.view_mode_override,
        }
    }
}
