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

/// Format a relative age ("now", "5m", "3h", "2d") from an RFC-3339 timestamp
/// against a captured `now`, with thresholds `<1m → now`, `<1h → Nm`,
/// `<24h → Nh`, else `Nd`. Empty on unparseable input. Shared by the rail's
/// agent rows and the Tasks page's `Updated` column (both want the narrow form;
/// the wider dashboard cards use [`relative_age_long`]).
pub(crate) fn relative_age_compact(ts: &str, now: &str) -> String {
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

/// Long-form relative age ("just now", "5 minutes ago", "14 hours ago",
/// "6 days ago") from an RFC-3339 timestamp against a captured `now` — the
/// reference cockpit's AGENTS-PAGE wording. The compact `relative_age_compact`
/// above is for the narrow rail rows; this is for the wider dashboard cards.
/// Empty on unparseable input.
pub fn relative_age_long(ts: &str, now: &str) -> String {
    let (Ok(t), Ok(n)) = (
        DateTime::parse_from_rfc3339(ts),
        DateTime::parse_from_rfc3339(now),
    ) else {
        return String::new();
    };
    let secs = (n - t).num_seconds().max(0);
    let ago = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        ago(secs / 60, "minute")
    } else if secs < 86_400 {
        ago(secs / 3_600, "hour")
    } else {
        ago(secs / 86_400, "day")
    }
}

