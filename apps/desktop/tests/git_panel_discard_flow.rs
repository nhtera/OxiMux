//! Integration test for the Phase 01 discard flow.
//!
//! Covers the seams that matter for safety:
//! - `GitPanel::discard_path` populates `pending_discard` with the
//!   right `(kind, copy)` for the file's status — never mutates the
//!   working tree.
//! - `GitPanel::confirmed_discard_path` clears the pending request,
//!   marks the path as in-flight, fires the actual `git restore --`,
//!   and clears the in-flight marker once the op returns. The on-disk
//!   file should match HEAD afterwards.
//! - `GitPanel::clear_pending_discard` drops the request without
//!   touching the working tree (Escape / Cancel path).
//!
//! Uses a real git binary against a temp repo so the round-trip
//! through `Repository::discard_paths` is exercised end-to-end.
//! `gpui::test` provides the foreground executor; `run_until_parked`
//! flushes the cx.spawn result handler after the tokio side resolves.

use gpui::TestAppContext;
use oximux_app::shell::git_panel::GitPanel;
use oximux_app::shell::git_panel::discard_confirm::DiscardKind;
use oximux_core::{FileStatus, GitState, IndexStatus, WorktreeStatus};
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::sync::watch;

/// Init a git repo at `p`, commit a single file `name` with `body`,
/// then overwrite the file with `dirty_body` to leave the worktree
/// modified.
fn seed_dirty_repo(p: &Path, name: &str, body: &str, dirty_body: &str) {
    let st = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(p)
            .status()
            .expect("git on PATH");
    };
    st(&["init", "-b", "main"]);
    st(&["config", "user.email", "test@oximux.dev"]);
    st(&["config", "user.name", "Test"]);
    std::fs::write(p.join(name), body).expect("write seed");
    st(&["add", name]);
    st(&["commit", "-m", "seed"]);
    std::fs::write(p.join(name), dirty_body).expect("write dirty");
}

fn modified_state(path: &str) -> PollState {
    PollState::Ready(GitState {
        branch: Some("main".into()),
        upstream: None,
        ahead: 0,
        behind: 0,
        head_oid: None,
        files: vec![FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Unmodified,
            WorktreeStatus::Modified,
        )],
        ..Default::default()
    })
}

#[gpui::test]
async fn discard_path_sets_pending_with_modified_copy(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_dirty_repo(tmp.path(), "src.rs", "v1\n", "v2\n");
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    let (_tx, rx) = watch::channel(modified_state("src.rs"));
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|_win, cx| {
        GitPanel::new(
            repo,
            rx,
            None,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None,
            None,
            cx,
        )
    });

    // discard_path is the safety boundary: it must NOT shell out.
    window
        .update(cx, |panel, _win, cx| {
            panel.discard_path(PathBuf::from("src.rs"), cx);
        })
        .expect("panel update");

    cx.read(|app| {
        let panel = window.read(app).expect("alive");
        let req = panel
            .pending_discard()
            .expect("pending_discard set after click");
        // Slice C reshaped DiscardRequest: `path` → `paths` (always
        // non-empty), `kind` → embedded in `scope: DiscardScope`.
        assert_eq!(req.paths, vec![PathBuf::from("src.rs")]);
        let kind = match req.scope {
            oximux_app::shell::git_panel::DiscardScope::Single { kind } => kind,
            other => panic!("expected DiscardScope::Single, got {other:?}"),
        };
        assert_eq!(kind, DiscardKind::Discard);
        assert!(req.copy.title.starts_with("Discard changes to"));
        assert_eq!(req.copy.confirm_label.as_ref(), "Discard");
        assert!(req.copy.title.contains("\"src.rs\""));
        assert!(
            panel.in_flight_discards().is_empty(),
            "no git op should run before confirmation"
        );
    });

    // Worktree must still hold the modified content — discard_path
    // alone is not destructive.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src.rs")).unwrap(),
        "v2\n"
    );
}

#[gpui::test]
async fn confirmed_discard_restores_file_and_clears_flags(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_dirty_repo(tmp.path(), "src.rs", "v1\n", "v2\n");
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    let (_tx, rx) = watch::channel(modified_state("src.rs"));
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|_win, cx| {
        GitPanel::new(
            repo,
            rx,
            None,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None,
            None,
            cx,
        )
    });

    window
        .update(cx, |panel, _win, cx| {
            panel.discard_path(PathBuf::from("src.rs"), cx);
            panel.confirmed_discard_path(PathBuf::from("src.rs"), cx);
        })
        .expect("panel update");

    // Immediately after confirm: pending cleared, in_flight populated.
    cx.read(|app| {
        let panel = window.read(app).expect("alive");
        assert!(panel.pending_discard().is_none());
        assert!(panel.in_flight_discards().contains(&PathBuf::from("src.rs")));
    });

    // Let the tokio op + cx.spawn result handler resolve. The op runs
    // on the tokio runtime (entered above); after it sends the result
    // through the oneshot channel, the gpui task picks it up on the
    // next foreground tick. A short sleep + park gives both pumps
    // room.
    std::thread::sleep(std::time::Duration::from_millis(150));
    cx.run_until_parked();

    // File restored to HEAD content; in_flight cleared.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src.rs")).unwrap(),
        "v1\n",
        "git restore -- should have brought the file back to HEAD"
    );
    cx.read(|app| {
        let panel = window.read(app).expect("alive");
        assert!(
            panel.in_flight_discards().is_empty(),
            "in_flight set must be cleared after op completes"
        );
    });
}

#[gpui::test]
async fn discard_path_is_a_noop_while_request_is_pending(cx: &mut TestAppContext) {
    // Guards the M2 fix: pressing the revert keybind while a confirm
    // dialog is already open must NOT overwrite `pending_discard` and
    // re-emit `DiscardRequested` (that would race the modal teardown
    // and let a second dialog mount over the first).
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_dirty_repo(tmp.path(), "src.rs", "v1\n", "v2\n");
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    let (_tx, rx) = watch::channel(modified_state("src.rs"));
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|_win, cx| {
        GitPanel::new(
            repo,
            rx,
            None,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None,
            None,
            cx,
        )
    });

    window
        .update(cx, |panel, _win, cx| {
            panel.discard_path(PathBuf::from("src.rs"), cx);
            // Capture the first request's copy; second call must NOT
            // overwrite it.
            let first_title = panel
                .pending_discard()
                .expect("first call sets pending")
                .copy
                .title
                .clone();
            // Second call while pending is active: silently dropped.
            panel.discard_path(PathBuf::from("src.rs"), cx);
            let after = panel.pending_discard().expect("still pending");
            assert_eq!(after.copy.title, first_title);
        })
        .expect("panel update");
}

#[gpui::test]
async fn clear_pending_discard_is_non_destructive(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_dirty_repo(tmp.path(), "src.rs", "v1\n", "v2\n");
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    let (_tx, rx) = watch::channel(modified_state("src.rs"));
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|_win, cx| {
        GitPanel::new(
            repo,
            rx,
            None,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None,
            None,
            cx,
        )
    });

    window
        .update(cx, |panel, _win, cx| {
            panel.discard_path(PathBuf::from("src.rs"), cx);
            assert!(panel.pending_discard().is_some());
            panel.clear_pending_discard(cx);
        })
        .expect("panel update");

    cx.read(|app| {
        let panel = window.read(app).expect("alive");
        assert!(panel.pending_discard().is_none());
        assert!(panel.in_flight_discards().is_empty());
    });
    // Worktree untouched.
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src.rs")).unwrap(),
        "v2\n"
    );
}
