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
fn workspace_set_linked_issue_round_trips() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "Fix", "fix", "oximux/fix", "/wt/fix")
        .expect("insert");
    // Fresh inserts have no linked issue.
    assert!(w.linked_issue.is_none());

    workspaces
        .set_linked_issue(&w.id, Some("#42"))
        .expect("set linked issue");
    let fetched = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(fetched.linked_issue.as_deref(), Some("#42"));
    // It also surfaces through the project listing.
    let listed = workspaces.list_for_project(&project_id).expect("list");
    assert_eq!(listed[0].linked_issue.as_deref(), Some("#42"));

    // Clearing it round-trips back to None.
    workspaces.set_linked_issue(&w.id, None).expect("clear");
    let cleared = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert!(cleared.linked_issue.is_none());
}

#[test]
fn workspace_set_tint_round_trips() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "Tint", "tint", "oximux/tint", "/wt/tint")
        .expect("insert");
    assert!(w.tint.is_none());

    workspaces.set_tint(&w.id, Some("blue")).expect("set tint");
    let fetched = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(fetched.tint.as_deref(), Some("blue"));
    // The live rail render reads via list_for_project — verify it carries tint.
    let listed = workspaces.list_for_project(&project_id).expect("list");
    assert_eq!(listed[0].tint.as_deref(), Some("blue"));

    workspaces.set_tint(&w.id, None).expect("clear");
    let cleared = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert!(cleared.tint.is_none());
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

// V013 dropped the agent_sessions → workspaces FK (synthesized
// 'primary:<project_id>' ids broke it), so deleting a workspace cascades
// to panes only; agent-session history rows survive as accepted orphans.
#[test]
fn workspace_delete_cascades_to_panes_but_keeps_agent_history() {
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
    assert_eq!(agents.list_for_workspace(&w.id).expect("list").len(), 1);
}
