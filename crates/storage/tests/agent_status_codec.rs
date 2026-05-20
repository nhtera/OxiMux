//! `AgentStatus` storage-codec round-trips: every variant must survive
//! `as_str()` + `exit_code_for_storage()` + `detail_for_storage()` →
//! `from_row(…)`. Unknown slug → None (caller degrades to Interrupted).

use oximux_core::AgentStatus;

fn round_trip(input: AgentStatus) {
    let slug = input.as_str();
    let code = input.exit_code_for_storage();
    let detail = input.detail_for_storage().map(str::to_string);
    let decoded = AgentStatus::from_row(slug, code, detail).expect("decodes");
    assert_eq!(decoded, input, "round-trip failed for {input:?}");
}

#[test]
fn agent_status_idle_round_trip() {
    round_trip(AgentStatus::Idle);
}

#[test]
fn agent_status_running_round_trip() {
    round_trip(AgentStatus::Running);
}

#[test]
fn agent_status_waiting_input_round_trip() {
    round_trip(AgentStatus::WaitingForInput);
}

#[test]
fn agent_status_needs_approval_round_trip() {
    round_trip(AgentStatus::NeedsApproval("Approve X?".into()));
}

#[test]
fn agent_status_done_with_code_round_trip() {
    round_trip(AgentStatus::Done { code: Some(0) });
    round_trip(AgentStatus::Done { code: Some(137) });
}

#[test]
fn agent_status_done_no_code_round_trip() {
    round_trip(AgentStatus::Done { code: None });
}

#[test]
fn agent_status_failed_round_trip() {
    round_trip(AgentStatus::Failed("boom".into()));
}

#[test]
fn agent_status_interrupted_round_trip() {
    round_trip(AgentStatus::Interrupted);
}

#[test]
fn agent_status_unknown_slug_returns_none() {
    assert!(AgentStatus::from_row("future_variant", None, None).is_none());
}

#[test]
fn agent_status_running_slug_matches_query_literal() {
    // `AgentSessionRepo::list_running_at_shutdown` hardcodes
    // `WHERE status = 'running'`. This test machine-checks the slug
    // matches — if `Running.as_str()` ever returns anything other than
    // "running", the query goes silent and this test fails.
    assert_eq!(AgentStatus::Running.as_str(), "running");
}
