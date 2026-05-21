//! Round-trip tests for `PaneBufferRepo` (Phase 4 step 16). Exercises the
//! V002 migration + the per-project upsert / fetch / delete contract.

use oximux_storage::{PaneBufferRepo, ProjectRepo, open_memory};

fn seed_project(db: &oximux_storage::Db) -> String {
    let repo = ProjectRepo::new(db.clone());
    let root = format!("/tmp/{}", uuid::Uuid::new_v4());
    repo.insert("p", &root, "main").unwrap().id
}

#[test]
fn set_then_get_round_trips_bytes_in_ordinal_order() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    // Insert out of order; reader should still get them sorted by ordinal.
    repo.set(&pid, 1, b"second pane bytes").unwrap();
    repo.set(&pid, 0, b"first pane bytes").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (0, b"first pane bytes".to_vec()));
    assert_eq!(rows[1], (1, b"second pane bytes".to_vec()));
}

#[test]
fn set_upserts_on_conflict() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    repo.set(&pid, 0, b"old").unwrap();
    repo.set(&pid, 0, b"new").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, b"new".to_vec());
}

#[test]
fn delete_for_project_only_clears_target() {
    let db = open_memory().expect("open memory");
    let pid_a = seed_project(&db);
    let pid_b = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    repo.set(&pid_a, 0, b"A").unwrap();
    repo.set(&pid_b, 0, b"B").unwrap();
    repo.delete_for_project(&pid_a).unwrap();
    assert!(repo.get_all_for_project(&pid_a).unwrap().is_empty());
    let b_rows = repo.get_all_for_project(&pid_b).unwrap();
    assert_eq!(b_rows.len(), 1);
    assert_eq!(b_rows[0].1, b"B".to_vec());
}

#[test]
fn missing_project_returns_empty() {
    let db = open_memory().expect("open memory");
    let repo = PaneBufferRepo::new(db);
    let rows = repo.get_all_for_project("nope").unwrap();
    assert!(rows.is_empty());
}
