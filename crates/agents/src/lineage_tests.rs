//! Lineage store tests: the P1 Critical rows that live in the store.

use super::*;
use chrono::TimeZone;

fn store() -> LineageStore {
    let db = oximux_storage::db::open_memory().expect("open in-memory db");
    LineageStore::new(db.conn())
}

fn t0() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).single().unwrap()
}

fn secs(n: i64) -> DateTime<Local> {
    t0() + chrono::Duration::seconds(n)
}

/// A clock past the grace a freshly active row gets.
fn settled() -> DateTime<Local> {
    secs(ACTIVE_GRACE_S + 1)
}

fn spec(request: &str) -> SpawnSpec {
    SpawnSpec {
        request_id: request.into(),
        origin: SpawnOrigin::Agent,
        posture: SpawnPosture::Default,
        notify_parent: NotifyParent::Auto,
        allow_parent_permits: false,
        preassigned_id: None,
        fingerprint_input: format!("claude|prompt for {request}"),
    }
}

fn caps() -> SpawnCaps {
    SpawnCaps::default()
}

/// Every session counts as working: nothing gets reconciled to parked.
fn all_working(_: &str) -> bool {
    true
}

fn none_working(_: &str) -> bool {
    false
}

fn parent(id: &str) -> SpawnParent {
    SpawnParent::Session(id.into())
}

/// Reserve + finalize one child of `of`; returns its session id.
fn spawn(s: &LineageStore, of: &str, request: &str, child: &str) -> String {
    match s.reserve(&parent(of), &spec(request), caps(), &all_working, t0()).unwrap() {
        Ok(Reservation::Fresh { row_id, .. }) => s.finalize(row_id, child, t0()).unwrap(),
        other => panic!("expected a fresh reservation, got {other:?}"),
    }
    child.to_string()
}

fn spawn_at(s: &LineageStore, of: &str, request: &str, child: &str, at: DateTime<Local>) {
    match s.reserve(&parent(of), &spec(request), caps(), &all_working, at).unwrap() {
        Ok(Reservation::Fresh { row_id, .. }) => s.finalize(row_id, child, at).unwrap(),
        other => panic!("expected a fresh reservation, got {other:?}"),
    }
}

fn refusal(s: &LineageStore, of: &str, request: &str, caps: SpawnCaps) -> SpawnRefusal {
    s.reserve(&parent(of), &spec(request), caps, &all_working, t0())
        .unwrap()
        .expect_err("expected a refusal")
}

#[test]
fn a_root_child_is_depth_one_under_the_parent_as_root() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    let row = s.row("c1").unwrap().unwrap();
    assert_eq!((row.depth, row.root_id.as_str(), row.parent_id.as_str()), (1, "root", "root"));
    assert_eq!(row.state, LineageState::Active);
    assert_eq!(s.depth_of("root").unwrap(), 0);
}

#[test]
fn default_depth_one_refuses_a_grandchild_and_two_allows_it() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    let r = refusal(&s, "c1", "r2", caps());
    assert_eq!(r, SpawnRefusal::Depth { depth: 2, max: 1 });
    assert!(r.to_string().ends_with("Complete this task yourself."));
    let deep = SpawnCaps { depth: 2, ..caps() };
    match s.reserve(&parent("c1"), &spec("r2"), deep, &all_working, t0()).unwrap() {
        Ok(Reservation::Fresh { row_id, depth, .. }) => {
            assert_eq!(depth, 2);
            s.finalize(row_id, "g1", t0()).unwrap();
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(refusal(&s, "g1", "r3", deep), SpawnRefusal::Depth { depth: 3, max: 2 });
}

#[test]
fn a_stored_depth_above_the_ceiling_clamps_to_two() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    let wild = SpawnCaps { depth: 5, ..caps() };
    let row_id = match s.reserve(&parent("c1"), &spec("r2"), wild, &all_working, t0()).unwrap() {
        Ok(Reservation::Fresh { row_id, .. }) => row_id,
        other => panic!("{other:?}"),
    };
    s.finalize(row_id, "g1", t0()).unwrap();
    assert_eq!(refusal(&s, "g1", "r3", wild), SpawnRefusal::Depth { depth: 3, max: 2 });
}

#[test]
fn the_ninth_active_child_is_refused_with_the_fanout_hint() {
    let s = store();
    for i in 0..8 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i}"));
    }
    let r = refusal(&s, "root", "r8", caps());
    assert_eq!(r, SpawnRefusal::Fanout { active: 8, limit: 8 });
    assert!(r.to_string().contains("Wait on a child, or cancel one."));
}

