//! Round-trip tests for `PaneRelayIdRepo` (Phase 5 step 5). Exercises
//! V003 migration + the per-project + per-session contract.

use oximux_storage::{PaneRelayIdRepo, ProjectRepo, open_memory};

fn seed_project(db: &oximux_storage::Db) -> String {
    let repo = ProjectRepo::new(db.clone());
    let root = format!("/tmp/{}", uuid::Uuid::new_v4());
    repo.insert("p", &root, "main").unwrap().id
}

#[test]
fn set_then_get_round_trips_in_ordinal_order() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneRelayIdRepo::new(db);
    repo.set(&pid, 1, "pty-second", "sess-1").unwrap();
    repo.set(&pid, 0, "pty-first", "sess-1").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (0, "pty-first".into(), "sess-1".into()));
    assert_eq!(rows[1], (1, "pty-second".into(), "sess-1".into()));
}

#[test]
fn set_upserts_relay_pty_id_and_session_on_conflict() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneRelayIdRepo::new(db);
    repo.set(&pid, 0, "pty-old", "sess-old").unwrap();
    repo.set(&pid, 0, "pty-new", "sess-new").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], (0, "pty-new".into(), "sess-new".into()));
}

#[test]
fn delete_for_project_only_clears_target() {
    let db = open_memory().expect("open memory");
    let pid_a = seed_project(&db);
    let pid_b = seed_project(&db);
    let repo = PaneRelayIdRepo::new(db);
    repo.set(&pid_a, 0, "pty-a", "sess-1").unwrap();
    repo.set(&pid_b, 0, "pty-b", "sess-1").unwrap();
    repo.delete_for_project(&pid_a).unwrap();
    assert!(repo.get_all_for_project(&pid_a).unwrap().is_empty());
    assert_eq!(repo.get_all_for_project(&pid_b).unwrap().len(), 1);
}

#[test]
fn delete_for_session_bulk_clears_across_projects() {
    // The whole point of `delete_for_session`: when the relay daemon
    // restarts, every row tied to the old session becomes garbage.
    // Verify it sweeps across project boundaries in one call.
    let db = open_memory().expect("open memory");
    let pid_a = seed_project(&db);
    let pid_b = seed_project(&db);
    let repo = PaneRelayIdRepo::new(db);
    repo.set(&pid_a, 0, "pty-1", "sess-OLD").unwrap();
    repo.set(&pid_a, 1, "pty-2", "sess-OLD").unwrap();
    repo.set(&pid_b, 0, "pty-3", "sess-OLD").unwrap();
    repo.set(&pid_b, 1, "pty-4", "sess-NEW").unwrap();

    repo.delete_for_session("sess-OLD").unwrap();

    assert!(repo.get_all_for_project(&pid_a).unwrap().is_empty());
    let b = repo.get_all_for_project(&pid_b).unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0], (1, "pty-4".into(), "sess-NEW".into()));
}

#[test]
fn project_delete_cascades_to_relay_ids() {
    // V001 + V003 FK with ON DELETE CASCADE: deleting a project should
    // sweep its pane_relay_ids in one statement. Catches schema
    // regressions where the cascade gets dropped from V003.
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let relay_repo = PaneRelayIdRepo::new(db.clone());
    relay_repo.set(&pid, 0, "pty-x", "sess-1").unwrap();
    // Sanity.
    assert_eq!(relay_repo.get_all_for_project(&pid).unwrap().len(), 1);

    let project_repo = ProjectRepo::new(db);
    project_repo.delete(&pid).expect("delete project");

    assert!(relay_repo.get_all_for_project(&pid).unwrap().is_empty());
}

#[test]
fn missing_project_returns_empty() {
    let db = open_memory().expect("open memory");
    let repo = PaneRelayIdRepo::new(db);
    let rows = repo.get_all_for_project("nope").unwrap();
    assert!(rows.is_empty());
}
