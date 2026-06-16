//! Pure data layer for the agents dashboard — no GPUI runtime required.
//!
//! Provides `AgentRow` (one row per live/status-bearing workspace across all
//! projects), `attention_rank` (priority tier for sorting), `sort_agent_rows`
//! (stable sort by tier), and `build_agent_rows` (assembles the list from the
//! snapshots already held by `LeftRail`).
//!
//! All functions are pure + testable without a window.

use std::collections::{HashMap, HashSet};

use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::Theme;

use crate::shell::agent_presentation::{AgentVerb, agent_verb};
use crate::shell::left_rail::LatestStatusMap;
use crate::shell::left_rail::workspace_row::DiffCounts;

/// One row in the agents dashboard. Carries enough data to render the row
/// and to activate the workspace on click — avoids re-querying any map
/// inside the `uniform_list` closure.
#[derive(Debug, Clone)]
pub struct AgentRow {
    /// Human-readable project name for the first-column label.
    pub project_name: String,
    /// The workspace this row represents. Its `id` and `worktree_path` drive
    /// the click handler; its `name`/`slug`/`branch` drive the row label.
    pub workspace: Workspace,
    /// Resolved verb label + color (reused from `agent_presentation`).
    pub verb: AgentVerb,
    /// Working-tree diff counts. `None` when not yet cached or unavailable.
    pub diff: Option<DiffCounts>,
    /// Latest agent-session status. `None` for live-but-never-started workspaces.
    pub status: Option<AgentStatus>,
    /// Whether the workspace currently has an open agent tab (green idle dot).
    pub is_live: bool,
    /// Live tool-activity line for Running sessions ("Bash: cargo test…"),
    /// tailed from the agent CLI's session log on a background tick. `None`
    /// when idle, non-Running, or the log has no fresh tool call.
    pub activity: Option<String>,
}

/// True for states a user should come look at: the session is blocked on
/// them (approval / input) or has stopped (done / failed / interrupted) —
/// the boolean companion of `attention_rank`'s tiers 0/3/4. Drives the
/// unread badge on the Agents nav row.
pub fn needs_attention(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::NeedsApproval(_)
            | AgentStatus::WaitingForInput
            | AgentStatus::Done { .. }
            | AgentStatus::Failed(_)
            | AgentStatus::Interrupted
    )
}

/// Attention tier for sorting. Lower = higher priority (floats to top).
///
/// Tiers:
///   0 — NeedsApproval / WaitingForInput  (user action required)
///   1 — Running                           (active work in progress)
///   2 — Idle / live without status        (ready, no action needed)
///   3 — Done (exit 0)                     (clean completion)
///   4 — Failed / Interrupted / Done (!0)  (terminal errors)
pub fn attention_rank(status: Option<&AgentStatus>, is_live: bool) -> u8 {
    match status {
        Some(AgentStatus::NeedsApproval(_)) | Some(AgentStatus::WaitingForInput) => 0,
        Some(AgentStatus::Running) => 1,
        // No status but live: treat same as Idle/live (tier 2).
        None if is_live => 2,
        Some(AgentStatus::Idle) => 2,
        Some(AgentStatus::Done { code: Some(0) }) => 3,
        // Non-zero exit, unknown exit, Failed, Interrupted, dormant.
        Some(AgentStatus::Done { .. })
        | Some(AgentStatus::Failed(_))
        | Some(AgentStatus::Interrupted) => 4,
        // Dormant workspace with no status and not live would be filtered
        // before reaching here, but map to lowest priority defensively.
        None => 4,
    }
}

/// Stable sort `rows` by `attention_rank`. Rows within a tier preserve their
/// input order (`sort_by_key` is stable in Rust).
pub fn sort_agent_rows(rows: &mut Vec<AgentRow>) {
    rows.sort_by_key(|r| attention_rank(r.status.as_ref(), r.is_live));
}

