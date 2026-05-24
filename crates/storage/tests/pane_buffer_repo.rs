//! Round-trip tests for `PaneBufferRepo` (Phase 4 step 16; F3.4 schema
//! extends with sub_pane_ordinal). Exercises the V002 + V004 migrations
//! + the per-project upsert / fetch / delete contract.

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
    // Insert out of order; reader should still get them sorted by
    // (ordinal, sub_pane_ordinal).
    repo.set(&pid, 1, 0, b"second tab pane bytes").unwrap();
    repo.set(&pid, 0, 0, b"first tab pane bytes").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (0, 0, b"first tab pane bytes".to_vec()));
    assert_eq!(rows[1], (1, 0, b"second tab pane bytes".to_vec()));
}

#[test]
fn sub_pane_ordinal_separates_rows_within_a_tab() {
    // F3.4: multi-sub-pane tab persists one buffer per leaf at
    // `(tab_ordinal, sub_pane_ordinal)`. Verify the unique key plus
    // composite ordering.
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    repo.set(&pid, 0, 2, b"third leaf").unwrap();
    repo.set(&pid, 0, 0, b"first leaf").unwrap();
    repo.set(&pid, 0, 1, b"second leaf").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], (0, 0, b"first leaf".to_vec()));
    assert_eq!(rows[1], (0, 1, b"second leaf".to_vec()));
    assert_eq!(rows[2], (0, 2, b"third leaf".to_vec()));
}

#[test]
fn set_upserts_on_conflict() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    repo.set(&pid, 0, 0, b"old").unwrap();
    repo.set(&pid, 0, 0, b"new").unwrap();
    let rows = repo.get_all_for_project(&pid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, b"new".to_vec());
}

#[test]
fn delete_for_project_only_clears_target() {
    let db = open_memory().expect("open memory");
    let pid_a = seed_project(&db);
    let pid_b = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    repo.set(&pid_a, 0, 0, b"A").unwrap();
    repo.set(&pid_b, 0, 0, b"B").unwrap();
    repo.delete_for_project(&pid_a).unwrap();
    assert!(repo.get_all_for_project(&pid_a).unwrap().is_empty());
    let b_rows = repo.get_all_for_project(&pid_b).unwrap();
    assert_eq!(b_rows.len(), 1);
    assert_eq!(b_rows[0].2, b"B".to_vec());
}

#[test]
fn missing_project_returns_empty() {
    let db = open_memory().expect("open memory");
    let repo = PaneBufferRepo::new(db);
    let rows = repo.get_all_for_project("nope").unwrap();
    assert!(rows.is_empty());
}
