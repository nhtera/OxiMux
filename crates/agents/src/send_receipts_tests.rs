//! Receipts: reservation, final states, take-over, prune.

use super::*;
use chrono::TimeZone;

fn receipts() -> Receipts {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    Receipts::new(db.conn())
}

fn t0() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).single().unwrap()
}

#[test]
fn a_key_seen_before_is_refused_with_its_state() {
    let r = receipts();
    let k = key_hash(KeyKind::Gate, "u-1");
    assert_eq!(r.reserve("s", &k, "host-a", t0()).unwrap(), None, "new key");
    assert_eq!(r.reserve("s", &k, "host-a", t0()).unwrap(), Some(ReceiptState::Pending));
    r.settle("s", &k, ReceiptState::Accepted, "host-a", t0()).unwrap();
    assert_eq!(r.reserve("s", &k, "host-a", t0()).unwrap(), Some(ReceiptState::Accepted), "never twice");
}

#[test]
fn accepted_and_rejected_are_final_and_late_proof_settles_unknown() {
    let r = receipts();
    let k = key_hash(KeyKind::Gate, "u-1");
    r.reserve("s", &k, "a", t0()).unwrap();
    r.settle("s", &k, ReceiptState::Accepted, "a", t0()).unwrap();
    r.settle("s", &k, ReceiptState::Unknown, "a", t0()).unwrap();
    assert_eq!(r.state("s", &k).unwrap(), Some(ReceiptState::Accepted));

    let k2 = key_hash(KeyKind::Gate, "u-2");
    r.settle("s", &k2, ReceiptState::Unknown, "a", t0()).unwrap();
    r.settle("s", &k2, ReceiptState::Accepted, "a", t0()).unwrap();
    assert_eq!(r.state("s", &k2).unwrap(), Some(ReceiptState::Accepted), "late proof wins over unknown");
}

#[test]
fn a_refusal_overrides_the_clis_acknowledgement_in_either_order() {
    let r = receipts();
    let (a, b) = (key_hash(KeyKind::Gate, "a"), key_hash(KeyKind::Gate, "b"));
    r.settle("s", &a, ReceiptState::Accepted, "h", t0()).unwrap();
    r.settle("s", &a, ReceiptState::Rejected, "h", t0()).unwrap();
    r.settle("s", &b, ReceiptState::Rejected, "h", t0()).unwrap();
    r.settle("s", &b, ReceiptState::Accepted, "h", t0()).unwrap();
    assert_eq!(r.state("s", &a).unwrap(), Some(ReceiptState::Rejected));
    assert_eq!(r.state("s", &b).unwrap(), Some(ReceiptState::Rejected));
}

#[test]
fn a_new_host_marks_what_another_left_pending_unknown() {
    let r = receipts();
    let (mine, theirs) = (key_hash(KeyKind::Gate, "m"), key_hash(KeyKind::Gate, "t"));
    r.reserve("s", &mine, "b", t0()).unwrap();
    r.reserve("s", &theirs, "a", t0()).unwrap();
    assert_eq!(r.take_over("b", t0()).unwrap(), 1);
    assert_eq!(r.state("s", &theirs).unwrap(), Some(ReceiptState::Unknown));
    assert_eq!(r.state("s", &mine).unwrap(), Some(ReceiptState::Pending));
}

#[test]
fn kinds_never_collide_and_keys_are_not_stored() {
    assert_ne!(key_hash(KeyKind::Gate, "x"), key_hash(KeyKind::V2, "x"));
    assert!(!key_hash(KeyKind::V2, "secret-key").contains("secret"));
}

#[test]
fn receipts_older_than_a_day_are_pruned() {
    let r = receipts();
    let k = key_hash(KeyKind::Gate, "old");
    r.reserve("s", &k, "a", t0()).unwrap();
    assert_eq!(r.prune(t0() + chrono::Duration::hours(23)).unwrap(), 0);
    assert_eq!(r.prune(t0() + chrono::Duration::hours(25)).unwrap(), 1);
}
