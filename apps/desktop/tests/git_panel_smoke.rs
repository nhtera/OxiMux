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
use oximux_app::git_state_cache::GitStateCache;
use oximux_app::shell::git_panel::GitPanel;
use oximux_core::GitState;
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

    // `gpui_component::vertical_scrollbar` (wired on the panel's inner
    // scroll region) reads the `gpui_component::Theme` global to colour
    // its track/thumb. Without this set, rendering panics with
    // "no state of type gpui_component::theme::Theme exists".
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|_win, cx| {
        GitPanel::new(
            repo,
            rx,
            None,
            Theme::default(),
            Density::default(),
            Typography::default(),
            None, // no host on_open callback in test wiring
            None, // no per-worktree settings persistence in test wiring
            cx,
        )
    });
    cx.run_until_parked();

    cx.read(|app| {
        let _view = window.read(app).expect("GitPanel root view alive");
    });
}

/// Stale-while-revalidate: when the panel is built with a `Loading` poll
/// channel but the process cache already holds a snapshot for this workdir,
/// the panel renders the cached snapshot immediately instead of the
/// "Loading…" placeholder. Without the cache entry it stays placeholder.
#[gpui::test]
async fn git_panel_seeds_from_cache_while_loading(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");
    let workdir = repo.workdir().to_path_buf();

    cx.update(|cx| {
        cx.set_global(gpui_component::Theme::default());
        // Pre-seed the cache for this workdir with a known snapshot.
        let cache = GitStateCache::default();
        let snapshot = GitState {
            branch: Some("main".into()),
            ..Default::default()
        };
        cache.put(&workdir, snapshot);
        cx.set_global(cache);
    });

    // Loading channel — the panel must fall back to the cached snapshot.
    let (_tx, rx) = watch::channel(PollState::Loading);
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
    cx.run_until_parked();

    cx.read(|app| {
        let panel = window.read(app).expect("GitPanel alive");
        // Seeded from cache (Some) rather than placeholder (None), even
        // though the poll channel never left `Loading`.
        assert_eq!(
            panel.snapshot_file_count(),
            Some(0),
            "panel should seed the cached snapshot while the poll is still Loading"
        );
    });
}