/// History scope for a NON-live row: shown ONLY when it is a CLEANLY-COMPLETED
/// session (`Done { code: 0 }`) whose `ended_at` is at or after the recency
/// cutoff (unbounded when `cutoff` is `None`). This mirrors the reference
/// cockpit, which retains only finished (`done && !interrupted`) agents as
/// history — a manually closed tab, a cancelled run, or a crash (`Interrupted`
/// / `Failed` / non-zero exit) is NOT a finished agent, so it disappears from
/// the rail instead of lingering as a red row. An agent that was open at the
/// last app quit comes back as a LIVE row via re-adoption (its title restored
/// from the per-PTY persistence), not as a dead history row. A never-closed
/// `Idle`/`Running` leftover (not terminal) is dropped too.
fn is_recent_history(s: &AgentSession, cutoff: Option<&str>) -> bool {
    if !matches!(s.status, AgentStatus::Done { code: Some(0) }) {
        return false;
    }
    match (cutoff, s.ended_at.as_deref()) {
        (Some(cut), Some(ended)) => ended >= cut,
        // A cutoff is set but this row never stamped its end → stale.
        (Some(_), None) => false,
        // No cutoff (tests / unbounded) → keep clean-exit history.
        (None, _) => true,
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

/// One hand-launched (ambient) agent resolved to its owning workspace. `pty_id`
/// is the per-pane identity (each terminal is its own rail row); `worktree_path`
/// (a workspace root, as produced by the cwd→workspace resolver) groups it.
#[derive(Clone, Debug, PartialEq)]
pub struct AmbientRow {
    pub pty_id: String,
    pub worktree_path: String,
    pub agent: AmbientAgent,
}

/// Add live rows for agent CLIs detected inside plain terminal tabs — one row
/// per PTY, so N hand-launched agents in a worktree list as N rows (the
/// reference cockpit's per-pane identity) rather than collapsing to one. These
/// rows are intentionally ephemeral: they do not write to `agent_sessions`, but
/// they make hand-launched Claude/Codex/etc. participate in the same
/// per-workspace list as spawned agents.
pub fn append_ambient_agent_rows(
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    ambient_rows: &[AmbientRow],
    workspace_agents: &mut WorkspaceAgentList,
) {
    for ambient in ambient_rows {
        // Group under the workspace whose root is this terminal's resolved
        // worktree path; skip if it no longer maps to a known workspace.
        let Some(ws) = workspaces_by_project
            .values()
            .flat_map(|w| w.iter())
            .find(|w| w.worktree_path == ambient.worktree_path)
        else {
            continue;
        };
        let db_id = format!("ambient:{}", ambient.pty_id);
        let rows = workspace_agents.entry(ws.id.clone()).or_default();
        // Dedup by PTY id — idempotent if the same terminal is seen twice.
        if rows.iter().any(|r| r.db_id == db_id) {
            continue;
        }
        let label = ambient.agent.label.unwrap_or("Agent");
        // Surface the hook detail (the user's prompt, or the live tool step) as
        // the row's title — the reference cockpit's primary per-agent
        // distinguisher. The persisted title is compared by
        // `agents_display_equal`, so a change repaints through the dirty-check
        // even though the row carries no live receiver.
        let persisted_title = ambient
            .agent
            .detail
            .as_ref()
            .and_then(ambient_title_from_detail);
        rows.push(RailAgentRow {
            db_id,
            target: RailAgentTarget::AmbientTerminal {
                pty_id: ambient.pty_id.clone(),
            },
            workspace_key: ws.id.clone(),
            adapter_id: adapter_id_for_label(label).to_string(),
            label: label.into(),
            is_live: true,
            status_rx: None,
            db_status: ambient.agent.status.clone(),
            started_at: None,
            ended_at: None,
            age_label: "now".to_string(),
            persisted_title,
            // Ambient rows carry their reply in `ambient_detail` (re-seeded from
            // the persisted reading on a warm re-attach), not the DB column.
            persisted_message: None,
            // Carries the live tool step + last assistant message so the row's
            // secondary text and done-check render like a tracked agent.
            ambient_detail: ambient.agent.detail.clone(),
        });
        sort_and_cap_rows(rows);
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
/// DESC). The result is the **live + recent-history** set: every live session
/// plus only CLEANLY-COMPLETED history (`Done { code: 0 }`) whose end is at or
/// after `history_cutoff` (an RFC-3339 `now - 24h`) — see [`is_recent_history`].
/// Like the reference cockpit, a closed/interrupted/failed agent disappears
/// rather than lingering; never-closed leftovers and old history drop too. Pass
/// `None` to keep all clean-exit history (tests).
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
    //    only when it is recent TERMINAL history (see `is_recent_history`) —
    //    a quit-interrupted "sleeping" agent or a recent clean exit — so an
    //    agent and its persisted title survive a restart; older leftovers and
    //    never-closed rows are dropped so the list stays a recent set.
    for s in db_sessions {
        let live = live_agents
            .get(&s.id)
            .filter(|e| e.workspace_key == workspace_key);
        if live.is_none() && !is_recent_history(s, history_cutoff) {
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
                .map(|t| relative_age_compact(t, now))
                .unwrap_or_default(),
            persisted_title: s.title.clone(),
            // Falls back to this when the live channel has no message — a
            // restored session keeps its finished-turn reply across a restart.
            persisted_message: s.last_message.clone(),
            // Tracked rows read live tool/message from `status_rx`.
            ambient_detail: None,
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
                age_label: relative_age_compact(&e.started_at, now),
                // No DB row yet → no persisted title/message; the live channel
                // supplies the prompt + reply once they arrive.
                persisted_title: None,
                persisted_message: None,
                ambient_detail: None,
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

    #[test]
    fn relative_age_long_uses_full_words_and_pluralizes() {
        assert_eq!(relative_age_long("2026-06-23T23:59:30Z", NOW), "just now");
        assert_eq!(relative_age_long("2026-06-23T23:59:00Z", NOW), "1 minute ago");
        assert_eq!(relative_age_long("2026-06-23T23:30:00Z", NOW), "30 minutes ago");
        assert_eq!(relative_age_long("2026-06-23T23:00:00Z", NOW), "1 hour ago");
        assert_eq!(relative_age_long("2026-06-23T10:00:00Z", NOW), "14 hours ago");
        assert_eq!(relative_age_long("2026-06-18T00:00:00Z", NOW), "6 days ago");
        assert_eq!(relative_age_long("not-a-date", NOW), "");
    }

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
            last_message: None,
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
    fn persisted_last_message_rides_the_tracked_row_for_restart_restore() {
        // A re-adopted (non-live) session kept as recent history carries its
        // DB-persisted last reply onto the row, so the rail can restore the
        // finished-turn message + done-check across a restart.
        let mut s = db("a", AgentStatus::Done { code: Some(0) }, NOW, Some(NOW));
        s.last_message = Some("All set — shipped the fix.".into());
        let rows = merge_workspace_agents(WS, &[s], &LiveAgentMap::new(), None, NOW);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].persisted_message.as_deref(),
            Some("All set — shipped the fix.")
        );
        // Ambient rows never use the DB column.
        assert!(rows.iter().all(|r| r.ambient_detail.is_none()));
    }

    #[test]
    fn relative_age_thresholds() {
        let now = "2026-06-24T00:00:00Z";
        assert_eq!(relative_age_compact("2026-06-24T00:00:00Z", now), "now"); // 0s
        assert_eq!(relative_age_compact("2026-06-23T23:59:30Z", now), "now"); // 30s
        assert_eq!(relative_age_compact("2026-06-23T23:45:00Z", now), "15m");
        assert_eq!(relative_age_compact("2026-06-23T20:00:00Z", now), "4h");
        assert_eq!(relative_age_compact("2026-06-21T00:00:00Z", now), "3d");
        assert_eq!(relative_age_compact("garbage", now), ""); // unparseable
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
    fn non_live_keeps_only_clean_exit_history() {
        let sessions = vec![
            // Never-closed leftover (no terminal mark) — dropped as stale.
            db("idle", AgentStatus::Idle, "2026-06-23T10:00:00Z", None),
            // Interrupted (closed tab / quit) — DROPPED.
            db(
                "interrupted",
                AgentStatus::Interrupted,
                "2026-06-23T09:00:00Z",
                Some("2026-06-23T09:10:00Z"),
            ),
            // Clean exit — kept as recent history (the only retained kind).
            db(
                "done",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T09:00:00Z",
                Some("2026-06-23T09:30:00Z"),
            ),
        ];
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None, NOW);
        let ids: std::collections::HashSet<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        assert_eq!(ids, std::collections::HashSet::from(["done"]));
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
    fn history_keeps_recent_clean_exit_drops_interrupted_and_old() {
        let sessions = vec![
            // Clean exit but long before the cutoff → dropped by age.
            db(
                "old-done",
                AgentStatus::Done { code: Some(0) },
                "2026-06-20T08:00:00Z",
                Some("2026-06-20T09:00:00Z"),
            ),
            // Clean exit after the cutoff → kept (the only retained kind).
            db(
                "recent-done",
                AgentStatus::Done { code: Some(0) },
                "2026-06-23T08:00:00Z",
                Some("2026-06-23T09:00:00Z"),
            ),
            // Recent interrupted (closed tab / quit) → DROPPED: a
            // closed agent disappears rather than lingering as a red row.
            db(
                "recent-interrupted",
                AgentStatus::Interrupted,
                "2026-06-23T08:00:00Z",
                Some("2026-06-23T09:05:00Z"),
            ),
            // Recent failure → also dropped (not a finished agent).
            db(
                "recent-failed",
                AgentStatus::Failed("boom".into()),
                "2026-06-23T08:00:00Z",
                Some("2026-06-23T09:06:00Z"),
            ),
        ];
        let cutoff = "2026-06-22T00:00:00Z";
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), Some(cutoff), NOW);
        let ids: std::collections::HashSet<&str> = rows.iter().map(|r| r.db_id.as_str()).collect();
        assert_eq!(ids, std::collections::HashSet::from(["recent-done"]));
    }

    #[test]
    fn history_drops_terminal_without_end_stamp_when_cutoff_applies() {
        // A clean-exit row that never stamped `ended_at` (legacy) is stale, so a
        // recency cutoff drops it rather than falling back to its (possibly
        // recent) start time; unbounded keeps it.
        let sessions = vec![db(
            "no-end",
            AgentStatus::Done { code: Some(0) },
            "2026-06-23T23:00:00Z",
            None,
        )];
        let cutoff = "2026-06-22T00:00:00Z";
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), Some(cutoff), NOW);
        assert!(rows.is_empty());
        // With no cutoff (unbounded), the same row is kept.
        let rows = merge_workspace_agents(WS, &sessions, &LiveAgentMap::new(), None, NOW);
        assert_eq!(rows.len(), 1);
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

    fn ambient_row(pty: &str, worktree: &str, prompt: &str) -> AmbientRow {
        AmbientRow {
            pty_id: pty.to_string(),
            worktree_path: worktree.to_string(),
            agent: AmbientAgent {
                status: AgentStatus::Running,
                label: Some("Claude Code"),
                detail: Some(oximux_core::SidebandDetail {
                    prompt: Some(prompt.to_string()),
                    ..Default::default()
                }),
            },
        }
    }

    #[test]
    fn ambient_terminal_agent_is_added_as_live_workspace_row() {
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let mut out =
            build_workspace_agent_lists(&wbp, &HashMap::new(), &LiveAgentMap::new(), None, NOW);

        append_ambient_agent_rows(&wbp, &[ambient_row("pty-1", "/tmp", "fix the parser")], &mut out);

        let rows = out.get("ws-a").expect("ambient row creates list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].db_id, "ambient:pty-1");
        assert_eq!(rows[0].adapter_id, "claude-code");
        // The hook prompt becomes the row's title (the per-agent distinguisher).
        assert_eq!(rows[0].persisted_title.as_deref(), Some("fix the parser"));
        assert_eq!(rows[0].effective_status(), AgentStatus::Running);
        assert!(rows[0].is_focusable());
        assert_eq!(
            rows[0].target,
            RailAgentTarget::AmbientTerminal {
                pty_id: "pty-1".to_string(),
            }
        );
    }

    #[test]
    fn multiple_ambient_terminals_in_one_workspace_each_get_a_row() {
        // The core fix: N hand-launched agents in one worktree must list as N
        // distinct rows (per-PTY identity), not collapse to a single row.
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let mut out =
            build_workspace_agent_lists(&wbp, &HashMap::new(), &LiveAgentMap::new(), None, NOW);

        append_ambient_agent_rows(
            &wbp,
            &[
                ambient_row("pty-1", "/tmp", "hi 1"),
                ambient_row("pty-2", "/tmp", "hi 2"),
                ambient_row("pty-3", "/tmp", "hi 3"),
                // A duplicate of pty-2 is idempotent (no extra row).
                ambient_row("pty-2", "/tmp", "hi 2"),
            ],
            &mut out,
        );

        let rows = out.get("ws-a").expect("ambient rows create list");
        assert_eq!(rows.len(), 3, "three distinct PTYs → three rows");
        let titles: std::collections::HashSet<_> =
            rows.iter().filter_map(|r| r.persisted_title.clone()).collect();
        assert_eq!(
            titles,
            ["hi 1", "hi 2", "hi 3"].iter().map(|s| s.to_string()).collect()
        );
    }

    /// A row for an agent found by its process rather than by a hook: it has a
    /// status and a name, but no sideband detail to draw a title from. This is
    /// the common shape now — most CLIs emit no hook at all, and an agent idle
    /// at its prompt emits nothing even when it can.
    fn process_row(pty: &str, worktree: &str, label: &'static str) -> AmbientRow {
        AmbientRow {
            pty_id: pty.to_string(),
            worktree_path: worktree.to_string(),
            agent: AmbientAgent {
                status: AgentStatus::Idle,
                label: Some(label),
                detail: None,
            },
        }
    }

    #[test]
    fn different_agents_in_one_workspace_keep_their_own_names_and_icons() {
        // The multi-agent shape the cockpit has to show: three DIFFERENT CLIs
        // side by side in one worktree. Each row must carry its own label and
        // adapter slug — the slug is what picks the brand mark, so a shared or
        // defaulted one would render three identical-looking rows for three
        // different agents.
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let mut out =
            build_workspace_agent_lists(&wbp, &HashMap::new(), &LiveAgentMap::new(), None, NOW);

        append_ambient_agent_rows(
            &wbp,
            &[
                process_row("pty-1", "/tmp", "Claude Code"),
                process_row("pty-2", "/tmp", "Codex"),
                process_row("pty-3", "/tmp", "Gemini CLI"),
            ],
            &mut out,
        );

        let rows = out.get("ws-a").expect("ambient rows create list");
        assert_eq!(rows.len(), 3);
        let mut named: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r.label.to_string(), r.adapter_id.clone()))
            .collect();
        named.sort();
        assert_eq!(
            named,
            vec![
                ("Claude Code".to_string(), "claude-code".to_string()),
                ("Codex".to_string(), "codex".to_string()),
                ("Gemini CLI".to_string(), "gemini".to_string()),
            ]
        );
        assert!(
            rows.len() > 1,
            "more than one row is what opens the `N agents` disclosure"
        );
    }

    #[test]
    fn a_process_detected_agent_needs_no_hook_detail_to_get_a_row() {
        // An agent that has never emitted a hook still belongs in the rail: it
        // simply has no prompt to show as a title. Dropping such a row (or
        // demanding a title) would put us back to listing only the one CLI
        // that reports hooks.
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let mut out =
            build_workspace_agent_lists(&wbp, &HashMap::new(), &LiveAgentMap::new(), None, NOW);

        append_ambient_agent_rows(&wbp, &[process_row("pty-1", "/tmp", "Codex")], &mut out);

        let rows = out.get("ws-a").expect("ambient row creates list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Codex");
        assert_eq!(rows[0].persisted_title, None, "no hook, so no prompt title");
        assert_eq!(rows[0].effective_status(), AgentStatus::Idle);
        assert!(rows[0].is_live, "a running process is a live agent");
        assert!(rows[0].is_focusable());
    }

    #[test]
    fn hand_launched_agents_list_alongside_a_tracked_session() {
        // A worktree can hold both at once: one agent OxiMux spawned (a DB
        // session) and others typed into terminals. All of them belong to the
        // same workspace list, and together they cross the threshold that
        // opens the disclosure.
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let sessions = HashMap::from([(
            "ws-a".to_string(),
            vec![db("sess-1", AgentStatus::Idle, "2026-06-23T23:00:00Z", None)],
        )]);
        // Held for the test's lifetime: dropping the sender closes the watch
        // channel and the row stops reading as live.
        let (_tx, rx) = live_rx(AgentStatus::Running);
        let mut live = LiveAgentMap::new();
        live.insert("sess-1".into(), entry("ws-a", rx, "2026-06-23T23:00:00Z"));
        let mut out = build_workspace_agent_lists(&wbp, &sessions, &live, None, NOW);
        assert_eq!(out.get("ws-a").map(Vec::len), Some(1), "the tracked session");

        append_ambient_agent_rows(
            &wbp,
            &[
                process_row("pty-1", "/tmp", "Codex"),
                process_row("pty-2", "/tmp", "Pi"),
            ],
            &mut out,
        );

        let rows = out.get("ws-a").expect("rows");
        assert_eq!(rows.len(), 3, "one tracked + two hand-launched");
        assert_eq!(
            rows.iter().filter(|r| r.db_id.starts_with("ambient:")).count(),
            2
        );
    }

    #[test]
    fn an_ambient_agent_outside_every_workspace_is_dropped() {
        // A terminal `cd`'d somewhere unrelated has no workspace to hang from;
        // inventing one would put a row under the wrong project.
        let wbp = HashMap::from([("proj-1".to_string(), vec![ws("ws-a")])]);
        let mut out =
            build_workspace_agent_lists(&wbp, &HashMap::new(), &LiveAgentMap::new(), None, NOW);

        append_ambient_agent_rows(
            &wbp,
            &[process_row("pty-1", "/somewhere/else", "Codex")],
            &mut out,
        );

        assert!(out.is_empty(), "no workspace matched, so no row");
    }
}
