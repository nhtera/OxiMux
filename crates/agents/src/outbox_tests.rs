//! Outbox tests: edge dedupe, batching, acceptance-only consume, restart.

use super::*;
use crate::envelope::ChildEventKind;
use chrono::TimeZone;

fn outbox() -> Outbox {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    Outbox::new(db.conn())
}

fn t0() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).single().unwrap()
}

fn secs(n: i64) -> DateTime<Local> {
    t0() + chrono::Duration::seconds(n)
}

fn event(child: &str, kind: ChildEventKind) -> ChildEvent {
    ChildEvent {
        kind,
        child: child.into(),
        depth: 1,
        title: format!("task {child}"),
        status: None,
        result: Some("done".into()),
        permit: None,
    }
}

fn states(ob: &Outbox, parent: &str) -> Vec<OutboxState> {
    ob.rows_for(parent).unwrap().into_iter().map(|r| r.state).collect()
}

#[test]
fn a_second_pass_over_the_same_edge_writes_nothing() {
    let ob = outbox();
    assert!(ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap());
    assert!(!ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap());
    // A new turn of the same child is a new edge.
    assert!(ob.push("p", &event("c1", ChildEventKind::Finished), "turn-2", t0()).unwrap());
    assert_eq!(ob.rows_for("p").unwrap().len(), 2);
    assert!(ob.has_edge("p", "c1", "finished", "turn-1").unwrap());
}

#[test]
fn one_drain_takes_every_pending_row_as_one_batch_in_order() {
    let ob = outbox();
    for c in ["c1", "c2", "c3"] {
        ob.push("p", &event(c, ChildEventKind::Finished), "turn-1", t0()).unwrap();
    }
    ob.push("other", &event("c9", ChildEventKind::Failed), "turn-1", t0()).unwrap();
    let batch = ob.claim("p", "host-a", t0()).unwrap().expect("a batch");
    let children: Vec<_> = batch.rows.iter().map(|r| r.child.as_str()).collect();
    assert_eq!(children, ["c1", "c2", "c3"]);
    assert_eq!(batch.key, batch_key(batch.rows.iter().map(|r| (r.id, 0))));
    // Nothing left to claim for that parent; the other parent is untouched.
    assert!(ob.claim("p", "host-a", t0()).unwrap().is_none());
    assert_eq!(states(&ob, "other"), [OutboxState::Pending]);
    assert_eq!(ob.pending_count("p").unwrap(), 3);
}

#[test]
fn the_key_depends_only_on_the_rows() {
    assert_eq!(batch_key([(3, 0), (1, 0), (2, 0)]), batch_key([(1, 0), (2, 0), (3, 0)]));
    assert_ne!(batch_key([(1, 0), (2, 0)]), batch_key([(1, 0), (2, 0), (3, 0)]));
    assert_ne!(batch_key([(1, 0)]), batch_key([(1, 1)]));
}

#[test]
fn a_row_is_consumed_on_acceptance_never_on_write() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let batch = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    assert_eq!(ob.mark_sent(batch.key).unwrap(), 1);
    assert_eq!(states(&ob, "p"), [OutboxState::Sent]);
    assert_eq!(ob.pending_count("p").unwrap(), 0);
    ob.mark_accepted(batch.key, secs(1)).unwrap();
    assert_eq!(states(&ob, "p"), [OutboxState::Accepted]);
    ob.mark_delivered(batch.key, secs(2)).unwrap();
    assert_eq!(states(&ob, "p"), [OutboxState::Delivered]);
    // Nothing moves a delivered row back.
    assert_eq!(ob.unclaim(batch.key).unwrap(), 0);
    assert_eq!(ob.mark_unconfirmed(batch.key).unwrap(), 0);
}

#[test]
fn a_write_with_no_proof_is_unconfirmed_and_not_claimable_again() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let batch = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    ob.mark_sent(batch.key).unwrap();
    ob.mark_unconfirmed(batch.key).unwrap();
    assert_eq!(states(&ob, "p"), [OutboxState::Unconfirmed]);
    assert!(ob.claim("p", "host-a", secs(600)).unwrap().is_none());
}

