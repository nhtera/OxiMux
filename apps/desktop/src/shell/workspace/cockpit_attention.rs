//! Publish cockpit rows' alerts into the host's attention registry.
//!
//! The rows themselves do not need this: they resolve their attention from the
//! status they are already holding, so a blocked agent is in NEEDS YOU the
//! instant its row is built. This is for everyone who is *not* looking at a
//! row — the notifier, a phone, `HostCore::needs_you` — because a terminal
//! agent has no `SessionHealth` for the host to read, and without a producer
//! the host would believe nothing needs anyone.
//!
//! Both sides ask `HostCore` for the item (`cockpit_item` here, the row
//! resolver's `attention_of_agent_status` there), so what is published and
//! what the row shows are the same alert, stamped with the same moment.

use std::collections::HashSet;

use oximux_agents::session_prefs::cockpit_key;
use oximux_remote_host::HostCore;

use crate::shell::left_rail::WorkspaceAgentList;
use crate::shell::left_rail::rail_agent_row::{RailAgentRow, RailAgentTarget};
use crate::shell::left_rail::workspace_agent_rows::is_completed_turn;

/// What one row does to the registry.
#[derive(Debug, Clone, PartialEq)]
pub enum RowAttention {
    /// A chat row: the session publishes its own health, so the host already
    /// has the better answer and an external item would compete with it.
    Skip,
    /// The key this row's alert lives under. The producer owns it: it raises
    /// what the row's status raises, and clearing is the only thing that ends
    /// an external alert.
    Own(String),
}

/// The key `row`'s alert is the producer's to own, or `Skip`. Pure, so the
/// decision is testable without a host; whether there is anything to raise
/// under it is the host's answer, asked once in the loop below, because the
/// host owns the moment that identifies the alert.
pub fn attention_for_row(row: &RailAgentRow) -> RowAttention {
    if matches!(row.target, RailAgentTarget::Chat { .. }) {
        return RowAttention::Skip;
    }
    RowAttention::Own(cockpit_key(&row.db_id))
}

/// The keys this pass can see, and the ones a previous pass raised that have
/// gone. Pure, so the end of a row's life is testable without a host.
///
/// A row that has left the snapshot is the case a per-row loop cannot handle:
/// it is not in the loop, so nothing would clear its key and `needs_you` would
/// keep naming a row no surface has. Closing a blocked terminal is exactly
/// that, and so is a failed row ageing out of the capped history.
///
/// `previous` is what *this* producer raised, not every cockpit key the host
/// holds. Each window runs its own pass over its own rows, and an agent found
/// inside another window's terminal is not in this one's: retiring every key
/// this pass cannot see would end that row's alert, the other window would
/// raise it again on its next frame, and the notifier would announce it twice.
pub fn cockpit_pass(
    previous: &HashSet<String>,
    workspace_agents: &WorkspaceAgentList,
) -> (HashSet<String>, Vec<String>) {
    let mut live: HashSet<String> = HashSet::new();
    for rows in workspace_agents.values() {
        for row in rows {
            if let RowAttention::Own(key) = attention_for_row(row) {
                live.insert(key);
            }
        }
    }
    let gone = previous.difference(&live).cloned().collect();
    (live, gone)
}

/// Raise or clear one external item per cockpit row, then retire the alerts of
/// the rows this producer raised and can no longer see.
///
/// Safe to call on every rail refresh. `publish_external` returns early when
/// the item is unchanged — and the item *is* unchanged while the alert is, the
/// host stamping it rather than the row — so a quiet app publishes nothing and
/// nothing is re-alerted.
///
/// `raised` is this producer's memory of the keys it owns, updated in place.
///
/// Chat rows are skipped: those sessions are in the registry and publish their
/// own health, so the host already has the better answer and an external item
/// would compete with it.
pub fn publish_cockpit_attention(
    host: &HostCore,
    workspace_agents: &WorkspaceAgentList,
    raised: &mut HashSet<String>,
) {
    let (live, gone) = cockpit_pass(raised, workspace_agents);
    for rows in workspace_agents.values() {
        for row in rows {
            let RowAttention::Own(key) = attention_for_row(row) else {
                continue;
            };
            match host.cockpit_item(
                &key,
                &key,
                &row.effective_status(),
                is_completed_turn(row),
                crate::shell::agents_dashboard::row_attention::row_began(row),
            ) {
                Some(item) => host.publish_external(item),
                // Nothing to raise, so whatever the key holds ends here.
                None => host.clear_external(&key),
            }
        }
    }
    for key in gone {
        host.clear_external(&key);
    }
    *raised = live;
}

