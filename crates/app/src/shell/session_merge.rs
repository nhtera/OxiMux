//! Merge live agent sessions with DB history into a per-workspace list.
//!
//! The rail used to collapse a workspace's agents to its single most-recent
//! session. This pure function instead unifies the live runtime sessions
//! (`LiveAgentMap`, keyed by `agent_sessions.id` UUID) with the workspace's
//! DB history rows by that same UUID, yielding one `RailAgentRow` per agent.
//! Kept free of GPUI so it is directly unit-testable.

use std::collections::{HashMap, HashSet};

use oximux_core::{AgentSession, AgentStatus, Workspace};

use crate::shell::agent_presentation::adapter_display_name;
use crate::shell::left_rail::{RailAgentRow, WorkspaceAgentList};
use crate::shell::session_live_store::LiveAgentMap;

/// Max agent rows kept per workspace (live + history). Newest win after the
/// live-first sort, so this trims the oldest finished sessions.
pub const HISTORY_CAP: usize = 20;

/// Build every workspace's agent list in one pass — the whole rail's
/// per-workspace lists, merging each workspace's DB history with the live
/// sessions. Workspaces with no agents are omitted (the rail keeps its
/// collapsed single-dot for those). Pure: `history_cutoff` (RFC-3339
/// `now - 24h`) is supplied by the caller so this stays clock-free.
pub fn build_workspace_agent_lists(
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    sessions_by_workspace: &HashMap<String, Vec<AgentSession>>,
    live_agents: &LiveAgentMap,
    history_cutoff: Option<&str>,
) -> WorkspaceAgentList {
    let mut out: WorkspaceAgentList = HashMap::new();
    for workspaces in workspaces_by_project.values() {
        for ws in workspaces {
            let db = sessions_by_workspace
                .get(&ws.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let rows = merge_workspace_agents(&ws.id, db, live_agents, history_cutoff);
            if !rows.is_empty() {
                out.insert(ws.id.clone(), rows);
            }
        }
    }
    out
}

/// Build the sorted agent list for one workspace.
///
/// `db_sessions` is `list_for_workspace` output (all rows, `started_at`
/// DESC). `history_cutoff` is an RFC-3339 instant (`now - 24h`); terminal
/// history rows that ended before it are dropped unless still live. Pass
/// `None` to disable the age cull (tests, or when no clock is handy).
pub fn merge_workspace_agents(
    workspace_key: &str,
    db_sessions: &[AgentSession],
    live_agents: &LiveAgentMap,
    history_cutoff: Option<&str>,
) -> Vec<RailAgentRow> {
    let mut rows: Vec<RailAgentRow> = Vec::with_capacity(db_sessions.len() + 1);

    // 1. DB rows. A matching live entry (same UUID + workspace) upgrades the
    //    row to live and attaches its status receiver.
    for s in db_sessions {
        let live = live_agents
            .get(&s.id)
            .filter(|e| e.workspace_key == workspace_key);
        // Cull stale finished history (ended before the cutoff) — but never a
        // live session, however old its original launch.
        if live.is_none()
            && s.status.is_terminal()
            && let (Some(cutoff), Some(ended)) = (history_cutoff, s.ended_at.as_deref())
            && ended < cutoff
        {
            continue;
        }
        rows.push(RailAgentRow {
            db_id: s.id.clone(),
            workspace_key: workspace_key.to_string(),
            adapter_id: s.adapter_id.clone(),
            label: live
                .map(|e| e.label.clone())
                .unwrap_or_else(|| adapter_display_name(&s.adapter_id).into()),
            is_live: live.is_some(),
            status_rx: live.map(|e| e.status_rx.clone()),
            db_status: s.status.clone(),
            started_at: s.started_at.clone(),
            ended_at: s.ended_at.clone(),
        });
    }

    // 2. Live entries whose DB insert hasn't landed yet (race on open) get a
    //    synthetic row so a freshly-spawned agent appears without waiting.
    let db_ids: HashSet<&str> = db_sessions.iter().map(|s| s.id.as_str()).collect();
    for (id, e) in live_agents {
        if e.workspace_key == workspace_key && !db_ids.contains(id.as_str()) {
            rows.push(RailAgentRow {
                db_id: id.clone(),
                workspace_key: workspace_key.to_string(),
                adapter_id: e.adapter_id.to_string(),
                label: e.label.clone(),
                is_live: true,
                status_rx: Some(e.status_rx.clone()),
                db_status: AgentStatus::Idle,
                started_at: Some(e.started_at.clone()),
                ended_at: None,
            });
        }
    }

    // 3. Live first, then most-recent launch first. Cap the tail.
    rows.sort_by(|a, b| {
        b.is_live
            .cmp(&a.is_live)
            .then_with(|| b.started_at.cmp(&a.started_at))
    });
    rows.truncate(HISTORY_CAP);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::session_live_store::LiveAgentEntry;
    use oximux_agents::AgentStatusStream;
    use oximux_core::AgentSnapshot;
    use tokio::sync::watch;

    const WS: &str = "primary:proj-1";

    fn db(id: &str, status: AgentStatus, started: &str, ended: Option<&str>) -> AgentSession {
        AgentSession {
            id: id.into(),
            workspace_id: WS.into(),
            adapter_id: "claude-code".into(),
            model: None,
            effort: None,
            status,
            started_at: Some(started.into()),
            ended_at: ended.map(Into::into),
        }
    }

    fn live_rx(status: AgentStatus) -> (watch::Sender<AgentSnapshot>, AgentStatusStream) {
        watch::channel(AgentSnapshot::from_status(status))
    }

    fn entry(ws: &str, rx: AgentStatusStream, started: &str) -> LiveAgentEntry {
        LiveAgentEntry {
            workspace_key: ws.into(),
            adapter_id: "claude-code",
            label: "Claude Code".into(),
            status_rx: rx,
            started_at: started.into(),
        }
    }

    fn ws(id: &str) -> Workspace {
        Workspace {
            id: id.into(),
            project_id: "proj-1".into(),
            name: id.into(),
            slug: id.into(),
            branch: "main".into(),
            worktree_path: "/tmp".into(),
            status: "active".into(),
            created_at: "2026-06-23T00:00:00Z".into(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
        }
    }

    #[test]
    fn all_db_no_live_marks_history() {
        let sessions = vec![
            db("a", AgentStatus::Idle, "2026-06-23T10:00:00Z", None),
            db(
                "b",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T09:00:00Z",
                Some("2026-06-23T09:30:00Z"),
            ),
        ];
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !r.is_live));
        assert!(rows.iter().all(|r| r.status_rx.is_none()));
    }

    #[test]
    fn live_matching_db_upgrades_row() {
        let sessions = vec![db("a", AgentStatus::Idle, "2026-06-23T10:00:00Z", None)];
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        live.insert("a".into(), entry(WS, rx, "2026-06-23T10:00:00Z"));

        let rows = merge_workspace_agents(WS, &sessions, &live, None);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_live);
        assert!(rows[0].status_rx.is_some());
        // Live snapshot overrides the DB-persisted Idle.
        assert_eq!(rows[0].effective_status(), AgentStatus::Running);
    }

    #[test]
    fn live_without_db_row_is_synthesized() {
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        live.insert("ghost".into(), entry(WS, rx, "2026-06-23T11:00:00Z"));

        let rows = merge_workspace_agents(WS, &[], &live, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].db_id, "ghost");
        assert!(rows[0].is_live);
        assert_eq!(rows[0].db_status, AgentStatus::Idle);
    }

    #[test]
    fn sorts_live_first_then_started_desc() {
        // Two history rows (older), one live (oldest launch) — live still leads.
        let sessions = vec![
            db("new", AgentStatus::Idle, "2026-06-23T12:00:00Z", None),
            db("old", AgentStatus::Idle, "2026-06-23T08:00:00Z", None),
        ];
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        live.insert("old".into(), entry(WS, rx, "2026-06-23T08:00:00Z"));

        let rows = merge_workspace_agents(WS, &sessions, &live, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].db_id, "old"); // live first despite older launch
        assert!(rows[0].is_live);
        assert_eq!(rows[1].db_id, "new");
    }

    #[test]
    fn caps_to_history_limit() {
        let sessions: Vec<AgentSession> = (0..30)
            .map(|i| {
                db(
                    &format!("s{i:02}"),
                    AgentStatus::Idle,
                    &format!("2026-06-23T{:02}:00:00Z", i % 24),
                    None,
                )
            })
            .collect();
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None);
        assert_eq!(rows.len(), HISTORY_CAP);
    }

    #[test]
    fn cutoff_drops_stale_terminal_but_keeps_live() {
        let sessions = vec![
            // Finished long before the cutoff → dropped.
            db(
                "stale",
                AgentStatus::Interrupted,
                "2026-06-20T08:00:00Z",
                Some("2026-06-20T09:00:00Z"),
            ),
            // Finished after the cutoff → kept.
            db(
                "recent",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T08:00:00Z",
                Some("2026-06-23T09:00:00Z"),
            ),
        ];
        let cutoff = "2026-06-22T00:00:00Z";
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), Some(cutoff));
        let ids: Vec<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        assert_eq!(ids, vec!["recent"]);
    }

    #[test]
    fn live_in_other_workspace_is_ignored() {
        let sessions = vec![db("a", AgentStatus::Idle, "2026-06-23T10:00:00Z", None)];
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        // Same UUID but a different workspace key — must not upgrade.
        live.insert("a".into(), entry("primary:other", rx, "2026-06-23T10:00:00Z"));

        let rows = merge_workspace_agents(WS, &sessions, &live, None);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].is_live);
    }

    #[test]
    fn build_lists_groups_by_workspace_and_omits_empty() {
        let wbp =
            HashMap::from([("proj-1".to_string(), vec![ws("ws-a"), ws("ws-empty")])]);
        let sessions = HashMap::from([(
            "ws-a".to_string(),
            vec![db("a", AgentStatus::Idle, "2026-06-23T10:00:00Z", None)],
        )]);
        let out = build_workspace_agent_lists(&wbp, &sessions, &LiveAgentMap::new(), None);
        // Only the workspace with sessions appears; the empty one is omitted.
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("ws-a").map(Vec::len), Some(1));
        assert!(!out.contains_key("ws-empty"));
    }
}
