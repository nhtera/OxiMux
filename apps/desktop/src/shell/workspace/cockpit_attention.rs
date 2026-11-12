//! Publish cockpit rows' alerts into the host's attention registry.
//!
//! The rows themselves do not need this: they resolve their attention from the
//! status they are already holding, so a blocked agent is in NEEDS YOU the
//! instant its row is built. This is for everyone who is *not* looking at a
//! row — the notifier, a phone, `HostCore::needs_you` — because a terminal
//! agent has no `SessionHealth` for the host to read, and without a producer
//! the host would believe nothing needs anyone.
//!
//! Both sides call `attention::external_of_agent_status`, so what is published
//! and what the row shows cannot disagree.

use oximux_agents::session_prefs::cockpit_key;
use oximux_remote_host::HostCore;

use crate::shell::left_rail::WorkspaceAgentList;
use crate::shell::left_rail::rail_agent_row::RailAgentTarget;
use crate::shell::left_rail::workspace_agent_rows::is_completed_turn;
use crate::shell::search_palette::recency::{now_ms, rfc3339_ms};

/// What one row does to the registry.
#[derive(Debug, Clone, PartialEq)]
pub enum RowAttention {
    /// A chat row: the session publishes its own health, so the host already
    /// has the better answer and an external item would compete with it.
    Skip,
    /// This alert, under the row's key.
    Raise(oximux_agents::attention::ExternalAttention),
    /// Nothing to raise, so whatever the key holds ends. The producer owns its
    /// key, and clearing is the only thing that ends an external alert.
    Clear(String),
}

/// What `row` should do to the registry. Pure, so the decision is testable
/// without a host — the publishing loop below only dispatches it.
pub fn attention_for_row(row: &crate::shell::left_rail::rail_agent_row::RailAgentRow) -> RowAttention {
    if matches!(row.target, RailAgentTarget::Chat { .. }) {
        return RowAttention::Skip;
    }
    let key = cockpit_key(&row.db_id);
    // The row's own last activity, never the clock: with the key it is the
    // item's edge identity, and the row resolver passes the same moment. A
    // clock here would make one alert look new on every refresh, and the
    // notifier would announce it again.
    let since = row
        .ended_at
        .as_deref()
        .or(row.started_at.as_deref())
        .and_then(rfc3339_ms)
        .unwrap_or_else(now_ms);
    match oximux_agents::attention::external_of_agent_status(
        key.clone(),
        key.clone(),
        &row.effective_status(),
        is_completed_turn(row),
        since,
    ) {
        Some(item) => RowAttention::Raise(item),
        None => RowAttention::Clear(key),
    }
}

/// Raise or clear one external item per cockpit row.
///
/// Safe to call on every rail refresh. `publish_external` returns early when
/// the item is unchanged, and `clear_external` on a key that holds nothing is
/// a no-op, so a quiet app publishes nothing and nothing is re-alerted.
///
/// Chat rows are skipped: those sessions are in the registry and publish their
/// own health, so the host already has the better answer and an external item
/// would compete with it.
pub fn publish_cockpit_attention(host: &HostCore, workspace_agents: &WorkspaceAgentList) {
    for rows in workspace_agents.values() {
        for row in rows {
            match attention_for_row(row) {
                RowAttention::Skip => {}
                RowAttention::Raise(item) => host.publish_external(item),
                RowAttention::Clear(key) => host.clear_external(&key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::left_rail::rail_agent_row::RailAgentRow;
    use oximux_agents::attention::AttentionKind;
    use oximux_core::AgentStatus;

    fn row(db_id: &str, target: RailAgentTarget, status: AgentStatus) -> RailAgentRow {
        RailAgentRow {
            db_id: db_id.into(),
            target,
            workspace_key: "ws".into(),
            adapter_id: "claude-code".into(),
            label: "Claude Code".into(),
            is_live: true,
            status_rx: None,
            db_status: status,
            started_at: Some("2026-06-23T00:00:00Z".into()),
            ended_at: None,
            age_label: "now".into(),
            persisted_title: None,
            persisted_message: None,
            ambient_detail: None,
        }
    }

    fn cockpit(status: AgentStatus) -> RailAgentRow {
        row("7", RailAgentTarget::AgentSession { db_id: "7".into() }, status)
    }

    #[test]
    fn a_blocked_cockpit_row_raises_an_alert_under_its_own_key() {
        let what = attention_for_row(&cockpit(AgentStatus::NeedsApproval("rm -rf build/".into())));
        let RowAttention::Raise(item) = what else {
            panic!("expected a raise, got {what:?}");
        };
        assert_eq!(item.key, "cockpit:7", "the row's key is its identity");
        assert_eq!(item.target, "cockpit:7", "and the row it renders on");
        assert_eq!(item.kind, AttentionKind::NeedsPermission);
        assert_eq!(item.reason, "rm -rf build/");
        assert_eq!(
            item.since,
            rfc3339_ms("2026-06-23T00:00:00Z").expect("a parseable fixture timestamp"),
            "the row's own activity moment, so a refresh does not mint a new edge"
        );
    }

    #[test]
    fn a_working_row_clears_its_key_rather_than_leaving_a_stale_alert() {
        // Nothing else ends an external alert: the producer owns its key, so a
        // row that stops needing anyone has to say so.
        assert_eq!(
            attention_for_row(&cockpit(AgentStatus::Running)),
            RowAttention::Clear("cockpit:7".into())
        );
    }

    #[test]
    fn a_chat_row_publishes_nothing_because_its_session_speaks_for_itself() {
        let chat = row(
            "s1",
            RailAgentTarget::Chat { session_id: "s1".into() },
            AgentStatus::NeedsApproval("x".into()),
        );
        assert_eq!(
            attention_for_row(&chat),
            RowAttention::Skip,
            "an external item would compete with the session's own health"
        );
    }

    #[test]
    fn two_refreshes_of_one_unchanged_row_mint_the_same_item() {
        // What keeps the notifier quiet: the edge identity is the key plus the
        // row's own activity moment, so re-publishing is a no-op rather than a
        // second alert.
        let blocked = cockpit(AgentStatus::Failed("boom".into()));
        assert_eq!(attention_for_row(&blocked), attention_for_row(&blocked));
    }
}
