//! One rail row's attention, resolved through the host.
//!
//! `build_agent_rows` takes this as a closure so it stays pure and testable
//! without a window. This module is where the kinds of row are told apart, and
//! it is deliberately neither in `left_rail/mod.rs` nor in the dashboard's
//! render path: both are already past their size budget, and this is logic
//! rather than layout.

use std::sync::Arc;

use oximux_agents::attention::Attention;
use oximux_agents::session_prefs::cockpit_key;
use oximux_remote_host::HostCore;

use crate::shell::left_rail::rail_agent_row::RailAgentRow;
use crate::shell::left_rail::workspace_agent_rows::is_completed_turn;
use crate::shell::search_palette::recency::{now_ms, rfc3339_ms};

/// What `rail` wants from a person, or `None` when it wants nothing.
///
/// A cockpit or ambient terminal agent has no `SessionHealth` at all — OxiMux
/// parses its PTY into an `AgentStatus` — so its status goes through the host's
/// status entry, which maps it to the same vocabulary every other surface uses
/// and ranks it against anything else published against that row.
pub fn attention_of_row(host: &HostCore, rail: &RailAgentRow) -> Option<Attention> {
    // The row's own last activity, never the clock: with the key it is the
    // alert's edge identity, and the producer publishes the same moment, so a
    // clock here would make one alert look like two.
    let since = rail
        .ended_at
        .as_deref()
        .or(rail.started_at.as_deref())
        .and_then(rfc3339_ms)
        .unwrap_or_else(now_ms);
    host.attention_of_agent_status(
        &cockpit_key(&rail.db_id),
        &rail.effective_status(),
        is_completed_turn(rail),
        since,
    )
}

/// The resolver `build_agent_rows` takes.
///
/// Resolves nothing when there is no host — a window rendered before the host
/// has started. Rows then carry no attention and are sectioned by their own
/// lifecycle, which is the reading that shows no alerts rather than wrong ones.
pub fn resolver(host: Option<Arc<HostCore>>) -> impl Fn(&RailAgentRow) -> Option<Attention> {
    move |rail| host.as_deref().and_then(|host| attention_of_row(host, rail))
}