#[test]
fn finished_children_park_and_free_their_slot() {
    let s = store();
    for i in 0..8 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i}"));
        assert!(s.park(&format!("c{i}"), settled()).unwrap());
    }
    spawn(&s, "root", "r8", "c8");
    assert_eq!(s.row("c8").unwrap().unwrap().state, LineageState::Active);
}

#[test]
fn reserve_reconciles_idle_active_rows_to_parked() {
    // A crash left eight rows `active` for sessions that are idle now.
    let s = store();
    for i in 0..8 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i}"));
    }
    // Within the grace a just-started child counts as working whatever it says.
    let early = s.reserve(&parent("root"), &spec("r8"), caps(), &none_working, t0()).unwrap();
    assert_eq!(early, Err(SpawnRefusal::Fanout { active: 8, limit: 8 }));
    match s.reserve(&parent("root"), &spec("r8"), caps(), &none_working, settled()).unwrap() {
        Ok(Reservation::Fresh { .. }) => {}
        other => panic!("reconcile should have freed the slots: {other:?}"),
    }
    assert_eq!(s.row("c0").unwrap().unwrap().state, LineageState::Parked);
}

#[test]
fn reviving_a_parked_child_at_the_cap_needs_no_slot() {
    let s = store();
    for i in 0..8 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i}"));
    }
    s.park("c0", settled()).unwrap();
    spawn(&s, "root", "r8", "c8"); // takes the freed slot: 8 active again
    assert!(s.activate("c0", t0()).unwrap(), "revive is allowed over the cap");
    // Nine working now; the next spawn waits until the count drops.
    assert_eq!(refusal(&s, "root", "r9", caps()), SpawnRefusal::Fanout { active: 9, limit: 8 });
}

#[test]
fn the_seventeenth_open_child_is_refused_with_parked_ids() {
    let s = store();
    for i in 0..16 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i:02}"));
        s.park(&format!("c{i:02}"), settled()).unwrap();
    }
    match refusal(&s, "root", "r16", caps()) {
        SpawnRefusal::Open { open, limit, parked_ids } => {
            assert_eq!((open, limit), (16, 16));
            assert_eq!(parked_ids.len(), 8, "at most eight ids are listed");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_lifetime_budget_counts_ended_children() {
    let s = store();
    let tight = SpawnCaps { lifetime_per_root: 3, ..caps() };
    for i in 0..3 {
        let row_id = match s.reserve(&parent("root"), &spec(&format!("r{i}")), tight, &all_working, t0()).unwrap() {
            Ok(Reservation::Fresh { row_id, .. }) => row_id,
            other => panic!("{other:?}"),
        };
        s.finalize(row_id, &format!("c{i}"), t0()).unwrap();
        s.mark_ended(&format!("c{i}"), false, t0()).unwrap();
    }
    assert_eq!(refusal(&s, "root", "r3", tight), SpawnRefusal::Budget { spawned: 3, limit: 3 });
}

#[test]
fn a_replayed_request_returns_the_same_child() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    match s.reserve(&parent("root"), &spec("r1"), caps(), &all_working, t0()).unwrap() {
        Ok(Reservation::Replayed(row)) => assert_eq!(row.session_id.as_deref(), Some("c1")),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.children("root", false).unwrap().len(), 1, "no duplicate child");
}

#[test]
fn a_reused_request_id_for_a_different_spawn_conflicts() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    let mut other = spec("r1");
    other.fingerprint_input = "codex|a different prompt".into();
    let r = s.reserve(&parent("root"), &other, caps(), &all_working, t0()).unwrap();
    assert_eq!(r, Err(SpawnRefusal::IdempotencyConflict));
}

#[test]
fn a_crash_between_launch_and_finalize_is_reclaimed_on_replay() {
    let s = store();
    // Reserved, then the host died before finalize.
    let first = s.reserve(&parent("root"), &spec("r1"), caps(), &all_working, t0()).unwrap();
    assert!(matches!(first, Ok(Reservation::Fresh { .. })));
    // While the launch could still be in flight, a replay waits.
    let early = s.reserve(&parent("root"), &spec("r1"), caps(), &all_working, secs(5)).unwrap();
    assert_eq!(early, Err(SpawnRefusal::InProgress));
    // A slow launch (two minutes) is still in flight, not reclaimed.
    let slow = s.reserve(&parent("root"), &spec("r1"), caps(), &all_working, secs(120)).unwrap();
    assert_eq!(slow, Err(SpawnRefusal::InProgress));
    // Another spawn's sweep must not reclaim it either while it is young.
    spawn_at(&s, "root", "r2", "c2", secs(130));
    let still = s.reserve(&parent("root"), &spec("r1"), caps(), &all_working, secs(140)).unwrap();
    assert_eq!(still, Err(SpawnRefusal::InProgress));
    // Past the launch timeout the stale reservation is reclaimed: one child.
    let later = s.reserve(&parent("root"), &spec("r1"), caps(), &all_working, secs(301)).unwrap();
    let Ok(Reservation::Fresh { row_id, .. }) = later else { panic!("{later:?}") };
    s.finalize(row_id, "c1", secs(302)).unwrap();
    assert_eq!(s.children("root", false).unwrap().len(), 2, "c1 once, plus c2");
}

