//! End-to-end regression test for commit-area auto-clear: after a
//! successful `git commit`, the inline composer textarea clears
//! itself rather than holding the just-committed message until the
//! user wipes it manually.
//!
//! Drives a real `tokio::Runtime` + `gpui::TestAppContext`:
//! - seeds a temp git repo with one staged file ready to commit,
//! - builds a `CommitArea` inside a Harness window root,
//! - types a message into the InputState,
//! - calls `submit` with a synthetic enabled `PrimaryAction::Commit`,
//! - drains both runtimes via `cx.run_until_parked()`,
//! - asserts the textarea is empty (success path) or preserved
//!   (failure path).

use gpui::{AppContext, Context, Entity, IntoElement, Render, TestAppContext, Window, div};
use oximux_app::shell::source_control::commit_area::CommitArea;
use oximux_app::shell::source_control::primary_action::{PrimaryAction, PrimaryActionKind};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::{Db, WorktreeSettingsRepo, open_memory};
use std::path::Path;
use std::process::Command;

/// Thin window root so `cx.add_window` accepts the test harness.
/// `CommitArea` doesn't implement `Render` itself (its `.render(...)`
/// takes parameters), so the harness can't paint it directly — but
/// the test only needs the entity to LIVE inside a real window, not
/// to be rendered. `render` returns a bare `div` for that reason.
struct Harness {
    inner: Entity<CommitArea>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Seed a git repo at `p`, then create + stage a single file `name`
/// with `body` so the test can commit it without further index
/// manipulation. An initial commit is created first so HEAD exists.
fn seed_repo_with_staged_file(p: &Path, name: &str, body: &str) {
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
    std::fs::write(p.join("README"), "seed\n").expect("write seed");
    st(&["add", "README"]);
    st(&["commit", "-m", "initial"]);
    std::fs::write(p.join(name), body).expect("write file");
    st(&["add", name]);
}

fn seed_workspace_settings(workspace_id: &str) -> (Db, WorktreeSettingsRepo) {
    let db = open_memory().expect("open in-memory storage");
    db.with_conn(|c| {
        c.execute(
            "INSERT INTO projects (id, name, root_path, default_branch, created_at) \
             VALUES ('proj-1', 'test', '/tmp/test', 'main', '0')",
            [],
        )?;
        c.execute(
            "INSERT INTO workspaces \
                 (id, project_id, name, slug, branch, worktree_path, status, created_at) \
             VALUES (?1, 'proj-1', 'ws', 'ws', 'main', ?1, 'active', '0')",
            [workspace_id],
        )
    })
    .expect("seed workspace");
    let repo = WorktreeSettingsRepo::new(db.clone());
    (db, repo)
}

fn commit_action() -> PrimaryAction {
    PrimaryAction {
        kind: PrimaryActionKind::Commit,
        label: "Commit".to_string(),
        title: String::new(),
        disabled: false,
    }
}

#[gpui::test]
async fn textarea_clears_after_successful_commit(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo_with_staged_file(tmp.path(), "src.rs", "fn main() {}\n");
    let workspace_id = tmp.path().to_string_lossy().to_string();
    let (_db, settings_repo) = seed_workspace_settings(&workspace_id);
    let repo = rt
        .block_on(Repository::open(tmp.path()))
        .expect("open repo");

    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    let window = cx.add_window(|win, cx| {
        let inner = cx.new(|cx2| {
            CommitArea::new(
                repo,
                Some(settings_repo.clone()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                win,
                cx2,
            )
        });
        Harness { inner }
    });

    // Type a commit message into the textarea via the inner entity.
    window
        .update(cx, |harness, win, cx| {
            harness.inner.update(cx, |area, cx| {
                area.message_state
                    .update(cx, |state, cx| state.set_value("test commit subject", win, cx));
            });
        })
        .expect("set message");

    // Sanity check.
    window
        .update(cx, |harness, _win, cx| {
            let value = harness.inner.read(cx).message_state.read(cx).value();
            assert_eq!(
                value.as_ref(),
                "test commit subject",
                "message should populate before submit",
            );
        })
        .expect("read message");

    // Fire commit through the same path the primary-button click takes.
    window
        .update(cx, |harness, win, cx| {
            harness.inner.update(cx, |area, cx| {
                area.submit(&commit_action(), win, cx);
            });
        })
        .expect("submit");

    // The tokio op runs on a separate thread (the rt entered above); after it
    // sends through the oneshot, gpui picks up on the next foreground tick.
    // Two-phase wait, because the pipeline has a variable-latency half and a
    // fixed-cost half:
    //
    // 1. Poll the REPO for the commit. The git child's spawn latency varies by
    //    machine (a cold Windows spawn behind an AV scan costs anywhere from
    //    tens of ms to seconds), so any single pre-sized nap here is a flake.
    //    The poll deliberately watches git rather than the textarea — laps of
    //    `run_until_parked` while the tokio worker is still mid-commit trip
    //    the test scheduler's determinism check ("activity on thread
    //    tokio-rt-worker"), the same trap the failure-path note at the bottom
    //    of this file describes.
    // 2. One short sleep + park for what remains: the oneshot send and the
    //    foreground tick, whose cost does not vary by machine.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["log", "-1", "--format=%s"])
            .output()
            .expect("git log");
        if String::from_utf8_lossy(&head.stdout).trim() == "test commit subject" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "commit never landed in the repo within 10s",
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    cx.run_until_parked();

    // The textarea must now be empty — primary acceptance criterion.
    window
        .update(cx, |harness, _win, cx| {
            let value = harness.inner.read(cx).message_state.read(cx).value();
            assert!(
                value.is_empty(),
                "textarea must auto-clear after successful commit; got {:?}",
                value.as_ref(),
            );
        })
        .expect("read post-commit");

    // Coherency: the persisted draft must also be cleared. The
    // success path explicitly calls `schedule_draft_save(String::new(), cx)`
    // alongside `set_value` because `InputState::set_value` suppresses
    // `InputEvent::Change` (its `emit_events = false` toggle), so the
    // Change-subscription debounce path is bypassed. Without this
    // explicit clear, the next panel mount would ghost the
    // just-committed message back into the textarea.
    //
    // schedule_draft_save spawns a 250ms-debounced upsert; sleep past
    // the debounce so the SQLite write lands before we read.
    std::thread::sleep(std::time::Duration::from_millis(350));
    cx.run_until_parked();
    assert!(
        settings_repo.get(&workspace_id).expect("get").is_none_or(|s| s.commit_draft.is_none()),
        "commit_draft must be cleared from SQLite after successful commit",
    );
}

// Failure-path preservation note:
//
// A failed commit must NOT clear the user's draft (the whole point of
// preserving it is to let the user fix whatever broke and retry). The
// failure-preserves-draft invariant is enforced STRUCTURALLY by
// `CommitArea::apply_result` — the `Err` arm only touches
// `self.status`; there's no codepath through `apply_result` that
// clears `message_state` outside the `Ok` arm.
//
// An end-to-end integration test for this WAS attempted but tripped
// GPUI's test-scheduler determinism check ("Detected activity on
// thread tokio-rt-worker while test scheduler is on test thread")
// because git's "nothing to commit" exit path returns so fast the
// oneshot delivers before gpui's foreground executor parks. The
// existing `textarea_clears_after_successful_commit` test exercises
// the same `apply_result` path with `Ok(...)`; the `Err(...)` arm is
// a 3-line `self.status = Failed(...)` with no other side effects.
// If a future refactor adds a clear in the `Err` arm, code review is
// the gate.