/// End every alert this producer raised, because its rows are going.
///
/// A closing window's pass never runs again, and the host is shared across the
/// process: without this, an alert the window raised would stand in NEEDS YOU
/// for the rest of the run with no pass left to retire it. Called from the
/// window-closed observer, which holds the root long enough for exactly this
/// kind of teardown.
pub fn retire_cockpit_attention(host: &HostCore, raised: &mut HashSet<String>) {
    for key in raised.drain() {
        host.clear_external(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn a_cockpit_rows_key_is_its_own_whatever_its_status() {
        // One key per row, raised or cleared by what the host makes of the
        // status — never a key that appears only while the row is blocked,
        // which would leave nothing to clear when it stops being blocked.
        for status in [
            AgentStatus::NeedsApproval("rm -rf build/".into()),
            AgentStatus::Running,
            AgentStatus::Idle,
        ] {
            assert_eq!(
                attention_for_row(&cockpit(status)),
                RowAttention::Own("cockpit:7".into())
            );
        }
    }

    #[test]
    fn an_ambient_rows_key_is_its_pty_row_not_a_database_id() {
        // The sweep compares these keys against what the host holds, so the
        // spelling here has to be the one the row resolver uses.
        let ambient = row(
            "ambient:pty-3",
            RailAgentTarget::AmbientTerminal { pty_id: "pty-3".into() },
            AgentStatus::Failed("boom".into()),
        );
        assert_eq!(
            attention_for_row(&ambient),
            RowAttention::Own("cockpit:ambient:pty-3".into())
        );
    }

    #[test]
    fn a_row_that_leaves_the_snapshot_is_retired_by_the_pass_that_raised_it() {
        // Closing a blocked terminal takes its row away. Nothing in the next
        // pass mentions it, so unless the pass remembers what it raised, the
        // alert stands for the rest of the run.
        let mut agents: WorkspaceAgentList = std::collections::HashMap::new();
        agents.insert(
            "ws".to_string(),
            vec![cockpit(AgentStatus::NeedsApproval("rm -rf build/".into()))],
        );
        let (raised, gone) = cockpit_pass(&HashSet::new(), &agents);
        assert_eq!(raised, HashSet::from(["cockpit:7".to_string()]));
        assert!(gone.is_empty(), "nothing had been raised before this pass");

        let (now_live, gone) = cockpit_pass(&raised, &std::collections::HashMap::new());
        assert!(now_live.is_empty());
        assert_eq!(gone, vec!["cockpit:7".to_string()], "the row went and so does its alert");
    }

    #[test]
    fn a_quiet_row_is_still_this_passs_to_own() {
        // A working row raises nothing, but it is still here: the pass keeps
        // its key, so the alert it raises later is cleared when that alert
        // ends rather than being read as a departed row's and retired.
        let mut agents: WorkspaceAgentList = std::collections::HashMap::new();
        agents.insert("ws".to_string(), vec![cockpit(AgentStatus::Running)]);
        let (live, gone) = cockpit_pass(&HashSet::from(["cockpit:7".to_string()]), &agents);
        assert_eq!(live, HashSet::from(["cockpit:7".to_string()]));
        assert!(gone.is_empty(), "the row is still in front of the person");
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
}
