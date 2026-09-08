//! The quit-and-relaunch half of a workspace rename.
//!
//! Moving the directory and the branch is only half of "atomic in effect".
//! `worktree_path` is also a **join key**: the persisted pane blob names it, and
//! left alone the next launch restores tabs at a path that no longer exists,
//! `get_by_worktree_path` finds nothing so agent sessions never re-attach, and
//! the row renders idle. Nothing errors — the state just quietly disagrees with
//! itself.
//!
//! These tests drive the rewrite against a real `SettingsRepo` and read the
//! result back out of it, so the guarantee is asserted across the actual store
//! and serialization rather than only at the pure-function level. (The restore
//! side deserializes directly rather than through `load_persisted_tabs`, whose
//! extra job — legacy-key fallback and corrupt-payload handling — is not what
//! is under test here.)

use oximux_app::session_restore::persisted_terminals::{
    PersistedAgentTab, PersistedLeafTab, PersistedSubPane, PersistedTab, PersistedTabs,
    settings_key,
};
use oximux_app::shell::workspace_ops::repoint_persisted_tabs_in;
use oximux_core::AgentAdapter;
use oximux_storage::{SettingsRepo, open_memory};

const PROJECT: &str = "proj_abc";
const OLD: &str = "/wt/fix-lgoin";
const NEW: &str = "/wt/fix-login";

fn agent_tab(worktree: &str) -> PersistedAgentTab {
    PersistedAgentTab {
        adapter: AgentAdapter::ClaudeCode,
        adapter_id: "claude-code".to_string(),
        worktree_path: worktree.to_string(),
        model: None,
        effort: None,
        relay_external_id: None,
        relay_session: None,
        profile: None,
    }
}

