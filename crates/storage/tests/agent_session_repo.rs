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
fn agent_session_title_round_trips_and_starts_none() {
    let (workspace_id, _, agents) = fixture();
    let a = agents
        .insert(&workspace_id, "claude_code", None, None)
        .unwrap();
    // A fresh session has no title.
    assert!(a.title.is_none());
    assert!(
        agents
            .get_by_id(&a.id)
            .unwrap()
            .unwrap()
            .title
            .is_none()
    );
    // Persist a title; it survives a re-read (the restart path).
    agents
        .update_title(&a.id, "refactor the parser")
        .expect("update title");
    let after = agents.get_by_id(&a.id).expect("get").expect("present");
    assert_eq!(after.title.as_deref(), Some("refactor the parser"));
    // A later prompt overwrites it.
    agents.update_title(&a.id, "now write tests").unwrap();
    assert_eq!(
        agents.get_by_id(&a.id).unwrap().unwrap().title.as_deref(),
        Some("now write tests")
    );
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
fn agent_session_list_unfinished_at_shutdown_filters_correctly() {
    let (workspace_id, _, agents) = fixture();

    let running = agents
        .insert(&workspace_id, "claude_code", None, None)
        .unwrap();
    agents
        .update_status(&running.id, &AgentStatus::Running)
        .expect("set running");

    // An agent parked at its prompt decays Running → Idle before a
    // crash/teardown; the sweep must rescue it too (insert leaves the row
    // at the default `idle` slug with no ended_at).
    let decayed_idle = agents
        .insert(&workspace_id, "claude_code", None, None)
        .unwrap();

    let done = agents.insert(&workspace_id, "codex", None, None).unwrap();
    agents
        .update_status(&done.id, &AgentStatus::Done { code: Some(0) })
        .expect("set done");
    agents.update_ended_at(&done.id).expect("end done");

    let interrupted = agents.insert(&workspace_id, "pi", None, None).unwrap();
    agents
        .update_status(&interrupted.id, &AgentStatus::Interrupted)
        .expect("set interrupted");

    let shutdown = agents.list_unfinished_at_shutdown().expect("list");
    let ids: Vec<&str> = shutdown.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(shutdown.len(), 2);
    assert!(ids.contains(&running.id.as_str()));
    assert!(ids.contains(&decayed_idle.id.as_str()));
}

// V013 dropped the workspaces FK (synthesized 'primary:<project_id>' ids
// have no workspaces row, so the FK kept the persistence path dead).
// Session rows now survive workspace deletion as accepted orphans.
#[test]
fn agent_session_survives_workspace_delete() {
    let (workspace_id, workspaces, agents) = fixture();
    let a = agents.insert(&workspace_id, "codex", None, None).unwrap();
    workspaces.delete(&workspace_id).expect("delete workspace");
    assert!(agents.get_by_id(&a.id).expect("get").is_some());
}

// The reason V013 exists: a synthesized primary-workspace id (no
// workspaces row backing it) must be insertable.
#[test]
fn agent_session_insert_accepts_synthesized_primary_id() {
    let (_workspace_id, _workspaces, agents) = fixture();
    let a = agents
        .insert("primary:some-project-uuid", "claude-code", None, None)
        .expect("insert with synthesized workspace id");
    let got = agents.get_by_id(&a.id).expect("get").expect("row");
    assert_eq!(got.workspace_id, "primary:some-project-uuid");
}
