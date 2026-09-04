//! Row assembly for the agents dashboard.
//!
//! `build_agent_rows` turns the rail's per-session `WorkspaceAgentList` into a
//! sorted `Vec<AgentRow>` (one row per agent session), reusing the rail's
//! shared title/activity/done helpers so a card matches a rail sub-row.
//! `widest_row_index` picks the widest row for the `uniform_list` horizontal
//! measurement. Both are pure + testable without a window.

use std::collections::HashMap;

use oximux_core::{Project, Workspace};

use crate::shell::agent_presentation::adapter_icon_path;
use crate::shell::agents_dashboard::model::{AgentRow, sort_agent_rows};
use crate::shell::left_rail::WorkspaceAgentList;
use crate::shell::left_rail::workspace_agent_rows::{
    collapse_ws, is_completed_turn, live_activity, live_prompt,
};
use crate::shell::session_merge::relative_age_long;

/// Build the dashboard row list — **one row per agent session** — from the
/// per-session `WorkspaceAgentList` the rail already assembles (live + recent
/// history + ambient, merged and capped). A workspace with N agents yields N
/// rows, matching the reference cockpit's per-session cards.
///
/// `projects` + `workspaces_by_project` resolve each row's `workspace_key`
/// (`workspaces.id` or `primary:<project_id>`) back to its `Workspace` and
/// project name for the repo/branch badge; a row whose workspace has vanished
/// (a refresh race) is skipped. The prompt/activity/done fields reuse the
/// rail's shared helpers so the card reads identically. Calls `sort_agent_rows`
/// before returning so callers get an attention-sorted list.
pub fn build_agent_rows(
    workspace_agents: &WorkspaceAgentList,
    projects: &[Project],
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    now: &str,
) -> Vec<AgentRow> {
    // workspace_key → (Workspace, project name). Built once before the loop so
    // each session resolves its badge in O(1) without rescanning projects.
    let mut lookup: HashMap<&str, (&Workspace, &str)> = HashMap::new();
    for project in projects {
        if let Some(workspaces) = workspaces_by_project.get(&project.id) {
            for workspace in workspaces {
                lookup.insert(workspace.id.as_str(), (workspace, project.name.as_str()));
            }
        }
    }

    let mut rows: Vec<AgentRow> = Vec::new();
    for (workspace_key, rail_rows) in workspace_agents {
        // Skip rows whose workspace is no longer in the snapshot (refresh race).
        let Some((workspace, project_name)) = lookup.get(workspace_key.as_str()).copied() else {
            continue;
        };
        for rail in rail_rows {
            // Reuse the rail's shared helpers so a card matches a sub-row: the
            // prompt is the title, the tool/reply is the secondary, and an
            // idle-with-reply row reads as "done". Read once here (a snapshot),
            // not in the render closure.
            let primary = live_prompt(rail).map(|p| collapse_ws(&p));
            let secondary = live_activity(rail).map(|s| collapse_ws(&s));
            // Recency key: terminal `ended_at` else `started_at`.
            let last_active_at = rail.ended_at.clone().or_else(|| rail.started_at.clone());
            // Long-form age ("14 hours ago") for the wider dashboard card; the
            // rail keeps its compact `age_label` for the narrow sub-rows.
            let age_label = last_active_at
                .as_deref()
                .map(|ts| relative_age_long(ts, now))
                .unwrap_or_default();
            rows.push(AgentRow {
                project_name: project_name.to_string(),
                workspace: workspace.clone(),
                db_id: rail.db_id.clone(),
                target: rail.target.clone(),
                age_label,
                status: Some(rail.effective_status()),
                is_live: rail.is_live,
                icon_path: adapter_icon_path(&rail.adapter_id),
                last_active_at,
                label: rail.label.to_string(),
                primary,
                secondary,
                completed_turn: is_completed_turn(rail),
            });
        }
    }

    sort_agent_rows(&mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::left_rail::{RailAgentRow, RailAgentTarget};
    use oximux_core::{AgentSnapshot, AgentStatus, SidebandDetail};
    use tokio::sync::watch;

    // Fixed clock: 4 days after every fixture's `started_at` default.
    const NOW: &str = "2026-06-24T00:00:00+00:00";

    fn project(id: &str, name: &str) -> Project {
        Project {
            id: id.to_string(),
            name: name.to_string(),
            root_path: format!("/tmp/{id}"),
            default_branch: "main".to_string(),
            created_at: "2026-06-01T00:00:00Z".to_string(),
            last_opened_at: None,
            sort_order: 0.0,
        }
    }

    fn workspace(id: &str, project_id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: project_id.to_string(),
            name: format!("ws-{id}"),
            slug: id.to_string(),
            branch: format!("oximux/{id}"),
            worktree_path: format!("/tmp/{project_id}/{id}"),
            status: "active".to_string(),
            created_at: "2026-06-01T00:00:00Z".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    /// A non-live history row (no status channel) carrying its DB status.
    fn rail(db_id: &str, ws_key: &str, adapter: &str, status: AgentStatus) -> RailAgentRow {
        RailAgentRow {
            db_id: db_id.to_string(),
            target: RailAgentTarget::AgentSession {
                db_id: db_id.to_string(),
            },
            workspace_key: ws_key.to_string(),
            adapter_id: adapter.to_string(),
            label: "Claude Code".into(),
            is_live: false,
            status_rx: None,
            db_status: status,
            started_at: Some("2026-06-20T00:00:00+00:00".to_string()),
            ended_at: None,
            age_label: "now".to_string(),
            persisted_title: None,
            persisted_message: None,
            ambient_detail: None,
        }
    }

    /// A live row whose status channel carries `status` + optional `detail`.
    fn live_rail(
        db_id: &str,
        ws_key: &str,
        status: AgentStatus,
        detail: Option<SidebandDetail>,
    ) -> RailAgentRow {
        let (_tx, rx) = watch::channel(AgentSnapshot {
            status: status.clone(),
            detail,
        });
        RailAgentRow {
            is_live: true,
            status_rx: Some(rx),
            ..rail(db_id, ws_key, "claude-code", status)
        }
    }

    fn one_project(wss: Vec<Workspace>) -> (Vec<Project>, HashMap<String, Vec<Workspace>>) {
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), wss);
        (vec![project("p1", "Proj")], wbp)
    }

    fn list(pairs: Vec<(&str, Vec<RailAgentRow>)>) -> WorkspaceAgentList {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn one_row_per_session() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let wa = list(vec![(
            "a",
            vec![
                rail("s1", "a", "claude-code", AgentStatus::Running),
                rail("s2", "a", "claude-code", AgentStatus::Idle),
            ],
        )]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert_eq!(rows.len(), 2);
        let ids: Vec<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        assert!(ids.contains(&"s1") && ids.contains(&"s2"));
    }

    #[test]
    fn resolves_project_name_and_workspace_for_badge() {
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![workspace("a", "p1")]);
        let projects = vec![project("p1", "MyProject")];
        let wa = list(vec![("a", vec![rail("s1", "a", "claude-code", AgentStatus::Idle)])]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert_eq!(rows[0].project_name, "MyProject");
        assert_eq!(rows[0].workspace.branch, "oximux/a");
        // No prompt captured → primary falls back to the adapter label.
        assert!(rows[0].primary.is_none());
        assert_eq!(rows[0].label, "Claude Code");
    }

    #[test]
    fn unknown_workspace_key_is_skipped() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let wa = list(vec![(
            "ghost",
            vec![rail("s1", "ghost", "claude-code", AgentStatus::Running)],
        )]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert!(rows.is_empty());
    }

    #[test]
    fn primary_is_prompt_secondary_is_activity() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let detail = SidebandDetail {
            prompt: Some("fix the parser".into()),
            tool_name: Some("Edit".into()),
            tool_input_summary: Some("src/lib.rs".into()),
            ..Default::default()
        };
        let wa = list(vec![(
            "a",
            vec![live_rail("s1", "a", AgentStatus::Running, Some(detail))],
        )]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert_eq!(rows[0].primary.as_deref(), Some("fix the parser"));
        assert_eq!(rows[0].secondary.as_deref(), Some("Edit: src/lib.rs"));
        assert!(!rows[0].completed_turn);
    }

    #[test]
    fn completed_turn_set_for_idle_with_reply() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let detail = SidebandDetail {
            prompt: Some("hi".into()),
            last_message: Some("All set — tests pass.".into()),
            ..Default::default()
        };
        let wa = list(vec![(
            "a",
            vec![live_rail("s1", "a", AgentStatus::Idle, Some(detail))],
        )]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert!(rows[0].completed_turn);
        assert_eq!(rows[0].primary.as_deref(), Some("hi"));
        assert_eq!(rows[0].secondary.as_deref(), Some("All set — tests pass."));
    }

    #[test]
    fn rows_are_sorted_on_return() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let wa = list(vec![(
            "a",
            vec![
                rail("done", "a", "claude-code", AgentStatus::Done { code: Some(0) }),
                rail("wait", "a", "claude-code", AgentStatus::WaitingForInput),
            ],
        )]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert_eq!(rows[0].db_id, "wait");
        assert_eq!(rows[1].db_id, "done");
    }

    #[test]
    fn icon_resolved_from_adapter_with_fallback() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let wa = list(vec![(
            "a",
            vec![
                rail("branded", "a", "claude-code", AgentStatus::Running),
                rail("bare", "a", "", AgentStatus::Running),
            ],
        )]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        let branded = rows.iter().find(|r| r.db_id == "branded").unwrap();
        let bare = rows.iter().find(|r| r.db_id == "bare").unwrap();
        assert_eq!(branded.icon_path, "icons/claude-code.svg");
        assert_eq!(bare.icon_path, "icons/sparkles.svg");
    }

    #[test]
    fn last_active_at_prefers_ended_then_started() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        let mut ended = rail("ended", "a", "claude-code", AgentStatus::Done { code: Some(0) });
        ended.ended_at = Some("2026-06-22T00:00:00+00:00".to_string());
        let started = rail("started", "a", "claude-code", AgentStatus::Running);
        let wa = list(vec![("a", vec![ended, started])]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        let e = rows.iter().find(|r| r.db_id == "ended").unwrap();
        let s = rows.iter().find(|r| r.db_id == "started").unwrap();
        assert_eq!(e.last_active_at.as_deref(), Some("2026-06-22T00:00:00+00:00"));
        assert_eq!(s.last_active_at.as_deref(), Some("2026-06-20T00:00:00+00:00"));
    }

    #[test]
    fn target_carried_and_age_is_long_form() {
        let (projects, wbp) = one_project(vec![workspace("a", "p1")]);
        // rail() defaults started_at to 2026-06-20; NOW is 4 days later.
        let r = rail("s1", "a", "claude-code", AgentStatus::Idle);
        let wa = list(vec![("a", vec![r])]);
        let rows = build_agent_rows(&wa, &projects, &wbp, NOW);
        assert_eq!(rows[0].age_label, "4 days ago");
        assert_eq!(
            rows[0].target,
            RailAgentTarget::AgentSession {
                db_id: "s1".to_string()
            }
        );
    }

}