#[test]
fn a_cancelled_line_goes_out_again_under_a_new_key() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    ob.push("p", &event("c2", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let first = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    ob.mark_sent(first.key).unwrap();
    assert_eq!(ob.unclaim(first.key).unwrap(), 2);
    let again = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    assert_eq!(again.rows.len(), 2);
    assert_ne!(again.key, first.key, "the CLI saw the first uuid");
}

#[test]
fn an_interrupted_accepted_batch_is_rearmed_under_a_new_key() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let first = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    ob.mark_sent(first.key).unwrap();
    ob.mark_accepted(first.key, t0()).unwrap();
    assert_eq!(ob.rearm(first.key).unwrap(), 1);
    let again = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    assert_ne!(again.key, first.key);
    // Delivered rows are never rearmed.
    ob.mark_delivered(again.key, t0()).unwrap();
    assert_eq!(ob.rearm(again.key).unwrap(), 0);
}

#[test]
fn an_unreachable_parent_backs_off_exponentially() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let b = ob.claim("p", "host-a", t0()).unwrap().unwrap();
    ob.release_with_backoff(b.key, t0()).unwrap();
    assert!(ob.claim("p", "host-a", t0()).unwrap().is_none(), "not due yet");
    let b = ob.claim("p", "host-a", secs(1)).unwrap().expect("due after 1 s");
    ob.release_with_backoff(b.key, secs(1)).unwrap();
    assert!(ob.claim("p", "host-a", secs(2)).unwrap().is_none());
    assert!(ob.claim("p", "host-a", secs(3)).unwrap().is_some(), "due after 2 s");
    assert_eq!(backoff_s(1), 1);
    assert_eq!(backoff_s(3), 4);
    assert_eq!(backoff_s(30), BACKOFF_CAP_S);
}

#[test]
fn an_ended_parent_settles_what_it_was_owed() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    ob.push("p", &event("c2", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    ob.claim("p", "host-a", t0()).unwrap();
    ob.push("p", &event("c3", ChildEventKind::Failed), "turn-1", t0()).unwrap();
    assert_eq!(ob.settle_undeliverable("p").unwrap(), 3);
    assert!(states(&ob, "p").iter().all(|s| *s == OutboxState::Undeliverable));
}

/// A restart: the new instance gives back claims that were never written and
/// marks written-but-unproven lines unconfirmed, never resending them.
#[test]
fn a_new_host_takes_over_claims_without_resending_written_lines() {
    let ob = outbox();
    ob.push("p", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let held = ob.claim("p", "host-old", t0()).unwrap().unwrap();
    ob.push("q", &event("c2", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let written = ob.claim("q", "host-old", t0()).unwrap().unwrap();
    ob.mark_sent(written.key).unwrap();
    ob.push("r", &event("c3", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    let mine = ob.claim("r", "host-new", t0()).unwrap().unwrap();

    ob.take_over("host-new").unwrap();
    assert_eq!(states(&ob, "p"), [OutboxState::Pending]);
    assert_eq!(states(&ob, "q"), [OutboxState::Unconfirmed]);
    assert_eq!(states(&ob, "r"), [OutboxState::Claimed], "its own claims are kept");
    // Re-claimed after the restart, the same rows carry the same key.
    assert_eq!(ob.claim("p", "host-new", t0()).unwrap().unwrap().key, held.key);
    let _ = mine;
}

#[test]
fn a_rekeyed_parent_keeps_its_rows() {
    let db = oximux_storage::db::open_memory().unwrap();
    let ob = Outbox::new(db.conn());
    ob.push("agent-1", &event("c1", ChildEventKind::Finished), "turn-1", t0()).unwrap();
    crate::lineage::LineageStore::new(db.conn()).rekey("agent-1", "claude-abc").unwrap();
    assert_eq!(ob.pending_count("claude-abc").unwrap(), 1);
    assert_eq!(ob.parents_with_pending().unwrap(), ["claude-abc"]);
}