/// Build the dashboard row list from `LeftRail`'s pushed-down snapshot fields.
///
/// One row per workspace that has an agent (status is `Some` OR `is_live`).
/// Dormant workspaces (neither status nor live) are skipped. Calls
/// `sort_agent_rows` before returning so callers get an attention-sorted list.
pub fn build_agent_rows(
    projects: &[Project],
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    latest_status: &LatestStatusMap,
    live_worktrees: &HashSet<String>,
    diff_counts: &HashMap<String, DiffCounts>,
    agent_activity: &HashMap<String, String>,
    theme: Theme,
) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = Vec::new();

    for project in projects {
        let workspaces = match workspaces_by_project.get(&project.id) {
            Some(ws) => ws,
            None => continue,
        };

        for workspace in workspaces {
            let status: Option<AgentStatus> = latest_status
                .get(&workspace.id)
                .cloned()
                .flatten();
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

            rows.push(AgentRow {
                project_name: project.name.clone(),
                workspace: workspace.clone(),
                verb,
                diff,
                status,
                is_live,
                activity,
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
    let activity_len = r.activity.as_ref().map_or(0, |a| a.len() + 3);
    r.project_name.len()
        + r.workspace.name.len()
        + branch_len
        + r.verb.label.len()
        + diff_len
        + activity_len
}

// ─── unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Theme {
        Theme::charcoal()
    }

    #[test]
    fn needs_attention_matches_blocked_and_terminal_states() {
        // Blocked-on-user and stopped states badge; in-flight states don't.
        assert!(needs_attention(&AgentStatus::NeedsApproval("x".into())));
        assert!(needs_attention(&AgentStatus::WaitingForInput));
        assert!(needs_attention(&AgentStatus::Done { code: Some(0) }));
        assert!(needs_attention(&AgentStatus::Done { code: None }));
        assert!(needs_attention(&AgentStatus::Failed("boom".into())));
        assert!(needs_attention(&AgentStatus::Interrupted));
        assert!(!needs_attention(&AgentStatus::Running));
        assert!(!needs_attention(&AgentStatus::Idle));
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

    // ── attention_rank tiers ──────────────────────────────────────────────────

    #[test]
    fn tier_zero_needs_approval() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::NeedsApproval("x".into())), false),
            0
        );
    }

    #[test]
    fn tier_zero_waiting_for_input() {
        assert_eq!(attention_rank(Some(&AgentStatus::WaitingForInput), false), 0);
    }

    #[test]
    fn tier_one_running() {
        assert_eq!(attention_rank(Some(&AgentStatus::Running), false), 1);
    }

    #[test]
    fn tier_two_idle() {
        assert_eq!(attention_rank(Some(&AgentStatus::Idle), false), 2);
    }

    #[test]
    fn tier_two_live_without_status() {
        // A workspace with an open agent tab but no concrete status → tier 2.
        assert_eq!(attention_rank(None, true), 2);
    }

    #[test]
    fn tier_two_idle_and_live() {
        // Idle + live (the green "Ready" case) stays tier 2 — the live flag
        // must not promote it above Running.
        assert_eq!(attention_rank(Some(&AgentStatus::Idle), true), 2);
    }

    #[test]
    fn tier_three_done_clean() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Done { code: Some(0) }), false),
            3
        );
    }

    #[test]
    fn tier_four_done_nonzero() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Done { code: Some(1) }), false),
            4
        );
    }

    #[test]
    fn tier_four_done_unknown_code() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Done { code: None }), false),
            4
        );
    }

    #[test]
    fn tier_four_failed() {
        assert_eq!(
            attention_rank(Some(&AgentStatus::Failed("boom".into())), false),
            4
        );
    }

    #[test]
    fn tier_four_interrupted() {
        assert_eq!(attention_rank(Some(&AgentStatus::Interrupted), false), 4);
    }

    // ── sort_agent_rows — tier ordering ──────────────────────────────────────

    fn make_row(id: &str, status: Option<AgentStatus>, is_live: bool) -> AgentRow {
        let theme = t();
        let verb = agent_verb(status.as_ref(), is_live, theme);
        AgentRow {
            project_name: "P".to_string(),
            workspace: workspace(id, "proj"),
            verb,
            diff: None,
            status,
            is_live,
            activity: None,
        }
    }

    #[test]
    fn sort_puts_needs_approval_before_running() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Running), false),
            make_row("b", Some(AgentStatus::NeedsApproval("x".into())), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "b");
        assert_eq!(rows[1].workspace.id, "a");
    }

    #[test]
    fn sort_puts_waiting_before_running() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Running), false),
            make_row("b", Some(AgentStatus::WaitingForInput), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "b");
    }

    #[test]
    fn sort_puts_running_before_idle() {
        let mut rows = vec![
            make_row("idle", Some(AgentStatus::Idle), false),
            make_row("run", Some(AgentStatus::Running), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "run");
    }

    #[test]
    fn sort_puts_idle_before_done_clean() {
        let mut rows = vec![
            make_row("done", Some(AgentStatus::Done { code: Some(0) }), false),
            make_row("idle", Some(AgentStatus::Idle), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "idle");
    }

    #[test]
    fn sort_puts_done_clean_before_failed() {
        let mut rows = vec![
            make_row("fail", Some(AgentStatus::Failed("x".into())), false),
            make_row("done", Some(AgentStatus::Done { code: Some(0) }), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "done");
    }

    #[test]
    fn sort_is_stable_within_tier() {
        // Two running agents — input order must be preserved.
        let mut rows = vec![
            make_row("first", Some(AgentStatus::Running), false),
            make_row("second", Some(AgentStatus::Running), false),
        ];
        sort_agent_rows(&mut rows);
        assert_eq!(rows[0].workspace.id, "first");
        assert_eq!(rows[1].workspace.id, "second");
    }

    #[test]
    fn sort_full_mixed_list() {
        let mut rows = vec![
            make_row("a", Some(AgentStatus::Done { code: Some(0) }), false),
            make_row("b", Some(AgentStatus::Failed("x".into())), false),
            make_row("c", Some(AgentStatus::Running), false),
            make_row("d", Some(AgentStatus::WaitingForInput), false),
            make_row("e", Some(AgentStatus::Idle), false),
        ];
        sort_agent_rows(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|r| r.workspace.id.as_str()).collect();
        // Expected order: d (0), c (1), e (2), a (3), b (4)
        assert_eq!(ids, vec!["d", "c", "e", "a", "b"]);
    }

    // ── build_agent_rows — filtering + assembly ───────────────────────────────

    #[test]
    fn dormant_workspaces_are_skipped() {
        let p = project("p1", "Proj");
        let ws = workspace("a", "p1");
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &HashMap::new(),   // no status
            &HashSet::new(),   // not live
            &HashMap::new(),
            &HashMap::new(),
            t(),
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
        let rows = build_agent_rows(&[p], &wbp, &HashMap::new(), &live, &HashMap::new(), &HashMap::new(), t());
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
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            t(),
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
        diff_map.insert(worktree_path, DiffCounts { added: 5, removed: 3 });
        let rows = build_agent_rows(&[p], &wbp, &HashMap::new(), &live, &diff_map, &HashMap::new(), t());
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
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            t(),
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
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            t(),
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
        status_map.insert("done".to_string(), Some(AgentStatus::Done { code: Some(0) }));
        let mut activity = HashMap::new();
        activity.insert("run".to_string(), "Bash: cargo test".to_string());
        // Stale entry for a finished session must NOT surface.
        activity.insert("done".to_string(), "Bash: old".to_string());
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![run, done]);
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &activity,
            t(),
        );
        let running = rows.iter().find(|r| r.workspace.id == "run").unwrap();
        let finished = rows.iter().find(|r| r.workspace.id == "done").unwrap();
        assert_eq!(running.activity.as_deref(), Some("Bash: cargo test"));
        assert!(finished.activity.is_none());
    }

    // ── widest_row_index ──────────────────────────────────────────────────────

    #[test]
    fn widest_row_index_empty_is_none() {
        assert_eq!(widest_row_index(&[]), None);
    }

    #[test]
    fn widest_row_index_picks_longest_label() {
        let rows = vec![
            make_row("a", Some(AgentStatus::Running), false),
            make_row(
                "a-much-longer-workspace-identifier-for-width",
                Some(AgentStatus::Running),
                false,
            ),
        ];
        assert_eq!(widest_row_index(&rows), Some(1));
    }

    #[test]
    fn project_name_is_carried_through() {
        let p = project("p1", "MyProject");
        let ws = workspace("a", "p1");
        let mut status_map: LatestStatusMap = HashMap::new();
        status_map.insert("a".to_string(), Some(AgentStatus::Running));
        let mut wbp = HashMap::new();
        wbp.insert("p1".to_string(), vec![ws]);
        let rows = build_agent_rows(
            &[p],
            &wbp,
            &status_map,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            t(),
        );
        assert_eq!(rows[0].project_name, "MyProject");
    }
}
