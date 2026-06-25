//! Row assembly for the agents dashboard.
//!
//! `build_agent_rows` turns `LeftRail`'s pushed-down snapshot maps into a
//! sorted `Vec<AgentRow>`; `widest_row_index` picks the widest row for the
//! `uniform_list` horizontal-scroll measurement. Both are pure + testable
//! without a window. Split out of `model.rs` to keep that file focused on the
//! `AgentRow` model + ranking.

use std::collections::{HashMap, HashSet};

use oximux_core::{AgentStatus, Project, SidebandDetail, Workspace};
use oximux_settings::Theme;

use crate::shell::agent_presentation::{adapter_icon_path, agent_verb};
use crate::shell::agents_dashboard::model::{AgentRow, sideband_subline, sort_agent_rows};
use crate::shell::left_rail::LatestStatusMap;
use crate::shell::left_rail::workspace_row::DiffCounts;

/// Build the dashboard row list from `LeftRail`'s pushed-down snapshot maps.
///
/// One row per workspace that has an agent (status is `Some` OR `is_live`).
/// Dormant workspaces (neither status nor live) are skipped. `latest_adapter`
/// (workspace id → adapter slug) drives the per-row icon; `last_active`
/// (workspace id → RFC-3339 timestamp) drives the recency tiebreaker. Calls
/// `sort_agent_rows` before returning so callers get an attention-sorted list.
#[allow(clippy::too_many_arguments)]
pub fn build_agent_rows(
    projects: &[Project],
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    latest_status: &LatestStatusMap,
    live_worktrees: &HashSet<String>,
    diff_counts: &HashMap<String, DiffCounts>,
    agent_activity: &HashMap<String, String>,
    agent_sideband: &HashMap<String, SidebandDetail>,
    latest_adapter: &HashMap<String, String>,
    last_active: &HashMap<String, String>,
    theme: Theme,
) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = Vec::new();

    for project in projects {
        let workspaces = match workspaces_by_project.get(&project.id) {
            Some(ws) => ws,
            None => continue,
        };

        for workspace in workspaces {
            let status: Option<AgentStatus> = latest_status.get(&workspace.id).cloned().flatten();
            let is_live = live_worktrees.contains(&workspace.worktree_path);

            // Skip dormant workspaces — no status and no live tab.
            if status.is_none() && !is_live {
                continue;
            }

            let verb = agent_verb(status.as_ref(), is_live, theme);
            let diff = diff_counts.get(&workspace.worktree_path).cloned();
            // Activity is only meaningful while Running — a stale "Bash: …"
            // on a Done row would read as still-working.
            let activity = matches!(status, Some(AgentStatus::Running))
                .then(|| agent_activity.get(&workspace.id).cloned())
                .flatten();
            // Live sideband tool step — same Running gate as the activity tail
            // it supersedes on the card. The watcher already clears the map
            // entry off Running, so this gate is belt-and-suspenders against a
            // momentarily-stale DB status.
            let sideband_detail = matches!(status, Some(AgentStatus::Running))
                .then(|| agent_sideband.get(&workspace.id).cloned())
                .flatten();
            // Resolve the brand glyph once here, not in the render closure.
            // Unknown / absent adapter → sparkles fallback (handled by
            // `adapter_icon_path`).
            let icon_path = adapter_icon_path(
                latest_adapter
                    .get(&workspace.id)
                    .map(String::as_str)
                    .unwrap_or(""),
            );
            let last_active_at = last_active.get(&workspace.id).cloned();

            rows.push(AgentRow {
                project_name: project.name.clone(),
                workspace: workspace.clone(),
                verb,
                diff,
                status,
                is_live,
                activity,
                icon_path,
                last_active_at,
                sideband_detail,
            });
        }
    }

    sort_agent_rows(&mut rows);
    rows
}

/// Index of the row with the widest estimated label. Feeds `uniform_list`'s
/// `with_width_from_item` so long rows scroll horizontally instead of clipping
/// in the narrow rail. Pure character-count proxy — no GPUI measurement.
pub fn widest_row_index(rows: &[AgentRow]) -> Option<usize> {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, r)| estimated_row_width(r))
        .map(|(i, _)| i)
}

