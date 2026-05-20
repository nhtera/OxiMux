//! PaneSessionRepo integration tests — ordering, update, and the
//! `agent_session_id` SET NULL behaviour when an agent row is deleted.

use oximux_storage::{AgentSessionRepo, PaneSessionRepo, ProjectRepo, WorkspaceRepo, open_memory};
use rusqlite::params;

fn fixture() -> (
    String,
    PaneSessionRepo,
    AgentSessionRepo,
    oximux_storage::Db,
) {
    let db = open_memory().expect("open memory");
    let projects = ProjectRepo::new(db.clone());
    let workspaces = WorkspaceRepo::new(db.clone());
    let panes = PaneSessionRepo::new(db.clone());
    let agents = AgentSessionRepo::new(db.clone());

    let p = projects.insert("Acme", "/r", "main").expect("project");
    let w = workspaces
        .insert(&p.id, "F", "f", "oximux/f", "/wt/f")
        .expect("workspace");
    (w.id, panes, agents, db)
}

#[test]
fn pane_session_insert_returns_row() {
    let (workspace_id, panes, _, _) = fixture();
    let p = panes
        .insert(&workspace_id, "bash", "0,0,1,1", Some("/tmp/log"))
        .expect("insert");
    assert!(!p.id.is_empty());
    assert_eq!(p.workspace_id, workspace_id);
    assert!(p.agent_session_id.is_none());
    assert_eq!(p.shell_command, "bash");
    assert_eq!(p.grid_position, "0,0,1,1");
    assert_eq!(p.log_path.as_deref(), Some("/tmp/log"));
}

#[test]
fn pane_session_get_by_id() {
    let (workspace_id, panes, _, _) = fixture();
    let p = panes.insert(&workspace_id, "zsh", "0,0,1,1", None).unwrap();
    let fetched = panes.get_by_id(&p.id).expect("get").expect("present");
    assert_eq!(fetched, p);
}

#[test]
fn pane_session_list_for_workspace_orders_asc() {
    let (workspace_id, panes, _, _) = fixture();
    let first = panes.insert(&workspace_id, "a", "0,0,1,1", None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let second = panes.insert(&workspace_id, "b", "0,0,1,1", None).unwrap();
    let list = panes.list_for_workspace(&workspace_id).expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, first.id, "oldest first — restore order");
    assert_eq!(list[1].id, second.id);
}

#[test]
fn pane_session_update_grid_position() {
    let (workspace_id, panes, _, _) = fixture();
    let p = panes
        .insert(&workspace_id, "bash", "0,0,1,1", None)
        .unwrap();
    panes
        .update_grid_position(&p.id, "1,1,2,2")
        .expect("update");
    let after = panes.get_by_id(&p.id).expect("get").expect("present");
    assert_eq!(after.grid_position, "1,1,2,2");
}

#[test]
fn pane_session_delete() {
    let (workspace_id, panes, _, _) = fixture();
    let p = panes
        .insert(&workspace_id, "bash", "0,0,1,1", None)
        .unwrap();
    panes.delete(&p.id).expect("delete");
    assert!(panes.get_by_id(&p.id).expect("get").is_none());
}

#[test]
fn pane_agent_link_set_null_on_agent_delete() {
    let (workspace_id, panes, agents, db) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    let p = panes
        .insert(&workspace_id, "bash", "0,0,1,1", None)
        .unwrap();

    // The repo doesn't expose linking; do it via raw SQL for this test.
    db.with_conn(|c| {
        c.execute(
            "UPDATE pane_sessions SET agent_session_id = ?1 WHERE id = ?2",
            params![a.id, p.id],
        )
    })
    .expect("link");

    // Delete the agent row directly — repo doesn't expose a destructive
    // delete (cancel-only flows through the runtime); raw SQL is fine for
    // exercising the FK behaviour.
    db.with_conn(|c| c.execute("DELETE FROM agent_sessions WHERE id = ?1", [&a.id]))
        .expect("delete agent");

    let after = panes.get_by_id(&p.id).expect("get").expect("present");
    assert!(after.agent_session_id.is_none(), "FK SET NULL");
}
