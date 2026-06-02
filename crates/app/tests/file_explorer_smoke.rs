//! Smoke test: construct `FileExplorer` with a static watch channel, push a
//! `PollState::Ready(GitState)` through the channel, and assert that the
//! entity's status_map is populated after the push.

use gpui::TestAppContext;
use oximux_app::shell::file_explorer::FileExplorer;
use oximux_core::{FileStatus, GitState, IndexStatus, WorktreeStatus};
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

fn setup() -> (tokio::runtime::Runtime, tempfile::TempDir, Repository) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());

    // Create a real file so the root dir load has something to return.
    std::fs::write(tmp.path().join("README.md"), b"hello").expect("write file");

    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");
    (rt, tmp, repo)
}

#[gpui::test]
async fn file_explorer_constructs_without_panic(cx: &mut TestAppContext) {
    let (rt, tmp, _repo) = setup();
    let _guard = rt.enter();

    // header_render uses gpui-component `Button` widgets which read from the
    // theme global initialized by `gpui_component::init`.
    cx.update(gpui_component::init);

    let (tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|win, cx| {
        FileExplorer::new(
            tmp.path().to_path_buf(),
            rx,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None, // no host on_open callback in test wiring
            win,
            cx,
        )
    });
    cx.run_until_parked();

    // Push a Ready state with one modified file.
    let git_state = GitState {
        branch: Some("main".into()),
        upstream: None,
        ahead: 0,
        behind: 0,
        head_oid: None,
        files: vec![FileStatus::with_status(
            std::path::PathBuf::from("README.md"),
            IndexStatus::Unmodified,
            WorktreeStatus::Modified,
        )],
        ..Default::default()
    };
    tx.send(PollState::Ready(git_state)).expect("send ok");
    cx.run_until_parked();

    cx.read(|app| {
        let explorer = window.read(app).expect("FileExplorer root view alive");
        // Status map should have the modified file.
        let key = std::path::PathBuf::from("README.md");
        assert!(
            explorer.status_map().contains_key(&key),
            "status_map should contain README.md after Ready push"
        );
    });

    // Keep tmp alive until here so the directory still exists during the test.
    drop(tmp);
}

#[gpui::test]
async fn file_explorer_renders_without_panic_before_dir_load(cx: &mut TestAppContext) {
    // Verifies the entity constructs and renders without panic before the async
    // dir-load completes (GPUI test scheduler runs single-thread; tokio oneshot
    // doesn't resolve synchronously, so rows stay empty — correct "Loading…" state).
    let (rt, tmp, _repo) = setup();
    let _guard = rt.enter();

    cx.update(gpui_component::init);

    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|win, cx| {
        FileExplorer::new(
            tmp.path().to_path_buf(),
            rx,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None, // no host on_open callback in test wiring
            win,
            cx,
        )
    });

    cx.run_until_parked();

    // Entity must be alive; no panic during construction or render.
    cx.read(|app| {
        let _explorer = window
            .read(app)
            .expect("FileExplorer alive after construction");
    });

    drop(tmp);
}