#[test]
fn concurrent_reservations_at_seven_active_admit_exactly_one() {
    // Two connections on one file (the cross-process case the immediate
    // transaction guards), racing for the eighth slot.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oximux.db");
    let a = LineageStore::new(oximux_storage::db::open(&path).unwrap().conn());
    let b = LineageStore::new(oximux_storage::db::open(&path).unwrap().conn());
    for i in 0..7 {
        spawn(&a, "root", &format!("r{i}"), &format!("c{i}"));
    }
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let race = |s: LineageStore, req: &'static str, bar: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            bar.wait();
            s.reserve(&parent("root"), &spec(req), caps(), &all_working, t0()).unwrap()
        })
    };
    let ha = race(a.clone(), "ra", barrier.clone());
    let hb = race(b, "rb", barrier);
    let results = [ha.join().unwrap(), hb.join().unwrap()];
    let won = results.iter().filter(|r| matches!(r, Ok(Reservation::Fresh { .. }))).count();
    assert_eq!(won, 1, "exactly one wins the last slot: {results:?}");
}

#[test]
fn a_failed_launch_releases_its_slot_immediately() {
    let s = store();
    for i in 0..7 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i}"));
    }
    let Ok(Reservation::Fresh { row_id, .. }) =
        s.reserve(&parent("root"), &spec("r7"), caps(), &all_working, t0()).unwrap()
    else {
        panic!()
    };
    s.rollback(row_id, "binary not found", t0()).unwrap();
    spawn(&s, "root", "r8", "c8");
}

#[test]
fn three_failed_launches_trip_the_breaker_until_the_cooldown() {
    let s = store();
    for i in 0..3 {
        let Ok(Reservation::Fresh { row_id, .. }) =
            s.reserve(&parent("root"), &spec(&format!("f{i}")), caps(), &all_working, secs(i)).unwrap()
        else {
            panic!()
        };
        s.rollback(row_id, "exit 127", secs(i)).unwrap();
    }
    let r = s.reserve(&parent("root"), &spec("f3"), caps(), &all_working, secs(10)).unwrap();
    match r {
        Err(SpawnRefusal::StartFailing { count, ref last_reason }) => {
            assert_eq!((count, last_reason.as_str()), (3, "exit 127"));
            assert!(r.unwrap_err().to_string().contains("fix the cause or complete the task yourself"));
        }
        other => panic!("{other:?}"),
    }
    // After the cooldown one probe goes through; its success clears the count.
    let Ok(Reservation::Fresh { row_id, .. }) =
        s.reserve(&parent("root"), &spec("f4"), caps(), &all_working, secs(70)).unwrap()
    else {
        panic!()
    };
    s.finalize(row_id, "ok1", secs(71)).unwrap();
    spawn(&s, "root", "f5", "ok2");
    assert!(s.children("root", false).unwrap().len() == 2, "no leftover reservations");
}

#[test]
fn the_breaker_cannot_be_laundered_by_fresh_request_ids() {
    // Same counter whatever the request id: it is keyed by parent.
    let s = store();
    for i in 0..3 {
        let Ok(Reservation::Fresh { row_id, .. }) =
            s.reserve(&parent("p"), &spec(&format!("id-{i}")), caps(), &all_working, t0()).unwrap()
        else {
            panic!()
        };
        s.rollback(row_id, "boom", t0()).unwrap();
    }
    assert!(matches!(refusal(&s, "p", "brand-new", caps()), SpawnRefusal::StartFailing { .. }));
}

#[test]
fn boot_reconcile_deletes_reservations_and_parks_active_rows() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    let _ = s.reserve(&parent("root"), &spec("r2"), caps(), &all_working, t0()).unwrap();
    s.boot_reconcile(t0()).unwrap();
    assert_eq!(s.row("c1").unwrap().unwrap().state, LineageState::Parked);
    assert!(s.find_by_request("root", "r2").unwrap().is_none(), "the reservation is gone");
}

