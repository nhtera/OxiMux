use super::*;

const NOW: i64 = 1_000_000_000;
const MIN: i64 = 60_000;

fn live() -> HealthInputs {
    HealthInputs { process: Process::Live, last_activity_at: NOW, ..Default::default() }
}

fn health(i: &HealthInputs) -> SessionHealth {
    compute(i, NOW, &HealthConfig::default())
}

#[test]
fn a_live_session_with_nothing_running_is_idle() {
    let h = health(&live());
    assert_eq!((h.status, h.liveness), (HealthStatus::Idle, Liveness::Live));
    assert!(h.reason.is_none() && h.stalled_since.is_none());
}

#[test]
fn a_running_turn_is_working() {
    let h = health(&HealthInputs { turn_active: true, ..live() });
    assert_eq!(h.status, HealthStatus::Working);
}

#[test]
fn a_continuation_after_the_turn_ended_is_working_not_idle() {
    let h = health(&HealthInputs { continuations_open: 1, ..live() });
    assert_eq!(h.status, HealthStatus::Working);
}

#[test]
fn an_unanswered_send_is_working_not_idle() {
    let h = health(&HealthInputs { pending_unanswered_send: true, ..live() });
    assert_eq!(h.status, HealthStatus::Working);
    assert!(h.pending_unanswered_send);
}

#[test]
fn an_exit_without_a_stop_is_a_failure_with_its_code() {
    let h = health(&HealthInputs { process: Process::Exited(ExitProof::ExitCode(137)), ..live() });
    assert_eq!(h.status, HealthStatus::Failed);
    assert_eq!(h.liveness, Liveness::Exited(ExitProof::ExitCode(137)));
    assert_eq!(h.reason.as_deref(), Some("agent process exited (code 137)"));
    assert!(h.exited_unexpectedly);
}

#[test]
fn an_exit_with_no_code_says_so_without_one() {
    let h = health(&HealthInputs { process: Process::Exited(ExitProof::ConnectionClosed), ..live() });
    assert_eq!(h.reason.as_deref(), Some("agent process exited"));
    assert_eq!(h.status, HealthStatus::Failed);
}

#[test]
fn an_exit_after_a_stop_is_an_interruption() {
    let h = health(&HealthInputs {
        interrupted: true,
        process: Process::Exited(ExitProof::ExitCode(143)),
        ..live()
    });
    assert_eq!(h.status, HealthStatus::Interrupted);
    assert_eq!(h.liveness, Liveness::Exited(ExitProof::UserStop));
    assert!(!h.exited_unexpectedly, "a Stop is not a crash");
}

#[test]
fn a_stop_while_the_process_lives_is_an_interruption() {
    let h = health(&HealthInputs { interrupted: true, turn_active: true, ..live() });
    assert_eq!((h.status, h.liveness), (HealthStatus::Interrupted, Liveness::Live));
}

#[test]
fn a_dormant_chat_is_unverifiable_never_failed() {
    let h = health(&HealthInputs { process: Process::Dormant, ..live() });
    assert_eq!((h.status, h.liveness), (HealthStatus::Idle, Liveness::Unverifiable));
    assert!(!h.exited_unexpectedly);
}

#[test]
fn a_restored_row_reads_restored_unconfirmed() {
    let h = health(&HealthInputs {
        process: Process::Dormant,
        restored: Some(Restored { was_mid_turn: false, prior_host_gone: true }),
        ..live()
    });
    assert_eq!(h.liveness, Liveness::RestoredUnconfirmed);
    assert_eq!(h.status, HealthStatus::Idle);
}

#[test]
fn a_restored_mid_turn_row_with_the_lock_proof_is_interrupted() {
    let h = health(&HealthInputs {
        process: Process::Dormant,
        restored: Some(Restored { was_mid_turn: true, prior_host_gone: true }),
        ..live()
    });
    assert_eq!(h.status, HealthStatus::Interrupted);
    assert_eq!(h.liveness, Liveness::Exited(ExitProof::HostLock));
}

#[test]
fn a_restored_mid_turn_row_without_proof_stays_unconfirmed() {
    let h = health(&HealthInputs {
        process: Process::Dormant,
        restored: Some(Restored { was_mid_turn: true, prior_host_gone: false }),
        ..live()
    });
    assert_eq!(h.liveness, Liveness::RestoredUnconfirmed);
    assert_ne!(h.status, HealthStatus::Interrupted, "no proof, no claim");
}

#[test]
fn a_pending_permission_outranks_a_running_turn() {
    let h = health(&HealthInputs { turn_active: true, prompt: Some(PromptKind::Permission), ..live() });
    assert_eq!(h.status, HealthStatus::NeedsPermission);
    let h = health(&HealthInputs { turn_active: true, prompt: Some(PromptKind::Question), ..live() });
    assert_eq!(h.status, HealthStatus::NeedsAnswer);
}

#[test]
fn an_armed_rate_limit_retry_is_limited_until_it_fires() {
    let h = health(&HealthInputs { limited_until: Some(NOW + 12 * MIN), ..live() });
    assert_eq!(h.status, HealthStatus::Limited);
    assert_eq!(h.limited_until, Some(NOW + 12 * MIN));
}

