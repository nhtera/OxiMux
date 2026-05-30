//! Integration tests for the V006 `worktree_settings` table via the
//! public `WorktreeSettingsRepo` surface. Covers:
//! - `get` returns `None` when no row exists (callers MUST substitute
//!   `WorktreeSettings::default()`).
//! - `upsert` + `get` round-trips every payload field.
//! - `upsert` on an existing key replaces values (no row-multiplication).
//! - `ON DELETE CASCADE` from `workspaces(id)` drops the settings row.
//! - `delete` on a missing key is a no-op (matches the repo's
//!   missing-row contract documented in `repositories/mod.rs`).

use oximux_core::WorktreeSettings;
use oximux_storage::{Db, WorktreeSettingsRepo, open_memory};

/// Insert a single project + workspace so that the FK on
/// `worktree_settings.workspace_id` resolves. Each test calls this on
/// its own fresh in-memory DB, so the hardcoded ids never collide.
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

#[test]
fn get_returns_none_for_missing_row() {
    let db = open_memory().expect("open mem");
    seed_workspace(&db, "ws-1");
    let repo = WorktreeSettingsRepo::new(db);
    assert!(repo.get("ws-1").expect("get ok").is_none());
}

#[test]
fn upsert_then_get_round_trips_all_fields() {
    let db = open_memory().expect("open mem");
    seed_workspace(&db, "ws-1");
    let repo = WorktreeSettingsRepo::new(db);

    let written = WorktreeSettings {
        base_ref: Some("origin/main".to_string()),
        commit_draft: Some("WIP: refactor the parser".to_string()),
        view_mode_override: Some("tree".to_string()),
    };
    repo.upsert("ws-1", &written).expect("upsert");

    let read_back = repo.get("ws-1").expect("get").expect("row present");
    assert_eq!(read_back, written);
}

#[test]
fn upsert_replaces_existing_row() {
    let db = open_memory().expect("open mem");
    seed_workspace(&db, "ws-1");
    let repo = WorktreeSettingsRepo::new(db);

    repo.upsert(
        "ws-1",
        &WorktreeSettings {
            base_ref: Some("origin/release".to_string()),
            ..Default::default()
        },
    )
    .expect("first upsert");
    repo.upsert(
        "ws-1",
        &WorktreeSettings {
            base_ref: Some("origin/main".to_string()),
            commit_draft: Some("draft v2".to_string()),
            ..Default::default()
        },
    )
    .expect("second upsert");

    let r = repo.get("ws-1").expect("get").expect("row");
    assert_eq!(r.base_ref.as_deref(), Some("origin/main"));
    assert_eq!(r.commit_draft.as_deref(), Some("draft v2"));
    assert!(r.view_mode_override.is_none());
}

#[test]
fn cascade_deletes_settings_when_workspace_dropped() {
    let db = open_memory().expect("open mem");
    seed_workspace(&db, "ws-1");
    let repo = WorktreeSettingsRepo::new(db.clone());

    repo.upsert(
        "ws-1",
        &WorktreeSettings {
            base_ref: Some("origin/main".to_string()),
            ..Default::default()
        },
    )
    .expect("upsert");

    db.with_conn(|c| {
        c.execute("DELETE FROM workspaces WHERE id = 'ws-1'", [])
            .map(|_| ())
    })
    .expect("drop workspace");

    assert!(
        repo.get("ws-1").expect("get").is_none(),
        "FK CASCADE must drop the settings row"
    );
}

#[test]
fn delete_missing_row_is_noop() {
    let db = open_memory().expect("open mem");
    seed_workspace(&db, "ws-1");
    let repo = WorktreeSettingsRepo::new(db);

    repo.delete("ws-1").expect("delete missing ok");
    repo.upsert("ws-1", &WorktreeSettings::default())
        .expect("upsert");
    repo.delete("ws-1").expect("delete real row ok");
    assert!(repo.get("ws-1").expect("get").is_none());
}
