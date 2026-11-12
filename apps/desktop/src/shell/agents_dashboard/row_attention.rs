//! One rail row's attention, resolved through the host.
//!
//! `build_agent_rows` takes this as a closure so it stays pure and testable
//! without a window. This module is where the kinds of row are told apart, and
//! it is deliberately neither in `left_rail/mod.rs` nor in the dashboard's
//! render path: both are already past their size budget, and this is logic
//! rather than layout.

use std::sync::Arc;

use gpui::App;
use oximux_agents::attention::Attention;
use oximux_agents::session_prefs::{chat_key, cockpit_key};
use oximux_remote_host::HostCore;
use oximux_remote_host::attention::PrefsSnapshot;

use crate::shell::left_rail::rail_agent_row::{RailAgentRow, RailAgentTarget};
use crate::shell::left_rail::workspace_agent_rows::is_completed_turn;
use crate::shell::search_palette::recency::rfc3339_ms;

/// The key a row's attention and preferences live under.
///
/// One spelling, read by the resolver below, by the producer that publishes a
/// cockpit row's alert, and by the click that records a look — a row whose
/// look was stored under a different key than its alert would never clear.
pub fn attention_target(db_id: &str, target: &RailAgentTarget) -> String {
    match target {
        // A chat session is in the registry under its own id.
        RailAgentTarget::Chat { session_id } => chat_key(session_id),
        // Everything else is a cockpit row, keyed by the row's own id: an
        // `agent_sessions.id` for a tracked agent, `ambient:<pty>` for an
        // agent found inside a plain terminal.
        RailAgentTarget::AgentSession { .. } | RailAgentTarget::AmbientTerminal { .. } => {
            cockpit_key(db_id)
        }
    }
}

/// When this row's state ended, if it has ended.
///
/// Seeds the moment the host stamps a cockpit row's alert with, the first time
/// it sees that alert. It exists for the restart and for nothing else:
/// `status_onset` is in memory, so after one every alert is new, and stamping
/// an old finish or an old failure with the clock dates it after the stored
/// look — the finish comes back unread, the dismissed failure comes back to
/// NEEDS YOU. `ended_at` survives the restart and is the moment that alert
/// really arose.
///
/// **A launch time is not a seed.** It is the one other timestamp a row
/// carries, and seeding with it was wrong in a way that showed every day: a
/// row launched an hour ago finishes a turn *now*, and dated an hour ago that
/// finish sits before any look the person has taken since — so it reads as
/// already read and never reaches FINISHED · UNREAD. A live row gets the
/// clock, which is right: its alert did arise in this run, and the host
/// remembers it for the rest of it.
///
/// Both readers call this, so the seed cannot differ between them.
pub fn row_ended(rail: &RailAgentRow) -> Option<i64> {
    rail.ended_at.as_deref().and_then(rfc3339_ms)
}

/// What `rail` wants from a person, or `None` when it wants nothing.
///
/// A cockpit or ambient terminal agent has no `SessionHealth` at all — OxiMux
/// parses its PTY into an `AgentStatus` — so its status goes through the host's
/// status entry, which maps it to the same vocabulary every other surface uses,
/// stamps it with the moment that alert arose, and ranks it against anything
/// else published against that row.
pub fn attention_of_row(
    host: &HostCore,
    rail: &RailAgentRow,
    prefs: &PrefsSnapshot,
) -> Option<Attention> {
    let target = attention_target(&rail.db_id, &rail.target);
    // A chat session is in the registry and publishes its own health, so the
    // host already knows everything about it — including the prefs and the
    // looks that decide whether a failure has been dealt with.
    if matches!(rail.target, RailAgentTarget::Chat { .. }) {
        return host.attention_of_in(&target, prefs);
    }
    host.attention_of_agent_status_in(
        &target,
        &rail.effective_status(),
        is_completed_turn(rail),
        row_ended(rail),
        prefs,
    )
}

