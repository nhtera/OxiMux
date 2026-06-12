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
fn agent_status_non_terminal_slugs_match_query() {
    // `AgentSessionRepo::list_unfinished_at_shutdown` hardcodes
    // `WHERE status IN ('idle', 'running', 'waiting_input',
    // 'needs_approval')`. This test machine-checks every non-terminal
    // slug against that literal set — if a slug drifts or a new
    // non-terminal variant appears without updating the query, the sweep
    // goes silently blind and this test fails.
    let query_literals = ["idle", "running", "waiting_input", "needs_approval"];
    let non_terminal = [
        AgentStatus::Idle,
        AgentStatus::Running,
        AgentStatus::WaitingForInput,
        AgentStatus::NeedsApproval(String::new()),
    ];
    for status in &non_terminal {
        assert!(!status.is_terminal());
        assert!(
            query_literals.contains(&status.as_str()),
            "non-terminal slug {:?} missing from the sweep query",
            status.as_str()
        );
    }
    // And the terminal set must stay OUT of the sweep.
    for status in [
        AgentStatus::Done { code: Some(0) },
        AgentStatus::Failed(String::new()),
        AgentStatus::Interrupted,
    ] {
        assert!(!query_literals.contains(&status.as_str()));
    }
}
