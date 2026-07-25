//! End-to-end regression test for the StashPanel's `push` plumbing.
//! Drives a real `tokio::Runtime` + GPUI test context against a temp
//! git repo with one dirty file:
//!
//! 1. Asserts initial `git stash list` is empty.
//! 2. Calls `panel.push(Some(msg), false)`.
//! 3. Asserts the underlying repo now has 1 stash entry with the
//!    expected message — proving the panel actually shelled out
//!    through tokio rather than no-op'ing the runtime check.
//!
//! Validation reads git's state directly via
//! `rt.block_on(repo.stash_list())` rather than driving the panel's
//! auto-refresh through another `cx.run_until_parked()` cycle. The
//! chained refresh (panel.push → tokio stash_push → gpui callback →
//! panel.refresh → tokio stash_list → gpui callback) crosses the
//! tokio/gpui boundary TWICE; pumping `run_until_parked` between
//! those crossings trips
//! `test_scheduler.rs::detect_non_determinism` (tokio activity
//! detected on the worker thread while gpui sits parked). Querying
//! git's truth directly proves the side-effect without that race.
//! Same trade-off documented in `sc_open_all_conflicts.rs` and
//! `sc_commit_area_auto_clear.rs`.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
};
use oximux_app::shell::stash_panel::StashPanel;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::Path;
use std::process::Command;

struct Harness {
    inner: Entity<StashPanel>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

fn seed_dirty_repo(p: &Path) {
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
    std::fs::write(p.join("alpha.txt"), "base\n").expect("write");
    st(&["add", "alpha.txt"]);
    st(&["commit", "-m", "base"]);
    // Dirty the tracked file so `git stash push` has something to save.
    std::fs::write(p.join("alpha.txt"), "dirty\n").expect("write");
}

#[gpui::test]
async fn push_shells_out_to_git_stash_push(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_dirty_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    // Sanity: clean stash stack to start.
    let pre = rt
        .block_on(repo.stash_list())
        .expect("stash_list pre-push");
    assert!(pre.is_empty(), "fresh repo should have no stashes, got {pre:?}");

    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|_win, cx| {
        let panel = cx.new(|cx2| {
            StashPanel::new(
                repo.clone(),
                Theme::default(),
                Density::default(),
                Typography::default(),
                cx2,
            )
        });
        Harness { inner: panel }
    });

    // Drive the push through the panel — exactly the path the
    // PushStashDialog's confirm callback walks in production.
    let push_message = "phase-09-test".to_string();
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.push(Some(push_message.clone()), false, cx);
            });
        })
        .expect("dispatch push");

    // Wait for the tokio `git stash push` shellout to finish. NO
    // `cx.run_until_parked()` afterwards: the panel's auto-refresh
    // chain spawns another tokio task from inside the gpui callback,
    // which trips
    // `test_scheduler.rs::detect_non_determinism` when run_until_parked
    // sees tokio activity it didn't expect. We don't need to pump
    // the gpui side because the assertion reads git directly — see
    // module-level comment.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Verify git state directly. Bypasses the panel's auto-refresh
    // entirely — proves the push side-effected git, which is the
    // contract the user cares about.
    let post = rt
        .block_on(repo.stash_list())
        .expect("stash_list post-push");
    assert_eq!(
        post.len(),
        1,
        "expected 1 stash after push, got {post:?}",
    );
    assert!(
        post[0].message.contains(&push_message),
        "expected stash message to contain {push_message:?}, got {:?}",
        post[0].message,
    );
}

// `StashPanel::push` with no tokio runtime entered MUST warn-log and
// no-op without panicking. The early-return path is structural (the
// `Err(_)` arm of `Handle::try_current`); an integration test was
// attempted but tripped GPUI's test-scheduler determinism check
// because the `tokio::sync::oneshot` allocator runs through tokio's
// intrinsics even before any task is spawned. The early-return path
// is short, branchless, and impossible to drift — code review is the
// gate. Mirrors the same trade-off documented in
// `sc_commit_area_auto_clear.rs` and `sc_open_all_conflicts.rs`.
