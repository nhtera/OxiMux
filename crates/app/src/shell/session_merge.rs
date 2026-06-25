//! Merge live agent sessions with DB history into a per-workspace list.
//!
//! The rail used to collapse a workspace's agents to its single most-recent
//! session. This pure function instead unifies the live runtime sessions
//! (`LiveAgentMap`, keyed by `agent_sessions.id` UUID) with the workspace's
//! DB history rows by that same UUID, yielding one `RailAgentRow` per agent.
//! Kept free of GPUI so it is directly unit-testable.

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use oximux_core::{AgentSession, AgentStatus, Workspace};

use crate::shell::agent_presentation::{AmbientAgent, adapter_display_name, adapter_id_for_label};
use crate::shell::left_rail::{RailAgentRow, RailAgentTarget, WorkspaceAgentList};
use crate::shell::session_live_store::LiveAgentMap;

/// Max agent rows kept per workspace (live + history). Newest win after the
/// live-first sort, so this trims the oldest finished sessions.
pub const HISTORY_CAP: usize = 20;

/// History scope for a NON-live row: shown only when it is a cleanly-finished
/// (`Done`) session whose `ended_at` is at or after the recency cutoff (or
/// unbounded when `cutoff` is `None`). This mirrors the reference cockpit,
/// which keeps only cleanly-completed agents as recent history — a crash,
/// a shutdown (`Interrupted`/`Failed`), or a never-closed `idle`/`running`
/// leftover with no live tab is not "recent work to glance at" and is dropped.
/// Format a relative age ("now", "5m", "3h", "2d") from an RFC-3339 timestamp
/// against a captured `now`, mirroring the reference cockpit's thresholds:
/// `<1m → now`, `<1h → Nm`, `<24h → Nh`, else `Nd`. Empty on unparseable input.
fn relative_age(ts: &str, now: &str) -> String {
    let (Ok(t), Ok(n)) = (
        DateTime::parse_from_rfc3339(ts),
        DateTime::parse_from_rfc3339(now),
    ) else {
        return String::new();
    };
    let secs = (n - t).num_seconds().max(0);
    if secs < 60 {
        "now".to_string()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn is_recent_done(s: &AgentSession, cutoff: Option<&str>) -> bool {
    if !matches!(s.status, AgentStatus::Done { .. }) {
        return false;
    }
    match (cutoff, s.ended_at.as_deref()) {
        (Some(cut), Some(ended)) => ended >= cut,
        _ => true,
    }
}

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
    now: &str,
) -> WorkspaceAgentList {
    let mut out: WorkspaceAgentList = HashMap::new();
    for workspaces in workspaces_by_project.values() {
        for ws in workspaces {
            let db = sessions_by_workspace
                .get(&ws.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let rows = merge_workspace_agents(&ws.id, db, live_agents, history_cutoff, now);
            if !rows.is_empty() {
                out.insert(ws.id.clone(), rows);
            }
        }
    }
    out
}

/// Add live rows for agent CLIs detected inside plain terminal tabs. These
/// rows are intentionally ephemeral: they do not write to `agent_sessions`,
/// but they make hand-launched Claude/Codex/etc. participate in the same
/// per-workspace list as spawned agents.
pub fn append_ambient_agent_rows(
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    ambient_by_path: &HashMap<String, AmbientAgent>,
    workspace_agents: &mut WorkspaceAgentList,
) {
    for workspaces in workspaces_by_project.values() {
        for ws in workspaces {
            let Some(agent) = ambient_by_path.get(&ws.worktree_path) else {
                continue;
            };
            let rows = workspace_agents.entry(ws.id.clone()).or_default();
            if rows.iter().any(|r| {
                matches!(
                    &r.target,
                    RailAgentTarget::AmbientTerminal { worktree_path }
                        if worktree_path == &ws.worktree_path
                )
            }) {
                continue;
            }
            let label = agent.label.unwrap_or("Agent");
            // Surface the hook detail (the user's prompt, or the live tool step)
            // as the row's title — the reference UX's primary per-agent distinguisher. The
            // detail rides the compared `ambient_status` map, so a change
            // repaints the rail through the dirty-check even though the row
            // carries no live receiver.
            let persisted_title = agent.detail.as_ref().and_then(ambient_title_from_detail);
            rows.push(RailAgentRow {
                db_id: format!("ambient:{}", ws.id),
                target: RailAgentTarget::AmbientTerminal {
                    worktree_path: ws.worktree_path.clone(),
                },
                workspace_key: ws.id.clone(),
                adapter_id: adapter_id_for_label(label).to_string(),
                label: label.into(),
                is_live: true,
                status_rx: None,
                db_status: agent.status.clone(),
                started_at: None,
                ended_at: None,
                age_label: "now".to_string(),
                persisted_title,
            });
            sort_and_cap_rows(rows);
        }
    }
}

/// The headline string for an ambient agent row from its hook detail: the
/// user's prompt when present, otherwise the live tool step (`"Edit: x.rs"`),
/// otherwise the last free-form message. `None` when the detail is empty.
fn ambient_title_from_detail(detail: &oximux_core::SidebandDetail) -> Option<String> {
    if let Some(p) = detail.prompt.as_deref().filter(|p| !p.is_empty()) {
        return Some(p.to_string());
    }
    if let Some(tool) = detail.tool_name.as_deref().filter(|t| !t.is_empty()) {
        return Some(
            match detail.tool_input_summary.as_deref().filter(|s| !s.is_empty()) {
                Some(input) => format!("{tool}: {input}"),
                None => tool.to_string(),
            },
        );
    }
    detail
        .last_message
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

/// Build the sorted agent list for one workspace.
///
/// `db_sessions` is `list_for_workspace` output (all rows, `started_at`
/// DESC). The result is the **live + recently-completed** set: every live
/// session plus only the cleanly-finished (`Done`) history whose `ended_at`
/// is at or after `history_cutoff` (an RFC-3339 `now - 24h`). Stale
/// non-terminal leftovers and interrupted/failed sessions are dropped — see
/// [`is_recent_done`]. Pass `None` to keep all `Done` history (tests).
pub fn merge_workspace_agents(
    workspace_key: &str,
    db_sessions: &[AgentSession],
    live_agents: &LiveAgentMap,
    history_cutoff: Option<&str>,
    now: &str,
) -> Vec<RailAgentRow> {
    let mut rows: Vec<RailAgentRow> = Vec::with_capacity(db_sessions.len() + 1);

    // 1. DB rows. A matching live entry (same UUID + workspace) upgrades the
    //    row to live and attaches its status receiver. A non-live row is kept
    //    only when it is recent cleanly-finished history (see `is_recent_done`)
    //    — stale leftovers and crashes are dropped so the list stays the
    //    live + recently-completed set, not the full session log.
    for s in db_sessions {
        let live = live_agents
            .get(&s.id)
            .filter(|e| e.workspace_key == workspace_key);
        if live.is_none() && !is_recent_done(s, history_cutoff) {
            continue;
        }
        rows.push(RailAgentRow {
            db_id: s.id.clone(),
            target: RailAgentTarget::AgentSession {
                db_id: s.id.clone(),
            },
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
            age_label: s
                .ended_at
                .as_deref()
                .or(s.started_at.as_deref())
                .map(|t| relative_age(t, now))
                .unwrap_or_default(),
            persisted_title: s.title.clone(),
        });
    }

    // 2. Live entries whose DB insert hasn't landed yet (race on open) get a
    //    synthetic row so a freshly-spawned agent appears without waiting.
    let db_ids: HashSet<&str> = db_sessions.iter().map(|s| s.id.as_str()).collect();
    for (id, e) in live_agents {
        if e.workspace_key == workspace_key && !db_ids.contains(id.as_str()) {
            rows.push(RailAgentRow {
                db_id: id.clone(),
                target: RailAgentTarget::AgentSession { db_id: id.clone() },
                workspace_key: workspace_key.to_string(),
                adapter_id: e.adapter_id.to_string(),
                label: e.label.clone(),
                is_live: true,
                status_rx: Some(e.status_rx.clone()),
                db_status: AgentStatus::Idle,
                started_at: Some(e.started_at.clone()),
                ended_at: None,
                age_label: relative_age(&e.started_at, now),
                // No DB row yet → no persisted title; the live channel supplies
                // the prompt once it arrives.
                persisted_title: None,
            });
        }
    }

    // 3. Live first, then most-recent launch first. Cap the tail.
    sort_and_cap_rows(&mut rows);
    rows
}

fn sort_and_cap_rows(rows: &mut Vec<RailAgentRow>) {
    rows.sort_by(|a, b| {
        b.is_live
            .cmp(&a.is_live)
            .then_with(|| b.started_at.cmp(&a.started_at))
    });
    rows.truncate(HISTORY_CAP);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::session_live_store::LiveAgentEntry;
    use oximux_agents::AgentStatusStream;
    use oximux_core::AgentSnapshot;
    use tokio::sync::watch;

    const WS: &str = "primary:proj-1";
    // Fixed clock for deterministic tests — later than every fixture timestamp.
    const NOW: &str = "2026-06-24T00:00:00Z";

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
            title: None,
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
            session_id: oximux_core::AgentSessionId::new(1),
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
    fn relative_age_thresholds() {
        let now = "2026-06-24T00:00:00Z";
        assert_eq!(relative_age("2026-06-24T00:00:00Z", now), "now"); // 0s
        assert_eq!(relative_age("2026-06-23T23:59:30Z", now), "now"); // 30s
        assert_eq!(relative_age("2026-06-23T23:45:00Z", now), "15m");
        assert_eq!(relative_age("2026-06-23T20:00:00Z", now), "4h");
        assert_eq!(relative_age("2026-06-21T00:00:00Z", now), "3d");
        assert_eq!(relative_age("garbage", now), ""); // unparseable
    }

    #[test]
    fn merge_populates_age_label() {
        let sessions = vec![db(
            "done",
            AgentStatus::Done { code: Some(0) },
            "2026-06-23T09:00:00Z",
            Some("2026-06-23T22:00:00Z"),
        )];
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None, NOW);
        // NOW is 2026-06-24T00:00:00Z; ended_at (used for done) is 2h earlier.
        assert_eq!(rows[0].age_label, "2h");
    }

    #[test]
    fn non_live_keeps_only_recent_done() {
        let sessions = vec![
            // Stale leftover (never closed) — dropped.
            db("idle", AgentStatus::Idle, "2026-06-23T10:00:00Z", None),
            // Crash / forced close — dropped.
            db(
                "crash",
                AgentStatus::Interrupted,
                "2026-06-23T09:00:00Z",
                Some("2026-06-23T09:10:00Z"),
            ),
            // Clean exit — kept as recent history.
            db(
                "done",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T09:00:00Z",
                Some("2026-06-23T09:30:00Z"),
            ),
        ];
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None, NOW);
        let ids: Vec<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        assert_eq!(ids, vec!["done"]);
        assert!(rows.iter().all(|r| !r.is_live && r.status_rx.is_none()));
    }

    #[test]
    fn live_matching_db_upgrades_row() {
        let sessions = vec![db("a", AgentStatus::Idle, "2026-06-23T10:00:00Z", None)];
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        live.insert("a".into(), entry(WS, rx, "2026-06-23T10:00:00Z"));

        let rows = merge_workspace_agents(WS, &sessions, &live, None, NOW);
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

        let rows = merge_workspace_agents(WS, &[], &live, None, NOW);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].db_id, "ghost");
        assert!(rows[0].is_live);
        assert_eq!(rows[0].db_status, AgentStatus::Idle);
    }

    #[test]
    fn sorts_live_first_then_started_desc() {
        // A live agent (older launch) + a recent Done history row — live leads
        // despite its older start time.
        let sessions = vec![db(
            "done-new",
            AgentStatus::Done { code: Some(0) },
            "2026-06-23T12:00:00Z",
            Some("2026-06-23T12:30:00Z"),
        )];
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        live.insert("live-old".into(), entry(WS, rx, "2026-06-23T08:00:00Z"));

        let rows = merge_workspace_agents(WS, &sessions, &live, None, NOW);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].db_id, "live-old"); // live first despite older launch
        assert!(rows[0].is_live);
        assert_eq!(rows[1].db_id, "done-new");
    }

    #[test]
    fn caps_to_history_limit() {
        let sessions: Vec<AgentSession> = (0..30)
            .map(|i| {
                db(
                    &format!("s{i:02}"),
                    AgentStatus::Done { code: Some(0) },
                    &format!("2026-06-23T{:02}:00:00Z", i % 24),
                    Some(&format!("2026-06-23T{:02}:30:00Z", i % 24)),
                )
            })
            .collect();
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None, NOW);
        assert_eq!(rows.len(), HISTORY_CAP);
    }

    #[test]
    fn history_keeps_only_recent_done() {
        let sessions = vec![
            // Clean exit but long before the cutoff → dropped by age.
            db(
                "old-done",
                AgentStatus::Done { code: Some(0) },
                "2026-06-20T08:00:00Z",
                Some("2026-06-20T09:00:00Z"),
            ),
            // Clean exit after the cutoff → kept.
            db(
                "recent-done",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T08:00:00Z",
                Some("2026-06-23T09:00:00Z"),
            ),
            // Recent but a crash, not a clean exit → dropped by status.
            db(
                "recent-crash",
                AgentStatus::Interrupted,
                "2026-06-23T08:00:00Z",
                Some("2026-06-23T09:05:00Z"),
            ),
        ];
        let cutoff = "2026-06-22T00:00:00Z";
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), Some(cutoff), NOW);
        let ids: Vec<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        assert_eq!(ids, vec!["recent-done"]);
    }

    #[test]
    fn live_in_other_workspace_is_ignored() {
        let sessions = vec![db(
            "a",
            AgentStatus::Done { code: Some(0) },
            "2026-06-23T10:00:00Z",
            Some("2026-06-23T10:30:00Z"),
        )];
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        // Same UUID but a different workspace key — must not upgrade this row.
        live.insert(
            "a".into(),
            entry("primary:other", rx, "2026-06-23T10:00:00Z"),
        );

        let rows = merge_workspace_agents(WS, &sessions, &live, None, NOW);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].is_live);
    }

    #[test]
    fn build_lists_groups_by_workspace_and_omits_empty() {
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a"), ws("ws-empty")])]);
        let sessions = HashMap::from([(
            "ws-a".to_string(),
            vec![db(
                "a",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T10:00:00Z",
                Some("2026-06-23T10:30:00Z"),
            )],
        )]);
        let out = build_workspace_agent_lists(&wbp, &sessions, &LiveAgentMap::new(), None, NOW);
        // Only the workspace with sessions appears; the empty one is omitted.
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("ws-a").map(Vec::len), Some(1));
        assert!(!out.contains_key("ws-empty"));
    }

    #[test]
    fn ambient_terminal_agent_is_added_as_live_workspace_row() {
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let mut out =
            build_workspace_agent_lists(&wbp, &HashMap::new(), &LiveAgentMap::new(), None, NOW);
        let ambient = HashMap::from([(
            "/tmp".to_string(),
            AmbientAgent {
                status: AgentStatus::Running,
                label: Some("Claude Code"),
                detail: Some(oximux_core::SidebandDetail {
                    prompt: Some("fix the parser".to_string()),
                    ..Default::default()
                }),
            },
        )]);

        append_ambient_agent_rows(&wbp, &ambient, &mut out);

        let rows = out.get("ws-a").expect("ambient row creates list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].db_id, "ambient:ws-a");
        assert_eq!(rows[0].adapter_id, "claude-code");
        // The hook prompt becomes the row's title (the reference UX's distinguisher).
        assert_eq!(rows[0].persisted_title.as_deref(), Some("fix the parser"));
        assert_eq!(rows[0].effective_status(), AgentStatus::Running);
        assert!(rows[0].is_focusable());
        assert_eq!(
            rows[0].target,
            RailAgentTarget::AmbientTerminal {
                worktree_path: "/tmp".to_string(),
            }
        );
    }
}
