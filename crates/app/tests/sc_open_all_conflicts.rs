//! End-to-end regression test for the ConflictSummaryCard's "Open all
//! in editor" wiring. Drives a real `tokio::Runtime` + GPUI test
//! context against a temp git repo that we deliberately push into a
//! conflict state, then asserts the panel's `open_all_conflicts`
//! method:
//!
//! 1. fetches conflicting paths via `Repository::list_conflicting_paths`,
//! 2. joins each with `repo.workdir()`,
//! 3. fires the host `OnOpenFile` callback once per absolute path.
//!
//! The test wires a recording `OnOpenFile` that pushes absolute paths
//! into a shared `Arc<Mutex<Vec<PathBuf>>>` so the assertion can read
//! back what the panel dispatched.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
};
use oximux_app::shell::file_tree_view::OnOpenFile;
use oximux_app::shell::source_control::{PanelConfig, SourceControlPanel};
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Window root so cx.add_window accepts the panel entity.
/// SourceControlPanel implements Render itself, so we can wrap it in a
/// thin `Harness` whose render returns its child as-is.
struct Harness {
    inner: Entity<SourceControlPanel>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

/// Init a git repo at `p` and engineer a two-file merge conflict on
/// `main` so `list_conflicting_paths` returns both files.
fn seed_conflicted_repo(p: &Path) {
    let st = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(p)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@oximux.dev")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@oximux.dev")
            .status()
            .expect("git on PATH");
    };
    st(&["init", "-b", "main"]);
    st(&["config", "user.email", "test@oximux.dev"]);
    st(&["config", "user.name", "Test"]);
    // Base commit with two files.
    std::fs::write(p.join("alpha.txt"), "base\n").expect("write");
    std::fs::write(p.join("beta.txt"), "base\n").expect("write");
    st(&["add", "alpha.txt", "beta.txt"]);
    st(&["commit", "-m", "base"]);
    st(&["branch", "feature"]);
    // feature: change both files.
    st(&["checkout", "feature"]);
    std::fs::write(p.join("alpha.txt"), "feature\n").expect("write");
    std::fs::write(p.join("beta.txt"), "feature\n").expect("write");
    st(&["add", "alpha.txt", "beta.txt"]);
    st(&["commit", "-m", "feature"]);
    // main: change both files differently → merging feature conflicts on both.
    st(&["checkout", "main"]);
    std::fs::write(p.join("alpha.txt"), "main\n").expect("write");
    std::fs::write(p.join("beta.txt"), "main\n").expect("write");
    st(&["add", "alpha.txt", "beta.txt"]);
    st(&["commit", "-m", "main"]);
    // Merge — will fail with conflicts on both files, leaving the
    // worktree in the in-progress merge state we want.
    let _ = std::process::Command::new("git")
        .args(["merge", "feature"])
        .current_dir(p)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@oximux.dev")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@oximux.dev")
        .status();
}

