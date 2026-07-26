//! End-to-end regression test for the DiffView hunk-level dispatch.
//!
//! Drives a real `tokio::Runtime` + GPUI test context against a temp
//! git repo with one 10-line committed file dirtied at lines 1 and 10
//! (two distant edits → two distinct hunks). Verifies:
//!
//! 1. `view.stage_hunk(0, 0)` shells out through `stage_hunks` and
//!    leaves only hunk 0 in the index. The other hunk stays unstaged.
//! 2. `view.confirmed_discard_hunk(0, 0)` shells out through
//!    `discard_hunks` and reverts the worktree side of hunk 0 to HEAD
//!    while leaving hunk 1 intact on disk. The index stays untouched.
//!
//! The DiffView is stamped directly into `Ready` via
//! `seed_ready_for_test` so the test never has to pump
//! `cx.run_until_parked()` across the panel's tokio crossings —
//! validation reads git's state directly via `rt.block_on(repo.…())`.
//! Same trade-off as `sc_stash_push_and_drop.rs` and
//! `sc_open_all_conflicts.rs`.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
};
use oximux_app::shell::diff_view::DiffView;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Harness {
    inner: Entity<DiffView>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

fn seed_two_hunk_repo(p: &Path) {
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
    let base = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
    std::fs::write(p.join("multi.txt"), &base).expect("write");
    st(&["add", "multi.txt"]);
    st(&["commit", "-m", "base"]);
    // Two distant edits → two separate hunks once `git diff` walks the file.
    let modified = base
        .replace("line 1\n", "LINE 1\n")
        .replace("line 10\n", "LINE 10\n");
    std::fs::write(p.join("multi.txt"), &modified).expect("write");
}

#[gpui::test]
async fn stage_hunk_dispatches_through_to_git(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_two_hunk_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    // Sanity: nothing staged yet.
    let pre = rt.block_on(repo.diff_staged()).expect("diff_staged pre");
    assert!(pre.is_empty(), "expected clean index, got {pre:?}");

    // Snapshot the unstaged diff so the test owns the FileDiff the
    // DiffView would have fetched. Diffs are fetched with full-file context
    // (scroll-the-whole-file), so the two distant edits land in ONE display
    // hunk — but `change_regions` must re-split them into two stageable
    // regions, which is what the per-region chips dispatch against.
    let unstaged = rt
        .block_on(repo.diff_unstaged())
        .expect("diff_unstaged pre");
    assert_eq!(unstaged.len(), 1);
    let regions = oximux_core::change_regions(&unstaged[0]);
    assert!(
        regions.len() >= 2,
        "expected ≥2 stageable regions, got {}",
        regions.len()
    );

    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let file_path = PathBuf::from("multi.txt");
    let diffs = unstaged.clone();
    let window = cx.add_window(|_win, cx| {
        let view = cx.new(|cx2| {
            DiffView::new(
                repo.clone(),
                Theme::default(),
                Density::default(),
                Typography::default(),
                cx2,
            )
        });
        view.update(cx, |v, _cx| {
            v.seed_ready_for_test(file_path.clone(), false, false, diffs);
        });
        Harness { inner: view }
    });

    // Dispatch stage_hunk(file_idx=0, hunk_idx=0).
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |view, cx| {
                view.stage_hunk(0, 0, cx);
            });
        })
        .expect("dispatch stage_hunk");

    // Wait for the tokio `git apply --cached -` shellout to land. No
    // `cx.run_until_parked()` — the post-op auto-reload spawns another
    // tokio task from inside the gpui callback and would trip
    // `test_scheduler.rs::detect_non_determinism`. Reading git's state
    // directly proves the side-effect that matters.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let staged_after = rt
        .block_on(repo.diff_staged())
        .expect("diff_staged post-stage");
    assert_eq!(
        staged_after.len(),
        1,
        "expected one staged file, got {staged_after:?}"
    );
    let staged_lines: Vec<&str> = staged_after[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.content.as_str())
        .collect();
    assert!(
        staged_lines.contains(&"LINE 1"),
        "hunk 0 (line 1) should be staged, got {staged_lines:?}"
    );
    assert!(
        !staged_lines.contains(&"LINE 10"),
        "hunk 1 (line 10) should NOT be staged, got {staged_lines:?}"
    );
}

#[gpui::test]
async fn discard_hunk_reverts_worktree_without_touching_index(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_two_hunk_repo(tmp.path());
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    let unstaged = rt
        .block_on(repo.diff_unstaged())
        .expect("diff_unstaged pre");
    assert_eq!(unstaged.len(), 1);
    assert!(oximux_core::change_regions(&unstaged[0]).len() >= 2);

    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let file_path = PathBuf::from("multi.txt");
    let diffs = unstaged.clone();
    let window = cx.add_window(|_win, cx| {
        let view = cx.new(|cx2| {
            DiffView::new(
                repo.clone(),
                Theme::default(),
                Density::default(),
                Typography::default(),
                cx2,
            )
        });
        view.update(cx, |v, _cx| {
            v.seed_ready_for_test(file_path.clone(), false, false, diffs);
        });
        Harness { inner: view }
    });

    // Dispatch confirmed_discard_hunk (bypasses the modal — the modal
    // is the same code path the test would otherwise have to drive
    // through key events, which the GPUI test harness doesn't simulate
    // for input widgets).
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |view, cx| {
                view.confirmed_discard_hunk(0, 0, cx);
            });
        })
        .expect("dispatch confirmed_discard_hunk");

    std::thread::sleep(std::time::Duration::from_millis(500));

    // Hunk 0 reverted in the worktree, hunk 1 still on disk.
    let on_disk = std::fs::read_to_string(tmp.path().join("multi.txt")).expect("read worktree");
    assert!(
        on_disk.contains("line 1\n"),
        "hunk 0 should be reverted to HEAD: {on_disk:?}"
    );
    assert!(
        on_disk.contains("LINE 10\n"),
        "hunk 1 should be intact: {on_disk:?}"
    );
    // Index never touched by discard.
    let staged_after = rt
        .block_on(repo.diff_staged())
        .expect("diff_staged post-discard");
    assert!(
        staged_after.is_empty(),
        "discard must not touch the index, got {staged_after:?}"
    );
}

// `DiffView::stage_hunk` with no tokio runtime entered MUST warn-log
// and no-op. The early-return path is structural (the `Err(_)` arm of
// `Handle::try_current`); covered structurally by code review rather
// than runtime — same trade-off documented in
// `sc_stash_push_and_drop.rs`.