#[test]
fn quitting_never_ends_rows() {
    // Quit = boot reconcile on the next start; parked rows stay parked.
    let s = store();
    for i in 0..3 {
        spawn(&s, "root", &format!("r{i}"), &format!("c{i}"));
        s.park(&format!("c{i}"), settled()).unwrap();
    }
    s.boot_reconcile(secs(100)).unwrap();
    assert!(s.children("root", false).unwrap().iter().all(|r| r.state == LineageState::Parked));
}

#[test]
fn cancel_with_cascade_ends_the_subtree_deepest_first() {
    let s = store();
    let deep = SpawnCaps { depth: 2, ..caps() };
    spawn(&s, "root", "r1", "c1");
    let Ok(Reservation::Fresh { row_id, .. }) =
        s.reserve(&parent("c1"), &spec("r2"), deep, &all_working, t0()).unwrap()
    else {
        panic!()
    };
    s.finalize(row_id, "g1", t0()).unwrap();
    let ended = s.mark_ended("c1", true, t0()).unwrap();
    assert_eq!(ended, vec!["g1".to_string(), "c1".to_string()]);
    assert!(s.row("g1").unwrap().unwrap().ended_at.is_some());
}

#[test]
fn cancelling_a_run_ends_every_role_and_its_helpers() {
    let s = store();
    let run = SpawnParent::Run("team:run-1".into());
    let mut role = spec("planner");
    role.origin = SpawnOrigin::Team;
    let Ok(Reservation::Fresh { row_id, depth, root_id }) =
        s.reserve(&run, &role, caps(), &all_working, t0()).unwrap()
    else {
        panic!()
    };
    assert_eq!((depth, root_id.as_str()), (1, "team:run-1"));
    s.finalize(row_id, "role-1", t0()).unwrap();
    let ended = s.end_run("team:run-1", t0()).unwrap();
    assert_eq!(ended, vec!["role-1".to_string()]);
}

#[test]
fn a_roles_helper_is_depth_two_and_refused_at_the_default() {
    let s = store();
    let mut role = spec("impl");
    role.origin = SpawnOrigin::Team;
    let Ok(Reservation::Fresh { row_id, .. }) =
        s.reserve(&SpawnParent::Run("team:r".into()), &role, caps(), &all_working, t0()).unwrap()
    else {
        panic!()
    };
    s.finalize(row_id, "role", t0()).unwrap();
    assert_eq!(refusal(&s, "role", "helper", caps()), SpawnRefusal::Depth { depth: 2, max: 1 });
}

#[test]
fn team_roles_are_exempt_from_agent_caps_but_bounded_by_the_run_lane() {
    let s = store();
    let lane = SpawnCaps { run_lane: 2, ..caps() };
    let run = SpawnParent::Run("team:r".into());
    for i in 0..2 {
        let mut role = spec(&format!("role{i}"));
        role.origin = SpawnOrigin::Team;
        let Ok(Reservation::Fresh { row_id, .. }) = s.reserve(&run, &role, lane, &all_working, t0()).unwrap() else {
            panic!()
        };
        s.finalize(row_id, &format!("role{i}"), t0()).unwrap();
    }
    let mut third = spec("role2");
    third.origin = SpawnOrigin::Team;
    assert!(matches!(
        s.reserve(&run, &third, lane, &all_working, t0()).unwrap(),
        Err(SpawnRefusal::Open { open: 2, limit: 2, .. })
    ));
}

#[test]
fn a_rekey_keeps_lineage_children_and_outbox_rows() {
    let s = store();
    spawn(&s, "root", "r1", "placeholder");
    {
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_event_outbox (parent_session, child, kind, edge_id, payload, created_at) \
             VALUES ('root', 'placeholder', 'done', '1', '{}', ?1)",
            params![t0().to_rfc3339()],
        )
        .unwrap();
    }
    s.rekey("placeholder", "real-id").unwrap();
    let row = s.row("real-id").unwrap().unwrap();
    assert_eq!(row.parent_id, "root");
    assert!(s.row("placeholder").unwrap().is_none());
    assert!(s.is_descendant("root", "real-id").unwrap());
    let child: String = s
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT child FROM agent_event_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(child, "real-id");
}

#[test]
fn rekeying_a_parent_moves_its_children_and_tree() {
    let s = store();
    spawn(&s, "tmp-root", "r1", "c1");
    s.rekey("tmp-root", "root").unwrap();
    let row = s.row("c1").unwrap().unwrap();
    assert_eq!((row.parent_id.as_str(), row.root_id.as_str()), ("root", "root"));
}