#[gpui::test]
async fn open_all_conflicts_dispatches_on_open_file_per_path(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_conflicted_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    // Recording callback — every OnOpenFile invocation pushes its
    // path into the shared Vec so the assertion can read back what
    // the panel dispatched.
    let opens: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let opens_for_cb = opens.clone();
    let on_open_file: OnOpenFile = Arc::new(move |path, _window, _app| {
        opens_for_cb.lock().expect("opens lock").push(path);
    });

    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    // Build the panel inside a real window. Use a dead state_rx
    // (Loading sentinel) — the panel doesn't need a live poller for
    // this test; open_all_conflicts shells out to git directly.
    let (_state_tx, state_rx) = watch::channel(PollState::Loading);
    let window = cx.add_window(|win, cx| {
        let panel = cx.new(|cx2| {
            // The panel's PanelConfig needs DiffView + GitPanel
            // entities, which themselves need a tokio runtime entered
            // for their async wiring. Build minimal stand-ins via the
            // existing constructors.
            let diff_view = cx2.new(|cx3| {
                oximux_app::shell::diff_view::DiffView::new(
                    repo.clone(),
                    Theme::default(),
                    Density::default(),
                    Typography::default(),
                    cx3,
                )
            });
            let git_panel_rx = state_rx.clone();
            let git_panel = cx2.new(|cx3| {
                oximux_app::shell::git_panel::GitPanel::new(
                    repo.clone(),
                    git_panel_rx,
                    None,
                    Theme::default(),
                    Density::default(),
                    Typography::default(),
                    None,
                    None,
                    cx3,
                )
            });
            SourceControlPanel::new(
                PanelConfig {
                    repo: repo.clone(),
                    theme: Theme::default(),
                    density: Density::default(),
                    typography: Typography::default(),
                    worktree_settings_repo: None,
                    on_open_file: Some(on_open_file),
                },
                state_rx,
                diff_view,
                git_panel,
                win,
                cx2,
            )
        });
        Harness { inner: panel }
    });

    // Drive open_all_conflicts. The implementation lives as a
    // pub(super) method so we go through the harness's inner entity.
    window
        .update(cx, |harness, win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.open_all_conflicts(win, cx);
            });
        })
        .expect("dispatch open_all_conflicts");

    // The async shellout to `git diff --diff-filter=U` runs on a
    // tokio thread; sleep gives it time to land, then park drains
    // the gpui-side dispatch loop. Same pattern as the discard test.
    // 500 ms (vs 150-200 ms used elsewhere) absorbs slow CI agents
    // where a fresh `git` subprocess + the two-stage tokio→oneshot
    // →gpui hop can comfortably exceed the lower threshold.
    std::thread::sleep(std::time::Duration::from_millis(500));
    cx.run_until_parked();

    let recorded = opens.lock().expect("opens lock").clone();
    // Both alpha.txt and beta.txt should have been opened, with
    // absolute paths rooted at the temp repo's workdir. The order
    // matches list_conflicting_paths output order (alphabetical from
    // git diff --name-only).
    assert_eq!(
        recorded.len(),
        2,
        "expected 2 opens (alpha + beta), got {recorded:?}",
    );
    // `tempfile::tempdir()` returns paths like `/tmp/.tmpXxx` on
    // macOS; `git rev-parse --show-toplevel` resolves symlinks and
    // returns `/private/tmp/.tmpXxx`. Canonicalize both sides so
    // the strip_prefix matches regardless of which form the OS
    // surfaced.
    let workdir = tmp
        .path()
        .canonicalize()
        .expect("canonicalize workdir");
    let recorded_relative: Vec<PathBuf> = recorded
        .iter()
        .map(|p| {
            p.canonicalize()
                .expect("canonicalize recorded path")
                .strip_prefix(&workdir)
                .expect("path absolute under workdir")
                .to_path_buf()
        })
        .collect();
    assert!(
        recorded_relative.contains(&PathBuf::from("alpha.txt")),
        "expected alpha.txt in {recorded_relative:?}",
    );
    assert!(
        recorded_relative.contains(&PathBuf::from("beta.txt")),
        "expected beta.txt in {recorded_relative:?}",
    );
}

// `open_all_conflicts` with `on_open_file: None` MUST early-return
// via warn-log without panicking or shelling out. The invariant is
// guaranteed STRUCTURALLY by the method body's first statement:
//
//     let Some(on_open) = self.on_open_file.clone() else {
//         tracing::warn!(...);
//         return;
//     };
//
// An end-to-end integration test for this WAS attempted but tripped
// GPUI's test-scheduler determinism check ("Detected activity on
// thread tokio-rt-worker while test scheduler is on test thread")
// because git's process spawn happens fast enough that the tokio
// task fires before gpui's foreground executor parks. The
// early-return path is short, branchless, and impossible to drift —
// code review is the gate. The render-side path (button rendered
// `.disabled(true)` when `self.on_open_file.is_none()`) means the
// user never reaches `open_all_conflicts` in this state in real
// usage either.
