//! Round-trip the SCM commit_draft load helper against an in-memory
//! SQLite store. Mirrors `sc_base_ref_persistence.rs` for the
//! commit_draft column.
//!
//! The write half (debounced 250 ms upsert from the InputState change
//! subscription) lives inside `CommitArea::schedule_draft_save` and
//! requires a `Context<CommitArea>` + GPUI executor to drive — covered
//! indirectly by `crates/storage/tests/worktree_settings_roundtrip.rs`
//! which exercises `WorktreeSettingsRepo::modify(|s| s.commit_draft =
//! ...)` directly (the helper the schedule path calls). What's tested
//! here is the LOAD half: panel mount semantics for what the textarea
//! is pre-populated with.

use oximux_app::shell::source_control::load_initial_commit_draft;
use oximux_core::WorktreeSettings;
use oximux_storage::{Db, WorktreeSettingsRepo, open_memory};

/// Seed the `projects` and `workspaces` rows the FK on
/// `worktree_settings.workspace_id` resolves against. Mirrors the
/// helper in `crates/storage/tests/worktree_settings_roundtrip.rs`.
fn seed_workspace(db: &Db, workspace_id: &str) {
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO projects (id, name, root_path, default_branch, created_at) \
             VALUES ('proj-1', 'test', '/tmp/test', 'main', '0')",
            [],
        )?;
        c.execute(
            "INSERT INTO workspaces \
                 (id, project_id, name, slug, branch, worktree_path, status, created_at) \
             VALUES (?1, 'proj-1', 'ws', 'ws', 'main', '/tmp/test/ws', 'active', '0')",
            [workspace_id],
        )
    })
    .expect("seed workspace");
}

fn fresh_repo(workspace_id: &str) -> WorktreeSettingsRepo {
    let db = open_memory().expect("open in-memory storage");
    seed_workspace(&db, workspace_id);
    WorktreeSettingsRepo::new(db)
}

#[test]
fn load_returns_none_when_no_settings_repo() {
    // Test wiring (`right_sidebar::PanelConfig { worktree_settings_repo:
    // None }`) must keep the textarea functional — the load helper
    // returns None and the panel ships an empty textarea.
    assert!(load_initial_commit_draft(None, "ws-1").is_none());
}

#[test]
fn load_returns_none_for_fresh_workspace() {
    let repo = fresh_repo("ws-1");
    assert!(
        load_initial_commit_draft(Some(&repo), "ws-1").is_none(),
        "never-touched workspace ⇒ empty textarea on mount",
    );
}

#[test]
fn load_returns_none_when_row_exists_but_draft_column_is_null() {
    // The user set a base_ref but never typed in the composer.
    // The row exists but commit_draft is NULL — load must still
    // surface None (vs. e.g. defaulting to empty string).
    let repo = fresh_repo("ws-1");
    repo.upsert(
        "ws-1",
        &WorktreeSettings {
            base_ref: Some("origin/main".to_string()),
            commit_draft: None,
            view_mode_override: None,
        },
    )
    .expect("seed row");
    assert!(load_initial_commit_draft(Some(&repo), "ws-1").is_none());
}

#[test]
fn load_round_trips_persisted_draft() {
    // The user typed a multi-line draft last session; on mount the
    // textarea must show exactly that text verbatim — including
    // newlines, leading whitespace, and trailing characters.
    let repo = fresh_repo("ws-1");
    let draft = "feat(scm): commit draft persistence\n\n- 250ms debounce\n- per-worktree key";
    repo.upsert(
        "ws-1",
        &WorktreeSettings {
            base_ref: None,
            commit_draft: Some(draft.to_string()),
            view_mode_override: None,
        },
    )
    .expect("seed draft");
    let loaded = load_initial_commit_draft(Some(&repo), "ws-1")
        .expect("draft restored");
    assert_eq!(loaded, draft, "draft must round-trip byte-for-byte");
}

#[test]
fn load_uses_workspace_id_as_key() {
    // Two workspaces in the same db should each see only their own
    // draft. Sanity check that the key isn't accidentally global.
    let db = open_memory().expect("open in-memory storage");
    seed_workspace(&db, "ws-a");
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO workspaces \
                 (id, project_id, name, slug, branch, worktree_path, status, created_at) \
             VALUES ('ws-b', 'proj-1', 'wsb', 'wsb', 'main', '/tmp/test/wsb', 'active', '0')",
            [],
        )
        .map(|_| ())
    })
    .expect("seed second workspace");
    let repo = WorktreeSettingsRepo::new(db);

    repo.upsert(
        "ws-a",
        &WorktreeSettings {
            commit_draft: Some("draft for A".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    repo.upsert(
        "ws-b",
        &WorktreeSettings {
            commit_draft: Some("draft for B".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        load_initial_commit_draft(Some(&repo), "ws-a").as_deref(),
        Some("draft for A"),
    );
    assert_eq!(
        load_initial_commit_draft(Some(&repo), "ws-b").as_deref(),
        Some("draft for B"),
    );
}