/// The resolver `build_agent_rows` takes.
///
/// Resolves nothing when there is no host — a window rendered before the host
/// has started. Rows then carry no attention and are sectioned by their own
/// lifecycle, which is the reading that shows no alerts rather than wrong ones.
///
/// The preferences are read **once per pass**, here, rather than per row: the
/// dashboard asks about every row it builds, and a point query each put one
/// SQLite read per row on the render path.
pub fn resolver(host: Option<Arc<HostCore>>) -> impl Fn(&RailAgentRow) -> Option<Attention> {
    let prefs = host.as_deref().map(|host| host.prefs_snapshot());
    move |rail| match (host.as_deref(), prefs.as_ref()) {
        (Some(host), Some(prefs)) => attention_of_row(host, rail, prefs),
        _ => None,
    }
}

/// Record that the person is now looking at the row they just clicked.
///
/// This is the one thing that ends an alert nobody has to answer: a failure
/// stands in NEEDS YOU until it is viewed or dismissed, and a finished turn
/// stays unread until it is read. Opening the row *is* reading it — the click
/// focuses its tab — so a row the person has opened stops asking for them.
///
/// **Only when the click actually opened something.** A row with nothing to
/// open — a history row, or a chat another agent spawned that has no tab —
/// shows the person nothing, and clearing its alert would retire the only
/// sign that it finished or failed. Both click sites test that first; an
/// explicit Dismiss (PR D) is what clears those.
///
/// Silent when there is no host: there is then no attention to clear either.
pub fn mark_row_viewed(db_id: &str, target: &RailAgentTarget, cx: &App) {
    let Some(host) = cx
        .try_global::<crate::remote_control::RemoteControl>()
        .and_then(|rc| rc.host_core())
    else {
        return;
    };
    host.mark_viewed(&attention_target(db_id, target));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rail_row(started: Option<&str>, ended: Option<&str>) -> RailAgentRow {
        RailAgentRow {
            db_id: "7".into(),
            target: RailAgentTarget::AgentSession { db_id: "7".into() },
            workspace_key: "ws".into(),
            adapter_id: "claude-code".into(),
            label: "Claude Code".into(),
            is_live: true,
            status_rx: None,
            db_status: oximux_core::AgentStatus::Idle,
            started_at: started.map(str::to_string),
            ended_at: ended.map(str::to_string),
            age_label: "now".into(),
            persisted_title: None,
            persisted_message: None,
            ambient_detail: None,
        }
    }

    #[test]
    fn only_a_row_that_has_ended_seeds_its_alerts_moment() {
        // The seed exists for the restart: `ended_at` is both a moment that
        // survives one and the moment the alert arose.
        let ended = rfc3339_ms("2026-06-23T10:00:00Z").expect("a parseable fixture");
        assert_eq!(
            row_ended(&rail_row(Some("2026-06-23T09:00:00Z"), Some("2026-06-23T10:00:00Z"))),
            Some(ended),
            "a row that has ended arose when it ended"
        );
        // The one that bit: a live row's launch is not when its next alert
        // arose. Dated at launch, a turn that finishes now sits before every
        // look taken since and is read before anyone sees it.
        assert_eq!(
            row_ended(&rail_row(Some("2026-06-23T09:00:00Z"), None)),
            None,
            "a live row has only its launch, which is not a seed"
        );
        assert_eq!(
            row_ended(&rail_row(None, None)),
            None,
            "an ambient row has neither, and the host falls back to the clock"
        );
    }

    #[test]
    fn each_kind_of_row_is_keyed_the_way_its_alert_is_published() {
        assert_eq!(
            attention_target("7", &RailAgentTarget::AgentSession { db_id: "7".into() }),
            "cockpit:7"
        );
        assert_eq!(
            attention_target(
                "ambient:pty-3",
                &RailAgentTarget::AmbientTerminal { pty_id: "pty-3".into() }
            ),
            "cockpit:ambient:pty-3",
            "an ambient row's key is its row id, which already carries the pty"
        );
        assert_eq!(
            attention_target("s1", &RailAgentTarget::Chat { session_id: "s1".into() }),
            "chat:s1",
            "a chat's key is the registry id its health is published under"
        );
    }
}
