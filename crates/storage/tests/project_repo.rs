//! ProjectRepo integration tests — in-memory DB, fresh per test.

use oximux_storage::{ProjectRepo, WorkspaceRepo, open_memory};

fn repo() -> (ProjectRepo, WorkspaceRepo) {
    let db = open_memory().expect("open memory");
    (ProjectRepo::new(db.clone()), WorkspaceRepo::new(db))
}

#[test]
fn project_insert_returns_full_row() {
    let (projects, _) = repo();
    let p = projects
        .insert("Acme", "/home/acme", "main")
        .expect("insert");
    assert!(!p.id.is_empty(), "id should be a UUID");
    assert_eq!(p.name, "Acme");
    assert_eq!(p.root_path, "/home/acme");
    assert_eq!(p.default_branch, "main");
    assert!(p.last_opened_at.is_none());
    assert!(!p.created_at.is_empty());
}

#[test]
fn project_get_by_id_found() {
    let (projects, _) = repo();
    let p = projects.insert("Acme", "/r1", "main").expect("insert");
    let fetched = projects.get_by_id(&p.id).expect("get").expect("present");
    assert_eq!(fetched, p);
}

#[test]
fn project_get_by_id_not_found() {
    let (projects, _) = repo();
    assert!(projects.get_by_id("nope").expect("get").is_none());
}

#[test]
fn project_list_recent_ordered_by_last_opened() {
    let (projects, _) = repo();
    let a = projects.insert("A", "/a", "main").expect("a");
    let b = projects.insert("B", "/b", "main").expect("b");
    let _c = projects.insert("C", "/c", "main").expect("c");

    // Touch B last → expect B first; A and C have no last_opened_at and
    // sort behind B but order between them is created-at DESC.
    projects.update_last_opened_at(&b.id).expect("touch b");
    let recent = projects.list_recent(10).expect("list");
    assert_eq!(recent[0].id, b.id);
    // A was inserted first → should be after C in created_at DESC.
    let a_pos = recent.iter().position(|p| p.id == a.id).unwrap();
    let c_pos = recent.iter().position(|p| p.name == "C").unwrap();
    assert!(c_pos < a_pos, "C should precede A in created_at DESC");
}

#[test]
fn project_update_last_opened_at() {
    let (projects, _) = repo();
    let p = projects.insert("A", "/a", "main").expect("insert");
    assert!(p.last_opened_at.is_none());
    projects.update_last_opened_at(&p.id).expect("touch");
    let p2 = projects.get_by_id(&p.id).expect("get").expect("present");
    assert!(p2.last_opened_at.is_some());
}

#[test]
fn project_delete_cascades_to_workspaces() {
    let (projects, workspaces) = repo();
    let p = projects.insert("A", "/a", "main").expect("project");
    workspaces
        .insert(&p.id, "feat", "feat", "oximux/feat", "/wt/feat")
        .expect("workspace");
    projects.delete(&p.id).expect("delete project");
    let remaining = workspaces.list_for_project(&p.id).expect("list");
    assert!(
        remaining.is_empty(),
        "cascade should remove child workspaces"
    );
}

#[test]
fn project_root_path_unique_conflict() {
    let (projects, _) = repo();
    projects.insert("A", "/same", "main").expect("first");
    let err = projects.insert("B", "/same", "main").expect_err("conflict");
    match err {
        oximux_storage::StorageError::Conflict { table, constraint } => {
            assert_eq!(table, "projects");
            assert_eq!(constraint, "root_path");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}
