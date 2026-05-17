//! Smoke test: `GitPanel::new` + first render against a real (empty) git
//! repo. Verifies the view constructs, takes the initial `Loading` poll
//! state, and renders without panic. No `StatusPoller` — we hand a watch
//! receiver directly, which mirrors what step 14 will do when the parent
//! shell owns the poller and fans out to the panel + sidebar badge.
//!
//! Requires a tokio runtime entered for the duration: `Repository::open`
//! shells out to `git rev-parse --show-toplevel` via `tokio::process::Command`,
//! and the watch-task uses tokio sync primitives.

use gpui::TestAppContext;
use oximux_app::shell::git_panel::GitPanel;
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

#[gpui::test]
async fn git_panel_constructs_and_renders_without_panic(cx: &mut TestAppContext) {
    // Bring up a tokio runtime so Repository::open and the watch task can run.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    // Static receiver — never ticks. The panel uses the initial `Loading` value
    // and renders the placeholder; that's all the smoke test exercises.
    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|_win, cx| {
        GitPanel::new(
            repo,
            rx,
            Theme::default(),
            Density::default(),
            Typography::default(),
            cx,
        )
    });
    cx.run_until_parked();

    cx.read(|app| {
        let _view = window.read(app).expect("GitPanel root view alive");
    });
}
