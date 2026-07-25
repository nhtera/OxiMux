//! The reconnect policy exercised purely through its public API: the backoff
//! schedule, the give-up rule (surfacing the last cause), reset-on-success, and the
//! guard that a stray loss signal can't resurrect a given-up connection.

use std::time::Duration;

use oximux_remote_session::{ConnAction, ConnState, Reconnect};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
fn fail() -> Result<(), String> {
    Err("no route".into()) // a failed dial carrying a stub cause
}

#[test]
fn begin_then_connect_settles_idle() {
    let mut rc = Reconnect::new();
    assert_eq!(rc.begin(), ConnAction::Dial);
    assert_eq!(*rc.state(), ConnState::Connecting);
    assert_eq!(rc.on_dial_result(Ok(())), ConnAction::Idle);
    assert_eq!(*rc.state(), ConnState::Connected);
}

#[test]
fn a_lost_connection_reconnects_immediately() {
    let mut rc = Reconnect::new();
    rc.begin();
    rc.on_dial_result(Ok(()));
    assert_eq!(rc.on_lost(), ConnAction::Dial, "first reconnect is immediate, no wait");
    assert_eq!(*rc.state(), ConnState::Connecting);
}

#[test]
fn repeated_failures_back_off_then_give_up_with_the_cause() {
    let mut rc = Reconnect::new(); // 3 retries, 1s base
    rc.begin();
    // 1s, 2s, 4s across the three retries.
    assert_eq!(rc.on_dial_result(fail()), ConnAction::Wait(secs(1)));
    assert_eq!(*rc.state(), ConnState::WaitingToRetry { attempt: 1, delay: secs(1) });
    assert_eq!(rc.on_retry_elapsed(), ConnAction::Dial);
    assert_eq!(rc.on_dial_result(fail()), ConnAction::Wait(secs(2)));
    assert_eq!(rc.on_retry_elapsed(), ConnAction::Dial);
    assert_eq!(rc.on_dial_result(fail()), ConnAction::Wait(secs(4)));
    assert_eq!(rc.on_retry_elapsed(), ConnAction::Dial);
    // Fourth failure exhausts the budget, surfacing the last cause.
    assert_eq!(rc.on_dial_result(fail()), ConnAction::GiveUp);
    assert_eq!(*rc.state(), ConnState::Unreachable { cause: "no route".into() });
}

#[test]
fn a_success_mid_backoff_resets_the_budget() {
    let mut rc = Reconnect::new();
    rc.begin();
    assert_eq!(rc.on_dial_result(fail()), ConnAction::Wait(secs(1)));
    rc.on_retry_elapsed();
    assert_eq!(rc.on_dial_result(fail()), ConnAction::Wait(secs(2)));
    rc.on_retry_elapsed();
    // Connect on the third dial → the budget resets...
    assert_eq!(rc.on_dial_result(Ok(())), ConnAction::Idle);
    // ...so a later loss starts the schedule over at 1s, not 4s.
    rc.on_lost();
    assert_eq!(rc.on_dial_result(fail()), ConnAction::Wait(secs(1)));
}

#[test]
fn a_lost_signal_from_a_dead_state_is_ignored() {
    let mut rc = Reconnect::with_budget(0, secs(1));
    rc.begin();
    assert_eq!(rc.on_dial_result(fail()), ConnAction::GiveUp);
    // Unreachable can only be left via begin(), never a stray loss signal.
    assert_eq!(rc.on_lost(), ConnAction::Idle);
    assert_eq!(*rc.state(), ConnState::Unreachable { cause: "no route".into() });
}
