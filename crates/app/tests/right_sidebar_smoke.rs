//! Smoke tests: construct `RightSidebar` with a static watch channel (no tokio
//! thread-pool tasks), verify default state, tab switching, toggle, and
//! the no-repo fallback that guards against SourceControl tab when hidden.
//!
//! Mirrors `git_panel_smoke.rs`: hands a watch::Receiver directly so no
//! `StatusPoller` background thread runs during the GPUI test scheduler.

use gpui::TestAppContext;
use oximux_app::shell::right_sidebar::{RightSidebar, tab::RightTab};
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use std::process::Command;
use tokio::sync::watch;

fn init_git_repo(p: &std::path::Path) {
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(p)
        .status()
        .expect("git on PATH");
}

/// Shared setup: temp git repo + current_thread runtime.
fn setup_repo() -> (tokio::runtime::Runtime, Repository) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());
    // leak dir so the path stays valid for the duration of the test
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let repo = rt.block_on(Repository::open(&path)).expect("open repo");
    (rt, repo)
}

#[gpui::test]
async fn right_sidebar_defaults_to_source_control_open(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();

    // Static receiver — never ticks. Mirrors git_panel_smoke.rs pattern.
    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|_win, cx| {
        RightSidebar::new_for_test(
            repo,
            rx,
            true, // has_repo
            Theme::default(),
            Density::default(),
            Typography::default(),
            cx,
        )
    });
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("RightSidebar root view alive");
        assert_eq!(sidebar.active_tab, RightTab::SourceControl);
        assert!(sidebar.open);
    });
}

#[gpui::test]
async fn right_sidebar_select_tab_and_toggle(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();

    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|_win, cx| {
        RightSidebar::new_for_test(
            repo,
            rx,
            true, // has_repo
            Theme::default(),
            Density::default(),
            Typography::default(),
            cx,
        )
    });
    cx.run_until_parked();

    // Switch to Explorer tab.
    window
        .update(cx, |sidebar, _window, cx| {
            sidebar.select_tab(RightTab::Explorer, cx);
        })
        .expect("update succeeds");
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert_eq!(sidebar.active_tab, RightTab::Explorer);
        assert!(sidebar.open);
    });

    // Toggle closes the sidebar.
    window
        .update(cx, |sidebar, _window, cx| {
            sidebar.toggle(cx);
        })
        .expect("update succeeds");
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert!(!sidebar.open);
    });
}

/// M3 bonus: when `has_repo = false`, `_poller` is None so visible_tabs omits
/// SourceControl. Attempting `select_tab(SourceControl)` must fall back to Explorer.
#[gpui::test]
async fn right_sidebar_no_repo_select_source_control_falls_back(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();

    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|_win, cx| {
        RightSidebar::new_for_test(
            repo,
            rx,
            false, // has_repo = false → _poller = None
            Theme::default(),
            Density::default(),
            Typography::default(),
            cx,
        )
    });
    cx.run_until_parked();

    // Default tab with no repo should NOT be SourceControl.
    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert_ne!(sidebar.active_tab, RightTab::SourceControl);
        assert_eq!(sidebar.active_tab, RightTab::Explorer);
    });

    // Trying to switch to SourceControl (not in visible_tabs) falls back to Explorer.
    window
        .update(cx, |sidebar, _window, cx| {
            sidebar.select_tab(RightTab::SourceControl, cx);
        })
        .expect("update succeeds");
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert_eq!(sidebar.active_tab, RightTab::Explorer);
    });
}
