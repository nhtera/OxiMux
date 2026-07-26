//! Round-trip the SCM base-ref persistence helper against an in-memory
//! SQLite store. Verifies:
//!
//! 1. A fresh workspace returns no base ref.
//! 2. Writing a base ref creates the row.
//! 3. Re-writing a different base ref updates the row in place.
//! 4. Writing `None` clears the column without dropping the row.
//! 5. Sibling fields (`commit_draft`, `view_mode_override`) survive
//!    the base-ref write — the panel must not clobber what other
//!    surfaces own in the same row.

use oximux_app::shell::source_control::merge_base_ref_into_settings;
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
fn empty_workspace_returns_none_initially() {
    let repo = fresh_repo("ws-1");
    assert!(
        repo.get("ws-1").expect("get").is_none(),
        "no row should exist for a never-touched workspace"
    );
}

#[test]
fn write_then_read_round_trip() {
    let repo = fresh_repo("ws-1");
    merge_base_ref_into_settings(&repo, "ws-1", Some("origin/main".to_string()))
        .expect("upsert succeeds");
    let got = repo
        .get("ws-1")
        .expect("get")
        .expect("row present after upsert");
    assert_eq!(got.base_ref.as_deref(), Some("origin/main"));
}

#[test]
fn write_overwrites_previous_value() {
    let repo = fresh_repo("ws-1");
    merge_base_ref_into_settings(&repo, "ws-1", Some("origin/main".to_string())).unwrap();
    merge_base_ref_into_settings(&repo, "ws-1", Some("origin/release".to_string())).unwrap();
    let got = repo.get("ws-1").unwrap().unwrap();
    assert_eq!(got.base_ref.as_deref(), Some("origin/release"));
}

#[test]
fn write_none_clears_base_ref_but_keeps_row() {
    let repo = fresh_repo("ws-1");
    merge_base_ref_into_settings(&repo, "ws-1", Some("origin/main".to_string())).unwrap();
    merge_base_ref_into_settings(&repo, "ws-1", None).unwrap();
    let got = repo.get("ws-1").expect("get").expect("row still exists");
    assert!(got.base_ref.is_none(), "base_ref column cleared");
}

#[test]
fn merge_preserves_sibling_fields() {
    // Pre-seed commit_draft + view_mode_override (the two other columns
    // on `worktree_settings`) via a direct upsert, then change the
    // base_ref through our helper. The siblings must survive — without
    // the read-modify-write step they'd be nulled out by the upsert.
    let repo = fresh_repo("ws-1");
    let initial = WorktreeSettings {
        base_ref: None,
        commit_draft: Some("WIP: hunt down race".to_string()),
        view_mode_override: Some("tree".to_string()),
    };
    repo.upsert("ws-1", &initial).expect("seed row");

    merge_base_ref_into_settings(&repo, "ws-1", Some("origin/release".to_string()))
        .expect("base-ref upsert");

    let got = repo.get("ws-1").unwrap().unwrap();
    assert_eq!(got.base_ref.as_deref(), Some("origin/release"));
    assert_eq!(
        got.commit_draft.as_deref(),
        Some("WIP: hunt down race"),
        "commit_draft must not be clobbered",
    );
    assert_eq!(
        got.view_mode_override.as_deref(),
        Some("tree"),
        "view_mode_override must not be clobbered",
    );
}