#[test]
fn limited_with_an_unanswered_send_reads_limited_not_idle() {
    let h = health(&HealthInputs {
        limited_until: Some(NOW + MIN),
        pending_unanswered_send: true,
        ..live()
    });
    assert_eq!(h.status, HealthStatus::Limited);
}

#[test]
fn a_failed_turn_with_nothing_running_is_failed() {
    let h = health(&HealthInputs { last_turn_failure: Some("API Error: 500".into()), ..live() });
    assert_eq!(h.status, HealthStatus::Failed);
    assert_eq!(h.reason.as_deref(), Some("API Error: 500"));
    assert!(!h.exited_unexpectedly, "the process is still there");
    // A new turn running supersedes it.
    let h = health(&HealthInputs { last_turn_failure: Some("x".into()), turn_active: true, ..live() });
    assert_eq!(h.status, HealthStatus::Working);
}

#[test]
fn ten_minutes_of_silence_while_working_is_a_stall_never_a_death() {
    let quiet = HealthInputs { turn_active: true, last_activity_at: NOW - 10 * MIN, ..live() };
    let h = health(&quiet);
    assert_eq!(h.status, HealthStatus::Working);
    assert_eq!(h.stalled_since, Some(NOW - 10 * MIN));
    assert_eq!(h.liveness, Liveness::Live);
    // Even a day of silence never exits anything.
    let h = health(&HealthInputs { last_activity_at: NOW - 24 * 60 * MIN, ..quiet });
    assert_eq!((h.status, h.liveness), (HealthStatus::Working, Liveness::Live));
}

#[test]
fn just_under_the_threshold_is_not_stalled_and_activity_clears_it() {
    let h = health(&HealthInputs { turn_active: true, last_activity_at: NOW - 10 * MIN + 1, ..live() });
    assert!(h.stalled_since.is_none());
    let h = health(&HealthInputs { turn_active: true, last_activity_at: NOW, ..live() });
    assert!(h.stalled_since.is_none());
}

#[test]
fn an_idle_session_is_never_stalled() {
    let h = health(&HealthInputs { last_activity_at: NOW - 60 * MIN, ..live() });
    assert!(h.stalled_since.is_none());
}

#[test]
fn a_zero_threshold_turns_stalls_off() {
    let cfg = HealthConfig { stall_after_ms: 0, ..Default::default() };
    let h = compute(&HealthInputs { turn_active: true, last_activity_at: 0, ..live() }, NOW, &cfg);
    assert!(h.stalled_since.is_none());
}

#[test]
fn an_accepted_send_with_no_turn_for_two_minutes_says_so_and_stays_working() {
    let i = HealthInputs {
        pending_unanswered_send: true,
        accepted_unstarted_since: Some(NOW - 2 * MIN),
        ..live()
    };
    let h = health(&i);
    assert_eq!(h.status, HealthStatus::Working);
    assert_eq!(h.reason.as_deref(), Some("Accepted, not started yet"));
    let h = health(&HealthInputs { accepted_unstarted_since: Some(NOW - MIN), ..i });
    assert!(h.reason.is_none(), "not yet");
}

#[test]
fn edge_identities_and_counts_pass_through() {
    let h = health(&HealthInputs {
        turn_id: Some("inc-1:7".into()),
        exit_seq: Some("inc-1:2".into()),
        held_restored: 3,
        ..live()
    });
    assert_eq!(h.turn_id.as_deref(), Some("inc-1:7"));
    assert_eq!(h.exit_seq.as_deref(), Some("inc-1:2"));
    assert_eq!(h.held_restored, 3);
}

#[test]
fn a_suspend_shows_as_wall_time_the_monotonic_clock_never_saw() {
    // Ticking every 30 s. A suspend stops the monotonic clock: the tick sees
    // 30 s pass while the wall clock moved 30 minutes.
    assert!(slept(30_000, 30 * MIN + 30_000, 30_000));
    // App Nap delays the tick itself.
    assert!(slept(5 * MIN, 5 * MIN, 30_000));
    // An ordinary tick, and one a little late, are not sleep.
    assert!(!slept(30_000, 30_050, 30_000));
    assert!(!slept(55_000, 55_000, 30_000));
}

#[test]
fn a_restored_chat_on_a_live_connection_runs_its_turn_but_is_unconfirmed() {
    let h = health(&HealthInputs {
        turn_active: true,
        restored: Some(Restored { was_mid_turn: true, prior_host_gone: true }),
        ..live()
    });
    assert_eq!((h.status, h.liveness), (HealthStatus::Working, Liveness::RestoredUnconfirmed));
}

#[test]
fn a_limited_session_says_why() {
    let h = health(&HealthInputs {
        limited_until: Some(NOW + MIN),
        limit_reason: Some("Provider overloaded".into()),
        ..live()
    });
    assert_eq!(h.reason.as_deref(), Some("Provider overloaded"));
}

#[test]
fn liveness_wire_names_are_stable() {
    assert_eq!(Liveness::Live.wire_name(), "live");
    assert_eq!(Liveness::Unverifiable.wire_name(), "unverifiable");
    assert_eq!(Liveness::RestoredUnconfirmed.wire_name(), "restoredUnconfirmed");
    assert_eq!(Liveness::Exited(ExitProof::HostLock).wire_name(), "exited");
}
