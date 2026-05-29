//! Round-trip tests for `PaneBufferRepo` (Phase 4 step 16; F3.4 schema
//! extends with sub_pane_ordinal; V005 adds window_id keying so two
//! windows on the same project store independent scrollback rows).

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
    repo.set(&pid, "main", 1, 0, b"second tab pane bytes")
        .unwrap();
    repo.set(&pid, "main", 0, 0, b"first tab pane bytes")
        .unwrap();
    let rows = repo.get_all_for_project(&pid, "main").unwrap();
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
    repo.set(&pid, "main", 0, 2, b"third leaf").unwrap();
    repo.set(&pid, "main", 0, 0, b"first leaf").unwrap();
    repo.set(&pid, "main", 0, 1, b"second leaf").unwrap();
    let rows = repo.get_all_for_project(&pid, "main").unwrap();
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
    repo.set(&pid, "main", 0, 0, b"old").unwrap();
    repo.set(&pid, "main", 0, 0, b"new").unwrap();
    let rows = repo.get_all_for_project(&pid, "main").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, b"new".to_vec());
}

#[test]
fn delete_for_project_only_clears_target() {
    let db = open_memory().expect("open memory");
    let pid_a = seed_project(&db);
    let pid_b = seed_project(&db);
    let repo = PaneBufferRepo::new(db);
    repo.set(&pid_a, "main", 0, 0, b"A").unwrap();
    repo.set(&pid_b, "main", 0, 0, b"B").unwrap();
    repo.delete_for_project(&pid_a, "main").unwrap();
    assert!(repo.get_all_for_project(&pid_a, "main").unwrap().is_empty());
    let b_rows = repo.get_all_for_project(&pid_b, "main").unwrap();
    assert_eq!(b_rows.len(), 1);
    assert_eq!(b_rows[0].2, b"B".to_vec());
}

#[test]
fn missing_project_returns_empty() {
    let db = open_memory().expect("open memory");
    let repo = PaneBufferRepo::new(db);
    let rows = repo.get_all_for_project("nope", "main").unwrap();
    assert!(rows.is_empty());
}

// V005 window-keying tests ------------------------------------------------

/// Two windows on the same (project, ordinal) must coexist independently.
/// Writing under "w1" must not affect "main" rows and vice versa.
#[test]
fn window_id_rows_coexist_independently() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);

    repo.set(&pid, "main", 0, 0, b"main-window-data").unwrap();
    repo.set(&pid, "w1", 0, 0, b"w1-window-data").unwrap();

    let main_rows = repo.get_all_for_project(&pid, "main").unwrap();
    let w1_rows = repo.get_all_for_project(&pid, "w1").unwrap();

    assert_eq!(main_rows.len(), 1);
    assert_eq!(main_rows[0].2, b"main-window-data".to_vec());

    assert_eq!(w1_rows.len(), 1);
    assert_eq!(w1_rows[0].2, b"w1-window-data".to_vec());
}

/// get_all_for_project filters strictly by window_id — rows from other
/// windows must not appear in the result.
#[test]
fn get_all_for_project_filters_by_window_id() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);

    repo.set(&pid, "main", 0, 0, b"tab0-main").unwrap();
    repo.set(&pid, "main", 1, 0, b"tab1-main").unwrap();
    repo.set(&pid, "w1", 0, 0, b"tab0-w1").unwrap();
    repo.set(&pid, "w2", 0, 0, b"tab0-w2").unwrap();

    let main_rows = repo.get_all_for_project(&pid, "main").unwrap();
    assert_eq!(main_rows.len(), 2, "main sees only its own rows");
    assert!(main_rows.iter().all(|(_, _, b)| b.starts_with(b"tab") && {
        let s = String::from_utf8_lossy(b);
        s.ends_with("main")
    }));

    let w1_rows = repo.get_all_for_project(&pid, "w1").unwrap();
    assert_eq!(w1_rows.len(), 1, "w1 sees only its own row");
    assert_eq!(w1_rows[0].2, b"tab0-w1".to_vec());

    let w2_rows = repo.get_all_for_project(&pid, "w2").unwrap();
    assert_eq!(w2_rows.len(), 1, "w2 sees only its own row");
    assert_eq!(w2_rows[0].2, b"tab0-w2".to_vec());
}

/// delete_for_project with "w1" must leave "main" rows untouched.
#[test]
fn delete_for_project_scoped_to_window_id() {
    let db = open_memory().expect("open memory");
    let pid = seed_project(&db);
    let repo = PaneBufferRepo::new(db);

    repo.set(&pid, "main", 0, 0, b"main-buf").unwrap();
    repo.set(&pid, "w1", 0, 0, b"w1-buf").unwrap();

    repo.delete_for_project(&pid, "w1").unwrap();

    assert!(repo.get_all_for_project(&pid, "w1").unwrap().is_empty());
    let main_rows = repo.get_all_for_project(&pid, "main").unwrap();
    assert_eq!(main_rows.len(), 1, "main rows survive deletion of w1");
    assert_eq!(main_rows[0].2, b"main-buf".to_vec());
}
