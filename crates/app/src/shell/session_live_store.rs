//! Live agent registry keyed by DB session UUID.
//!
//! Each open agent tab already owns a `watch::Receiver<AgentSnapshot>`
//! (the per-tab badge reads it directly). The rail, however, only ever
//! saw a single collapsed status per workspace. To list every agent in a
//! workspace, the rail needs the live receivers keyed by a stable id that
//! also matches DB history rows — the `agent_sessions.id` UUID.
//!
//! The persistence watcher (`agent_session_persistence`) owns the
//! lifecycle: it registers an entry once the DB insert returns the UUID,
//! and removes it when the session reaches a terminal status or its status
//! sender drops (tab close). No separate watcher task is needed — the
//! existing one already spans exactly that lifetime.

use std::collections::HashMap;

use gpui::{Context, SharedString, Window};
use oximux_agents::AgentStatusStream;
use oximux_core::AgentSessionId;

use crate::workspace_root::WorkspaceRoot;

/// One live agent session, keyed in [`LiveAgentMap`] by its `agent_sessions.id`
/// UUID. Holds a cheap clone of the status receiver so the rail reads the
/// current snapshot directly via `status_rx.borrow()` — no mirrored value.
pub struct LiveAgentEntry {
    /// Owning workspace key (`workspaces.id` or `primary:<project_id>`).
    pub workspace_key: String,
    /// Stable adapter id (`claude-code`, `codex`, …).
    pub adapter_id: &'static str,
    /// Display label shown on the rail sub-row.
    pub label: SharedString,
    /// Live status receiver — the source of truth for this agent's state.
    pub status_rx: AgentStatusStream,
    /// Launch timestamp (DB `started_at`), for relative-age rendering.
    pub started_at: String,
    /// Runtime session handle — maps a clicked rail row back to its open tab.
    pub session_id: AgentSessionId,
}

/// Live agent sessions keyed by `agent_sessions.id` UUID. Populated while a
/// session is non-terminal; an entry survives only as long as its tab is open.
pub type LiveAgentMap = HashMap<String, LiveAgentEntry>;

impl WorkspaceRoot {
    /// Register a live agent under its DB UUID and repaint the rail. Called by
    /// the persistence watcher once the insert resolves the row id.
    pub(crate) fn register_live_agent(
        &mut self,
        db_session_id: String,
        entry: LiveAgentEntry,
        cx: &mut Context<Self>,
    ) {
        self.live_agents.insert(db_session_id, entry);
        self.mark_rail_dirty(cx);
    }

    /// Drop a live agent (terminal status or tab close). Repaints only when an
    /// entry actually existed so a double-remove cannot churn the rail.
    pub(crate) fn remove_live_agent(&mut self, db_session_id: &str, cx: &mut Context<Self>) {
        if self.live_agents.remove(db_session_id).is_some() {
            self.mark_rail_dirty(cx);
        }
    }

    /// Focus the live agent identified by its DB session UUID: resolve its
    /// runtime session, switch to the owning project if it isn't active, and
    /// activate the agent's tab. A history-only row (not in `live_agents`) has
    /// no open tab, so this is a no-op for it.
    pub(crate) fn focus_agent_by_db_id(
        &mut self,
        db_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.live_agents.get(db_id).map(|e| e.session_id) else {
            return;
        };
        // The session lives in exactly one project's panes; find its owner so
        // we switch to that project before searching it for the tab.
        let owner = self
            .project_panes_by_project
            .iter()
            .find_map(|(pid, panes)| {
                panes
                    .read(cx)
                    .has_agent_session(session_id, cx)
                    .then(|| pid.clone())
            });
        let Some(project_id) = owner else {
            return;
        };
        if self.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str()) {
            match self
                .app_state
                .recent_projects
                .iter()
                .find(|p| p.id == project_id)
                .cloned()
            {
                Some(project) => self.set_active_project(project, window, cx),
                None => return,
            }
        }
        if let Some(panes) = self.active_project_panes() {
            panes.update(cx, |p, cx| {
                p.focus_agent_session(session_id, window, cx);
            });
        }
    }

    /// Focus a hand-launched (ambient) agent the user clicked in the rail. The
    /// row is keyed by the terminal's PTY id (its per-pane identity) rather than
    /// a DB session id, because no tracked `agent_sessions` row exists for an
    /// ambient terminal agent.
    pub(crate) fn focus_ambient_agent_terminal(
        &mut self,
        pty_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = self
            .project_panes_by_project
            .iter()
            .find_map(|(pid, panes)| panes.read(cx).has_terminal_pty(pty_id, cx).then(|| pid.clone()));
        let Some(project_id) = owner else {
            return;
        };
        if self.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str()) {
            match self
                .app_state
                .recent_projects
                .iter()
                .find(|p| p.id == project_id)
                .cloned()
            {
                Some(project) => self.set_active_project(project, window, cx),
                None => return,
            }
        }
        if let Some(panes) = self.active_project_panes() {
            panes.update(cx, |p, cx| {
                p.focus_ambient_agent_terminal(pty_id, window, cx);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::{AgentSnapshot, AgentStatus};
    use tokio::sync::watch;

    fn entry(rx: AgentStatusStream) -> LiveAgentEntry {
        LiveAgentEntry {
            workspace_key: "primary:proj-1".into(),
            adapter_id: "claude-code",
            label: "Claude Code".into(),
            status_rx: rx,
            started_at: "2026-06-23T00:00:00Z".into(),
            session_id: AgentSessionId::new(1),
        }
    }

    #[test]
    fn map_keys_entries_by_uuid_and_reads_live_snapshot() {
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let mut map = LiveAgentMap::new();
        map.insert("uuid-1".to_string(), entry(rx));

        assert!(map.contains_key("uuid-1"));
        // The held receiver is a live view: a sender push is visible with no
        // mirrored copy — the contract the Phase 2 rail render relies on.
        tx.send(AgentSnapshot::from_status(AgentStatus::Running))
            .unwrap();
        assert_eq!(
            map["uuid-1"].status_rx.borrow().status,
            AgentStatus::Running
        );

        // Removal is keyed by the same UUID.
        assert!(map.remove("uuid-1").is_some());
        assert!(map.is_empty());
    }

    #[test]
    fn dropped_sender_still_borrows_last_snapshot() {
        // On tab close the runtime drops the status sender. The receiver still
        // borrows the final value — the persistence watcher uses that drop as
        // its remove trigger, so the rail never reads a poisoned receiver.
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Running));
        let e = entry(rx);
        drop(tx);
        assert_eq!(e.status_rx.borrow().status, AgentStatus::Running);
    }
}