/// One agent tab in the worktree plus one terminal that had `cd`-ed into a
/// subdirectory of it.
fn snapshot_in(worktree: &str) -> PersistedTabs {
    PersistedTabs {
        tabs: vec![
            PersistedTab {
                agent: Some(agent_tab(worktree)),
                sub_panes: vec![PersistedSubPane {
                    cwd: Some(worktree.to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            PersistedTab {
                sub_panes: vec![PersistedSubPane {
                    cwd: Some(format!("{worktree}/src")),
                    tabs: vec![PersistedLeafTab {
                        cwd: Some(format!("{worktree}/tests")),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

fn write_snapshot(repo: &SettingsRepo, window_id: &str, snap: &PersistedTabs) {
    repo.set(
        &settings_key(PROJECT, window_id),
        &serde_json::to_string(snap).expect("serialize"),
    )
    .expect("write snapshot");
}

/// Read a snapshot back out of the store the way a relaunch would.
fn restored(repo: &SettingsRepo, window_id: &str) -> PersistedTabs {
    let raw = repo
        .get(&settings_key(PROJECT, window_id))
        .expect("get")
        .expect("snapshot present");
    serde_json::from_str(&raw).expect("stored snapshot must still deserialize")
}

#[test]
fn a_relaunch_after_a_rename_restores_tabs_in_the_new_directory() {
    let db = open_memory().expect("open memory");
    let repo = SettingsRepo::new(db);
    write_snapshot(&repo, "main", &snapshot_in(OLD));

    assert_eq!(repoint_persisted_tabs_in(&repo, PROJECT, OLD, NEW), 1);

    // Read back out of the store, not from the value we wrote.
    let tabs = restored(&repo, "main");
    assert_eq!(
        tabs.tabs[0].agent.as_ref().unwrap().worktree_path,
        NEW,
        "the agent tab must respawn in the renamed worktree, and its \
         worktree_path is what re-attaches the session"
    );
    assert_eq!(tabs.tabs[0].sub_panes[0].cwd.as_deref(), Some(NEW));
    // A shell that had cd'd deeper keeps its position relative to the worktree.
    assert_eq!(
        tabs.tabs[1].sub_panes[0].cwd.as_deref(),
        Some("/wt/fix-login/src")
    );
    assert_eq!(
        tabs.tabs[1].sub_panes[0].tabs[0].cwd.as_deref(),
        Some("/wt/fix-login/tests")
    );
}

/// A rename in one window must not leave another window's saved layout pointing
/// at the dead path — every key under the project's prefix is rewritten.
#[test]
fn every_windows_saved_layout_is_repointed_not_just_the_active_one() {
    let db = open_memory().expect("open memory");
    let repo = SettingsRepo::new(db);
    write_snapshot(&repo, "main", &snapshot_in(OLD));
    write_snapshot(&repo, "w2", &snapshot_in(OLD));
    write_snapshot(&repo, "w3", &snapshot_in(OLD));

    assert_eq!(repoint_persisted_tabs_in(&repo, PROJECT, OLD, NEW), 3);

    for window in ["main", "w2", "w3"] {
        let tabs = restored(&repo, window);
        assert_eq!(
            tabs.tabs[0].agent.as_ref().unwrap().worktree_path,
            NEW,
            "window {window} still points at the dead path"
        );
    }
}

/// Another project's blob shares neither the prefix nor the fate.
#[test]
fn another_projects_layout_is_untouched() {
    let db = open_memory().expect("open memory");
    let repo = SettingsRepo::new(db);
    write_snapshot(&repo, "main", &snapshot_in(OLD));
    let other_key = settings_key("proj_other", "main");
    let other_json = serde_json::to_string(&snapshot_in(OLD)).expect("serialize");
    repo.set(&other_key, &other_json).expect("write other");

    repoint_persisted_tabs_in(&repo, PROJECT, OLD, NEW);

    let untouched = repo.get(&other_key).expect("get").expect("present");
    assert_eq!(
        untouched, other_json,
        "a different project's blob must not be rewritten"
    );
}

/// A project id that is a string PREFIX of another must not drag it along:
/// `terminal_tabs:proj_abc` is also a prefix of `terminal_tabs:proj_abc2:main`,
/// so a bare `list_prefixed` match would rewrite a different project's layout.
#[test]
fn a_project_id_that_prefixes_another_does_not_rewrite_it() {
    let db = open_memory().expect("open memory");
    let repo = SettingsRepo::new(db);
    write_snapshot(&repo, "main", &snapshot_in(OLD));
    // `proj_abc2` starts with `proj_abc`.
    let sibling_key = settings_key("proj_abc2", "main");
    let sibling_json = serde_json::to_string(&snapshot_in(OLD)).expect("serialize");
    repo.set(&sibling_key, &sibling_json).expect("write sibling");

    assert_eq!(
        repoint_persisted_tabs_in(&repo, PROJECT, OLD, NEW),
        1,
        "only this project's key"
    );
    assert_eq!(
        repo.get(&sibling_key).expect("get").expect("present"),
        sibling_json,
        "a project whose id merely starts with ours must be untouched"
    );
}

/// The legacy pre-V005 key (`terminal_tabs:<id>`, no window suffix) is this
/// project's too, and a single-window user's layout lives in it.
#[test]
fn the_legacy_single_window_key_is_repointed_too() {
    let db = open_memory().expect("open memory");
    let repo = SettingsRepo::new(db);
    let legacy_key = format!("terminal_tabs:{PROJECT}");
    repo.set(
        &legacy_key,
        &serde_json::to_string(&snapshot_in(OLD)).expect("serialize"),
    )
    .expect("write legacy");

    assert_eq!(repoint_persisted_tabs_in(&repo, PROJECT, OLD, NEW), 1);
    let tabs: PersistedTabs =
        serde_json::from_str(&repo.get(&legacy_key).expect("get").expect("present"))
            .expect("deserialize");
    assert_eq!(tabs.tabs[0].agent.as_ref().unwrap().worktree_path, NEW);
}

/// A project with nothing pointing at the renamed worktree writes nothing —
/// the rewrite must not churn every blob on every rename.
#[test]
fn a_layout_that_names_no_matching_path_is_not_rewritten() {
    let db = open_memory().expect("open memory");
    let repo = SettingsRepo::new(db);
    write_snapshot(&repo, "main", &snapshot_in("/wt/somewhere-else"));

    assert_eq!(repoint_persisted_tabs_in(&repo, PROJECT, OLD, NEW), 0);
    let tabs = restored(&repo, "main");
    assert_eq!(
        tabs.tabs[0].agent.as_ref().unwrap().worktree_path,
        "/wt/somewhere-else"
    );
}
