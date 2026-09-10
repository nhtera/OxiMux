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
        .insert(&project_id, "Feat", "feat", "oximux/feat", "/wt/feat", true)
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
        .insert(&project_id, "F", "f", "oximux/f", "/wt/f", true)
        .expect("insert");
    let fetched = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(fetched, w);
}

#[test]
fn workspace_set_linked_issue_round_trips() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "Fix", "fix", "oximux/fix", "/wt/fix", true)
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
        .insert(&project_id, "Tint", "tint", "oximux/tint", "/wt/tint", true)
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
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a", true)
        .expect("a");
    workspaces
        .insert(&project_id, "B", "b", "oximux/b", "/wt/b", true)
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
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a", true)
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
        .insert(&project_id, "Old", "o", "oximux/o", "/wt/o", true)
        .expect("insert");
    workspaces.rename(&w.id, "New").expect("rename");
    let after = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(after.name, "New");
}

#[test]
fn workspace_delete_removes_row() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a", true)
        .expect("insert");
    workspaces.delete(&w.id).expect("delete");
    assert!(workspaces.get_by_id(&w.id).expect("get").is_none());
}

#[test]
fn workspace_unique_project_slug_conflict() {
    let (project_id, workspaces, _, _) = project_and_repos();
    workspaces
        .insert(&project_id, "A", "feat", "oximux/feat", "/wt/feat", true)
        .expect("first");
    let err = workspaces
        .insert(&project_id, "B", "feat", "oximux/feat2", "/wt/feat2", true)
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
        .insert(&project_id, "A", "a", "oximux/a", "/wt/a", true)
        .expect("workspace");
    panes.insert(&w.id, "bash", "0,0,1,1", None).expect("pane");
    agents
        .insert(&w.id, "claude_code", None, None)
        .expect("agent");

    workspaces.delete(&w.id).expect("delete workspace");
    assert!(panes.list_for_workspace(&w.id).expect("list").is_empty());
    assert_eq!(agents.list_for_workspace(&w.id).expect("list").len(), 1);
}

// Archive is a reversible visibility flip, not a delete: the row moves between
// `list_for_project` and `list_archived_for_project`, and — the point of this
// test — every user-set field survives the round trip. The visibility flip is
// the easy half; a restore that silently dropped the tint or the phase would
// still pass a listing-only assertion.
#[test]
fn workspace_archive_unarchive_round_trip_preserves_every_field() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let w = workspaces
        .insert(&project_id, "Feat", "feat", "oximux/feat", "/wt/feat", true)
        .expect("insert");
    // Set every field a user can change, so the restore has something to lose.
    workspaces.set_tint(&w.id, Some("blue")).expect("tint");
    workspaces.set_pinned(&w.id, true).expect("pin");
    workspaces.set_sort_order(&w.id, 42.5).expect("sort order");
    workspaces.set_comment(&w.id, "needs review").expect("comment");
    workspaces.set_phase(&w.id, "in_review").expect("phase");
    workspaces
        .set_linked_issue(&w.id, Some("#7"))
        .expect("linked issue");
    let before = workspaces.get_by_id(&w.id).expect("get").expect("present");

    workspaces.mark_archived(&w.id).expect("archive");
    assert!(
        workspaces
            .list_for_project(&project_id)
            .expect("list active")
            .is_empty(),
        "archived row must vanish from the active listing"
    );
    let archived = workspaces
        .list_archived_for_project(&project_id)
        .expect("list archived");
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, w.id);
    assert_eq!(archived[0].status, "archived");
    assert!(archived[0].archived_at.is_some());

    workspaces.unarchive(&w.id).expect("unarchive");
    assert!(
        workspaces
            .list_archived_for_project(&project_id)
            .expect("list archived")
            .is_empty(),
        "restored row must vanish from the archived listing"
    );
    let after = workspaces.get_by_id(&w.id).expect("get").expect("present");
    assert_eq!(after.status, "active");
    assert!(after.archived_at.is_none());
    // Everything the user set is still there.
    assert_eq!(after.name, before.name);
    assert_eq!(after.slug, before.slug);
    assert_eq!(after.branch, before.branch);
    assert_eq!(after.worktree_path, before.worktree_path);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.tint.as_deref(), Some("blue"));
    assert!(after.pinned);
    assert_eq!(after.sort_order, 42.5);
    assert_eq!(after.comment, "needs review");
    assert_eq!(after.phase, "in_review");
    assert_eq!(after.linked_issue.as_deref(), Some("#7"));
    // And it is back in the active listing, unchanged.
    let listed = workspaces.list_for_project(&project_id).expect("list active");
    assert_eq!(listed, vec![after]);
}

// Archived listings are per-project: one project's archived rows must never
// leak into another's group header count.
#[test]
fn workspace_list_archived_is_scoped_to_its_project() {
    let db = open_memory().expect("open memory");
    let projects = ProjectRepo::new(db.clone());
    let a = projects.insert("A", "/a", "main").expect("project a");
    let b = projects.insert("B", "/b", "main").expect("project b");
    let workspaces = WorkspaceRepo::new(db);
    let wa = workspaces
        .insert(&a.id, "A1", "a1", "oximux/a1", "/wt/a1", true)
        .expect("insert a1");
    workspaces
        .insert(&b.id, "B1", "b1", "oximux/b1", "/wt/b1", true)
        .expect("insert b1");
    workspaces.mark_archived(&wa.id).expect("archive");

    let archived_a = workspaces
        .list_archived_for_project(&a.id)
        .expect("list archived a");
    assert_eq!(archived_a.len(), 1);
    assert_eq!(archived_a[0].id, wa.id);
    assert!(
        workspaces
            .list_archived_for_project(&b.id)
            .expect("list archived b")
            .is_empty()
    );
}

/// **The minted-vs-adopted flag survives a round trip.** Every path that
/// removes a worktree reads it to decide whether removing the branch is
/// cleanup or data loss, so a value that did not persist would silently pick
/// the destructive answer.
#[test]
fn branch_minted_round_trips_in_both_states() {
    let (project_id, workspaces, _, _) = project_and_repos();
    let minted = workspaces
        .insert(&project_id, "Mine", "mine", "oximux/mine", "/wt/mine", true)
        .expect("insert minted");
    let adopted = workspaces
        .insert(&project_id, "Theirs", "theirs", "feature/api/retry", "/wt/theirs", false)
        .expect("insert adopted");
    assert!(minted.branch_minted, "the returned row must carry it");
    assert!(!adopted.branch_minted);

    // And it comes back off disk, which is the half that actually matters.
    assert!(
        workspaces.get_by_id(&minted.id).expect("get").expect("row").branch_minted,
        "a minted branch read back as adopted would never be cleaned up"
    );
    assert!(
        !workspaces.get_by_id(&adopted.id).expect("get").expect("row").branch_minted,
        "an adopted branch read back as minted would be force-deleted"
    );
    // Listings too — the rail and the delete flow both read through these.
    let listed = workspaces.list_for_project(&project_id).expect("list");
    let by_slug = |s: &str| listed.iter().find(|w| w.slug == s).expect("listed").branch_minted;
    assert!(by_slug("mine"));
    assert!(!by_slug("theirs"));
}
