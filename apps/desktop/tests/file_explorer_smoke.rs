//! Smoke test: construct `FileExplorer` with a static watch channel, push a
//! `PollState::Ready(GitState)` through the channel, and assert that the
//! entity's status_map is populated after the push.
//!
//! Built through `new_unwatched`: `#[gpui::test]` runs a deterministic
//! scheduler that panics when anything reaches the app from a thread it does
//! not own, and a live filesystem watch is its own OS thread. See
//! `FileExplorer::new_unwatched` for the full reasoning and for where the
//! watch is covered instead.

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
        FileExplorer::new_unwatched(
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
        FileExplorer::new_unwatched(
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

#[gpui::test]
async fn a_seeded_channel_populates_the_status_map_without_a_second_send(
    cx: &mut TestAppContext,
) {
    // Regression: the poller is spawned SEEDED with the cached `GitState` and
    // publishes through `send_if_modified`, so on a relaunch against an
    // unchanged repo the first poll matches the seed and sends nothing. An
    // explorer that only awaited `changed()` stayed blank — no badges, no eye
    // toggle, every ignored entry visible — until the user happened to edit a
    // file. Construction must adopt whatever the channel already holds.
    let (rt, tmp, _repo) = setup();
    let _guard = rt.enter();

    cx.update(gpui_component::init);

    let git_state = GitState {
        branch: Some("main".into()),
        upstream: None,
        ahead: 0,
        behind: 0,
        head_oid: None,
        files: vec![
            FileStatus::with_status(
                std::path::PathBuf::from("README.md"),
                IndexStatus::Unmodified,
                WorktreeStatus::Modified,
            ),
            FileStatus::with_status(
                std::path::PathBuf::from("dist/"),
                IndexStatus::Unmodified,
                WorktreeStatus::Ignored,
            ),
        ],
        ..Default::default()
    };
    // The channel carries the sample BEFORE the explorer subscribes, and
    // nothing is ever sent afterwards — exactly the seeded-poller case.
    let (_tx, rx) = watch::channel(PollState::Ready(git_state));

    let window = cx.add_window(|win, cx| {
        FileExplorer::new_unwatched(
            tmp.path().to_path_buf(),
            rx,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None,
            win,
            cx,
        )
    });
    cx.run_until_parked();

    cx.read(|app| {
        let explorer = window.read(app).expect("FileExplorer root view alive");
        assert!(
            explorer
                .status_map()
                .contains_key(&std::path::PathBuf::from("README.md")),
            "the seeded sample must populate status_map with no second send"
        );
        assert!(
            explorer.has_ignored_entries(),
            "the eye toggle must appear from the seeded sample alone"
        );
    });

    drop(tmp);
}
