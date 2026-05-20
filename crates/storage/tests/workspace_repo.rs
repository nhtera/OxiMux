//! WorkspaceRepo integration tests — UNIQUE conflict + cascade behaviour
//! are the focus; the worktree-rollback flow is exercised at step 6.

use oximux_storage::{
    AgentSessionRepo, PaneSessionRepo, ProjectRepo, StorageError, WorkspaceRepo, open_memory,
};

fn project_and_repos() -> (String, WorkspaceRepo, PaneSessionRepo, AgentSessionRepo) {
    let db = open_memory().expect("open memory");
    let projects = ProjectRepo::new(db.clone());
    let p = projects.insert("Acme", "/r", "main").expect("project");
    (
        p.id,
        WorkspaceRepo::new(db.clone()),
        PaneSessionRepo::new(db.clone()),
        AgentSessionRepo::new(db),
    )
}

#[test]
fn workspace_insert_returns_full_row() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "Feat", "feat", "oximux/feat", "/wt/feat")
        .expect("insert");
    assert!(!w.id.is_empty());
    assert_eq!(w.project_id, project_id);
    assert_eq!(w.name, "Feat");
    assert_eq!(w.slug, "feat");
    assert_eq!(w.status, "active");
    assert!(w.archived_at.is_none());
}

#[test]
fn workspace_get_by_id() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "F", "f", "oximux/f", "/wt/f")
        .expect("insert");
    let fetched = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(fetched, w);
}

#[test]
fn workspace_list_for_project_excludes_archived() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let a = workspaces
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a")
        .expect("a");
    workspaces
        .insert(&project_id, "B", "b", "oximux/b", "/wt/b")
        .expect("b");
    workspaces.mark_archived(&a.id).expect("archive a");
    let active = workspaces.list_for_project(&project_id).expect("list");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].slug, "b");
}

#[test]
fn workspace_mark_archived_sets_timestamp_and_status() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a")
        .expect("insert");
    workspaces.mark_archived(&w.id).expect("archive");
    let after = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert!(after.archived_at.is_some());
    assert_eq!(after.status, "archived");
}

#[test]
fn workspace_rename() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "Old", "o", "oximux/o", "/wt/o")
        .expect("insert");
    workspaces.rename(&w.id, "New").expect("rename");
    let after = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(after.name, "New");
}

#[test]
fn workspace_delete_removes_row() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a")
        .expect("insert");
    workspaces.delete(&w.id).expect("delete");
    assert!(workspaces.get_by_id(&w.id).expect("get").is_none());
}

#[test]
fn workspace_unique_project_slug_conflict() {
    let (project_id, workspaces, _, _) = project_and_repos();
    workspaces
        .insert(&project_id, "A", "feat", "oximux/feat", "/wt/feat")
        .expect("first");
    let err = workspaces
        .insert(&project_id, "B", "feat", "oximux/feat2", "/wt/feat2")
        .expect_err("conflict");
    match err {
        StorageError::Conflict { table, constraint } => {
            assert_eq!(table, "workspaces");
            assert_eq!(constraint, "project_id_slug");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[test]
fn workspace_delete_cascades_to_panes_and_agents() {
    let (project_id, workspaces, panes, agents) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a")
        .expect("workspace");
    panes.insert(&w.id, "bash", "0,0,1,1", None).expect("pane");
    agents
        .insert(&w.id, "claude_code", None, None)
        .expect("agent");

    workspaces.delete(&w.id).expect("delete workspace");
    assert!(panes.list_for_workspace(&w.id).expect("list").is_empty());
    assert!(agents.list_for_workspace(&w.id).expect("list").is_empty());
}
