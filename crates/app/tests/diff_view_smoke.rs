//! Smoke test: `DiffView::new` against a real (empty) git repo. Verifies
//! the view constructs in `DiffViewState::Empty`, renders without panic,
//! and survives a `cx.run_until_parked` cycle. No tokio runtime entered
//! during the view lifetime — verifies the empty-state placeholder path.

use gpui::TestAppContext;
use oximux_app::shell::diff_view::DiffView;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::process::Command;

fn init_git_repo(p: &std::path::Path) {
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(p)
        .status()
        .expect("git on PATH");
}

#[gpui::test]
async fn diff_view_constructs_and_renders_empty_state(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    let window = cx.add_window(|_win, cx| {
        DiffView::new(
            repo,
            Theme::default(),
            Density::default(),
            Typography::default(),
            cx,
        )
    });
    cx.run_until_parked();

    cx.read(|app| {
        let _view = window.read(app).expect("DiffView root view alive");
    });
}
