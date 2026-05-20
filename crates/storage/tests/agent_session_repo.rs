//! AgentSessionRepo integration tests — status codec round-trip + shutdown
//! query semantics + FK cascade.

use oximux_core::AgentStatus;
use oximux_storage::{AgentSessionRepo, ProjectRepo, WorkspaceRepo, open_memory};

fn fixture() -> (String, WorkspaceRepo, AgentSessionRepo) {
    let db = open_memory().expect("open memory");
    let projects = ProjectRepo::new(db.clone());
    let workspaces = WorkspaceRepo::new(db.clone());
    let agents = AgentSessionRepo::new(db);

    let p = projects.insert("Acme", "/r", "main").expect("project");
    let w = workspaces
        .insert(&p.id, "F", "f", "oximux/f", "/wt/f")
        .expect("workspace");
    (w.id, workspaces, agents)
}

#[test]
fn agent_session_insert_starts_idle() {
    let (workspace_id, _, agents) = fixture();
    let a = agents
        .insert(&workspace_id, "claude_code", Some("sonnet"), None)
        .expect("insert");
    assert_eq!(a.status, AgentStatus::Idle);
    assert_eq!(a.adapter_id, "claude_code");
    assert_eq!(a.model.as_deref(), Some("sonnet"));
    assert!(a.effort.is_none());
    assert!(a.started_at.is_some());
    assert!(a.ended_at.is_none());
}

#[test]
fn agent_session_get_by_id() {
    let (workspace_id, _, agents) = fixture();
    let a = agents
        .insert(&workspace_id, "codex", None, None)
        .expect("insert");
    let fetched = agents.get_by_id(&a.id).expect("get").expect("present");
    assert_eq!(fetched, a);
}

#[test]
fn agent_session_list_for_workspace_orders_desc() {
    let (workspace_id, _, agents) = fixture();
    let _a1 = agents
        .insert(&workspace_id, "claude_code", None, None)
        .unwrap();
    // RFC3339 includes nanosecond resolution → DESC is meaningful.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let a2 = agents.insert(&workspace_id, "codex", None, None).unwrap();
    let list = agents.list_for_workspace(&workspace_id).expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, a2.id, "newest first");
}

#[test]
fn agent_session_update_status_done_with_code() {
    let (workspace_id, _, agents) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    agents
        .update_status(&a.id, &AgentStatus::Done { code: Some(0) })
        .expect("update");
    let after = agents.get_by_id(&a.id).expect("get").expect("present");
    assert_eq!(after.status, AgentStatus::Done { code: Some(0) });
}

#[test]
fn agent_session_update_status_needs_approval_round_trip() {
    let (workspace_id, _, agents) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    let reason = "Approve dangerous shell command?";
    agents
        .update_status(&a.id, &AgentStatus::NeedsApproval(reason.into()))
        .expect("update");
    let after = agents.get_by_id(&a.id).expect("get").expect("present");
    assert_eq!(after.status, AgentStatus::NeedsApproval(reason.into()));
}

#[test]
fn agent_session_update_status_interrupted_round_trip() {
    let (workspace_id, _, agents) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    agents
        .update_status(&a.id, &AgentStatus::Interrupted)
        .expect("update");
    let after = agents.get_by_id(&a.id).expect("get").expect("present");
    assert_eq!(after.status, AgentStatus::Interrupted);
}

#[test]
fn agent_session_update_ended_at() {
    let (workspace_id, _, agents) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    assert!(a.ended_at.is_none());
    agents.update_ended_at(&a.id).expect("end");
    let after = agents.get_by_id(&a.id).expect("get").expect("present");
    assert!(after.ended_at.is_some());
}

#[test]
fn agent_session_list_running_at_shutdown_filters_correctly() {
    let (workspace_id, _, agents) = fixture();

    let running = agents
        .insert(&workspace_id, "claude_code", None, None)
        .unwrap();
    agents
        .update_status(&running.id, &AgentStatus::Running)
        .expect("set running");

    let done = agents.insert(&workspace_id, "codex", None, None).unwrap();
    agents
        .update_status(&done.id, &AgentStatus::Done { code: Some(0) })
        .expect("set done");
    agents.update_ended_at(&done.id).expect("end done");

    let interrupted = agents.insert(&workspace_id, "aider", None, None).unwrap();
    agents
        .update_status(&interrupted.id, &AgentStatus::Interrupted)
        .expect("set interrupted");

    let shutdown = agents.list_running_at_shutdown().expect("list");
    assert_eq!(shutdown.len(), 1);
    assert_eq!(shutdown[0].id, running.id);
}

#[test]
fn agent_session_cascade_delete_on_workspace() {
    let (workspace_id, workspaces, agents) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    workspaces.delete(&workspace_id).expect("delete workspace");
    assert!(agents.get_by_id(&a.id).expect("get").is_none());
}
