//! Retry rows: a durable attempt count, one live retry per session, pruning.

use super::*;
use chrono::TimeZone;

fn store() -> RetryStore {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    RetryStore::new(db.conn())
}

fn t0() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).single().unwrap()
}

#[test]
fn the_attempt_count_survives_re_arming() {
    let s = store();
    assert_eq!(s.arm("s", "turn-a", "window", "Usage limit reached", 1_000, "h", t0()).unwrap(), 0);
    assert_eq!(s.fired("s", "turn-a").unwrap(), 1);
    // A restart re-arms the same turn: the count carries on.
    assert_eq!(s.arm("s", "turn-a", "window", "Usage limit reached", 2_000, "h2", t0()).unwrap(), 1);
    assert_eq!(s.live("s").unwrap().unwrap().next_wake_at_ms, Some(2_000));
    assert_eq!(s.attempts("s", "turn-a").unwrap(), 1);
}

#[test]
fn a_new_turn_supersedes_the_live_retry() {
    let s = store();
    s.arm("s", "turn-a", "overload", "Provider overloaded", 1_000, "h", t0()).unwrap();
    s.arm("s", "turn-b", "overload", "Provider overloaded", 2_000, "h", t0()).unwrap();
    assert_eq!(s.live("s").unwrap().unwrap().turn_key, "turn-b", "one live retry per session");
}

#[test]
fn a_cancelled_retry_counts_from_zero_when_armed_again() {
    let s = store();
    s.arm("s", "t", "window", "r", 1, "h", t0()).unwrap();
    s.fired("s", "t").unwrap();
    assert!(s.resolve("s", Resolution::Cancelled, t0()).unwrap());
    assert!(s.live("s").unwrap().is_none());
    assert_eq!(s.arm("s", "t", "window", "r", 2, "h", t0()).unwrap(), 0);
}

#[test]
fn a_live_row_abandoned_for_a_week_is_pruned() {
    let s = store();
    s.arm("s", "t", "window", "r", 1, "h", t0()).unwrap();
    assert_eq!(s.prune(t0() + chrono::Duration::days(6)).unwrap(), 0);
    assert_eq!(s.prune(t0() + chrono::Duration::days(8)).unwrap(), 1);
}

#[test]
fn resolved_rows_are_pruned_after_a_day_and_a_fresh_live_one_is_kept() {
    let s = store();
    s.arm("s", "done", "window", "r", 1, "h", t0()).unwrap();
    s.resolve("s", Resolution::Succeeded, t0()).unwrap();
    s.arm("s", "live", "window", "r", 1, "h", t0()).unwrap();
    let later = t0() + chrono::Duration::hours(30);
    assert_eq!(s.prune(later).unwrap(), 1);
    assert!(s.live("s").unwrap().is_some());
}