#[test]
fn a_user_root_is_its_own_root_once_finalized() {
    let s = store();
    let mut op = spec("op1");
    op.origin = SpawnOrigin::User;
    let Ok(Reservation::Fresh { row_id, depth, .. }) =
        s.reserve(&SpawnParent::UserRoot, &op, caps(), &all_working, t0()).unwrap()
    else {
        panic!()
    };
    assert_eq!(depth, 0);
    s.finalize(row_id, "u1", t0()).unwrap();
    let row = s.row("u1").unwrap().unwrap();
    assert_eq!((row.parent_id.as_str(), row.root_id.as_str()), (USER_ROOT, "u1"));
    // Its own children hang under it at depth 1.
    spawn(&s, "u1", "r1", "c1");
    assert_eq!(s.row("c1").unwrap().unwrap().root_id, "u1");
}

#[test]
fn descendants_are_reachable_and_strangers_are_not() {
    let s = store();
    spawn(&s, "a", "r1", "a1");
    spawn(&s, "b", "r1", "b1");
    assert!(s.is_descendant("a", "a1").unwrap());
    assert!(!s.is_descendant("a", "b1").unwrap());
    assert!(!s.is_descendant("a1", "a").unwrap(), "a parent is not its child's descendant");
}

#[test]
fn posture_notify_and_permits_are_stored_on_the_row() {
    let s = store();
    let mut review = spec("rev");
    review.posture = SpawnPosture::Review;
    review.notify_parent = NotifyParent::Stuck;
    review.allow_parent_permits = true;
    review.preassigned_id = Some("pre-1".into());
    let Ok(Reservation::Fresh { row_id, .. }) =
        s.reserve(&parent("root"), &review, caps(), &all_working, t0()).unwrap()
    else {
        panic!()
    };
    // A pre-assigned id is visible while still reserved.
    assert_eq!(s.find_by_request("root", "rev").unwrap().unwrap().session_id.as_deref(), Some("pre-1"));
    s.finalize(row_id, "pre-1", t0()).unwrap();
    let row = s.row("pre-1").unwrap().unwrap();
    assert_eq!(row.posture, SpawnPosture::Review);
    assert_eq!(row.notify_parent, NotifyParent::Stuck);
    assert!(row.allow_parent_permits);
}

#[test]
fn ending_a_dormant_child_needs_no_session() {
    // Answered from the table alone: nothing to open.
    let s = store();
    spawn(&s, "root", "r1", "c1");
    s.park("c1", settled()).unwrap();
    assert_eq!(s.mark_ended("c1", false, t0()).unwrap(), vec!["c1".to_string()]);
    assert_eq!(s.row("c1").unwrap().unwrap().state, LineageState::Ended);
}

#[test]
fn a_just_started_child_is_not_parked_by_an_idle_report() {
    let s = store();
    spawn(&s, "root", "r1", "c1");
    assert!(!s.park("c1", secs(5)).unwrap(), "inside the grace");
    assert!(s.park("c1", settled()).unwrap());
}

#[test]
fn a_cancelled_parent_cannot_spawn() {
    let s = store();
    let deep = SpawnCaps { depth: 2, ..caps() };
    spawn(&s, "root", "r1", "c1");
    s.mark_ended("c1", false, t0()).unwrap();
    let r = s.reserve(&parent("c1"), &spec("r2"), deep, &all_working, t0()).unwrap();
    assert_eq!(r, Err(SpawnRefusal::ParentEnded));
}

#[test]
fn a_late_finalize_cannot_take_over_a_reused_slot() {
    // The slow launch's reservation is reclaimed and another parent reserves;
    // ids are never reused, so the late finalize fails instead of hijacking.
    let s = store();
    let Ok(Reservation::Fresh { row_id: slow, .. }) =
        s.reserve(&parent("a"), &spec("slow"), caps(), &all_working, t0()).unwrap()
    else {
        panic!()
    };
    let Ok(Reservation::Fresh { row_id: fresh, .. }) =
        s.reserve(&parent("a"), &spec("slow"), caps(), &all_working, secs(RESERVATION_TIMEOUT_S + 1)).unwrap()
    else {
        panic!()
    };
    assert_ne!(slow, fresh, "a reclaimed id is not handed out again");
    assert!(s.finalize(slow, "late-child", secs(400)).is_err(), "the slow launch lost its slot");
    s.finalize(fresh, "c1", secs(400)).unwrap();
    assert!(s.row("late-child").unwrap().is_none());
}