/// Rough rendered-width proxy for a row, in characters. Exact pixel width is
/// irrelevant — only the relative ordering matters for picking the widest row.
fn estimated_row_width(r: &AgentRow) -> usize {
    let branch_len = if r.workspace.branch.is_empty() {
        r.workspace.slug.len()
    } else {
        r.workspace.branch.len()
    };
    // Fixed budget for the diff chip when present (`+NN −NN`).
    let diff_len = if r.diff.is_some() { 8 } else { 0 };
    // Line 2 shows the sideband subline when present, else the activity tail —
    // mirror that precedence so the widest-row measurement matches the paint.
    let subline_len = r
        .sideband_detail
        .as_ref()
        .and_then(sideband_subline)
        .map(|s| s.chars().count())
        .or_else(|| r.activity.as_ref().map(|a| a.len()))
        .map_or(0, |n| n + 3);
    r.project_name.len()
        + r.workspace.name.len()
        + branch_len
        + r.verb.label.len()
        + diff_len
        + subline_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::charcoal()
    }

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
        }
    }

    /// `build_agent_rows` with the two new maps defaulted empty, for the tests
    /// that don't exercise icon/recency.
    #[allow(clippy::too_many_arguments)]
    fn build(
        projects: &[Project],
        wbp: &HashMap<String, Vec<Workspace>>,
        latest_status: &LatestStatusMap,
        live: &HashSet<String>,
        diff: &HashMap<String, DiffCounts>,
        activity: &HashMap<String, String>,
    ) -> Vec<AgentRow> {
        build_agent_rows(
            projects,
            wbp,
            latest_status,
            live,
            diff,
            activity,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            t(),
        )
    }

    #[test]
    fn dormant_workspaces_are_skipped() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build(
            &[p],
            &wbp,
            &HashMap::new(), // no status
            &HashSet::new(), // not live
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(rows.is_empty(), "dormant workspace must be skipped");
    }

    #[test]
    fn live_workspace_without_status_is_included() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let worktree_path = ws.worktree_path.clone();
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let mut live = HashSet::new();
        live.insert(worktree_path);
        let rows = build(
            &[p],
            &wbp,
            &HashMap::new(),
            &live,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].workspace.id, "a");
    }

    #[test]
    fn status_bearing_workspace_is_included() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("a".to_string(), Some(AgentStatus::Running));
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].workspace.id, "a");
    }

    #[test]
    fn diff_counts_are_carried_through() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let worktree_path = ws.worktree_path.clone();
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let mut live = HashSet::new();
        live.insert(worktree_path.clone());
        let mut diff_map = HashMap::new();
        diff_map.insert(
            worktree_path,
            DiffCounts {
                added: 5,
                removed: 3,
            },
        );
        let rows = build(
            &[p],
            &wbp,
            &HashMap::new(),
            &live,
            &diff_map,
            &HashMap::new(),
        );
        let diff = rows[0].diff.as_ref().expect("diff must be carried");
        assert_eq!(diff.added, 5);
        assert_eq!(diff.removed, 3);
    }

    #[test]
    fn verb_is_resolved_via_agent_presentation() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("a".to_string(), Some(AgentStatus::WaitingForInput));
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(rows[0].verb.label, "Waiting for input");
        assert_eq!(rows[0].verb.color, t().status_warn);
    }

    #[test]
    fn rows_are_sorted_on_return() {
        let p = project("p1", "Proj");
        let wa = workspace("a", "p1");
        let wb = workspace("b", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        // 'a' = Done (tier 3), 'b' = WaitingForInput (tier 0)
        status_map.insert("a".to_string(), Some(AgentStatus::Done { code: Some(0) }));
        status_map.insert("b".to_string(), Some(AgentStatus::WaitingForInput));
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![wa, wb]);
        let rows = build(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(rows[0].workspace.id, "b");
        assert_eq!(rows[1].workspace.id, "a");
    }

    #[test]
    fn activity_attached_to_running_rows_only() {
        let p = project("p1", "Proj");
        let run = workspace("run", "p1");
        let done = workspace("done", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("run".to_string(), Some(AgentStatus::Running));
        status_map.insert(
            "done".to_string(),
            Some(AgentStatus::Done { code: Some(0) }),
        );
        let mut activity = HashMap::new();
        activity.insert("run".to_string(), "Bash: cargo test".to_string());
        // Stale entry for a finished session must NOT surface.
        activity.insert("done".to_string(), "Bash: old".to_string());
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![run, done]);
        let rows = build(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &activity,
        );
        let running = rows.iter().find(|r| r.workspace.id == "run").unwrap();
        let finished = rows.iter().find(|r| r.workspace.id == "done").unwrap();
        assert_eq!(running.activity.as_deref(), Some("Bash: cargo test"));
        assert!(finished.activity.is_none());
    }

    #[test]
    fn sideband_detail_attached_to_running_rows_only() {
        // The live tool subline rides only Running rows; a stale entry on a
        // finished session must not surface (same gate as the activity tail).
        let p = project("p1", "Proj");
        let run = workspace("run", "p1");
        let done = workspace("done", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("run".to_string(), Some(AgentStatus::Running));
        status_map.insert(
            "done".to_string(),
            Some(AgentStatus::Done { code: Some(0) }),
        );
        let mut sideband = HashMap::new();
        let edit_detail = SidebandDetail {
            tool_name: Some("Edit".to_string()),
            tool_input_summary: Some("src/lib.rs".to_string()),
            last_message: None,
            session_id: None,
            ..Default::default()
        };
        sideband.insert("run".to_string(), edit_detail.clone());
        sideband.insert("done".to_string(), edit_detail);
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![run, done]);
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &sideband,
            &HashMap::new(),
            &HashMap::new(),
            t(),
        );
        let running = rows.iter().find(|r| r.workspace.id == "run").unwrap();
        let finished = rows.iter().find(|r| r.workspace.id == "done").unwrap();
        assert_eq!(
            running
                .sideband_detail
                .as_ref()
                .and_then(|d| d.tool_name.as_deref()),
            Some("Edit")
        );
        assert!(finished.sideband_detail.is_none());
    }

    #[test]
    fn build_agent_rows_populates_icon_path() {
        // SC3: a claude-code adapter row resolves the branded glyph; a
        // workspace with no adapter entry falls back to sparkles.
        let p = project("p1", "Proj");
        let branded = workspace("branded", "p1");
        let bare = workspace("bare", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("branded".to_string(), Some(AgentStatus::Running));
        status_map.insert("bare".to_string(), Some(AgentStatus::Running));
        let mut adapters = HashMap::new();
        adapters.insert("branded".to_string(), "claude-code".to_string());
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![branded, bare]);
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &adapters,
            &HashMap::new(),
            t(),
        );
        let branded_row = rows.iter().find(|r| r.workspace.id == "branded").unwrap();
        let bare_row = rows.iter().find(|r| r.workspace.id == "bare").unwrap();
        assert_eq!(branded_row.icon_path, "icons/claude-code.svg");
        assert!(!branded_row.icon_path.is_empty());
        assert_eq!(bare_row.icon_path, "icons/sparkles.svg");
    }

    #[test]
    fn build_agent_rows_populates_last_active_at() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("a".to_string(), Some(AgentStatus::Running));
        let mut last_active = HashMap::new();
        last_active.insert("a".to_string(), "2026-06-21T08:00:00+00:00".to_string());
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &last_active,
            t(),
        );
        assert_eq!(
            rows[0].last_active_at.as_deref(),
            Some("2026-06-21T08:00:00+00:00")
        );
    }

    #[test]
    fn widest_row_index_empty_is_none() {
        assert_eq!(widest_row_index(&[]), None);
    }

    #[test]
    fn widest_row_index_picks_longest_label() {
        let p = project("p1", "Proj");
        let short = workspace("a", "p1");
        let long = workspace("a-much-longer-workspace-identifier-for-width", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("a".to_string(), Some(AgentStatus::Running));
        status_map.insert(
            "a-much-longer-workspace-identifier-for-width".to_string(),
            Some(AgentStatus::Running),
        );
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![short, long]);
        let rows = build(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        // The longer workspace id yields the widest estimated width.
        let widest = widest_row_index(&rows).expect("non-empty");
        assert!(
            rows[widest]
                .workspace
                .id
                .contains("much-longer-workspace-identifier")
        );
    }

    #[test]
    fn project_name_is_carried_through() {
        let p = project("p1", "MyProject");
        let ws = workspace("a", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("a".to_string(), Some(AgentStatus::Running));
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(rows[0].project_name, "MyProject");
    }
}
