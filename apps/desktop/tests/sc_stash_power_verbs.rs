//! End-to-end tests for Phase 8's two panel-level ops: `branch_from_stash`
//! and `restore_file_confirmed`.
//!
//! Same tokio/GPUI trade-off as `sc_stash_push_and_drop.rs` and
//! `sc_stash_partial_push.rs`: each op crosses the tokio boundary and comes
//! back through a GPUI callback, so the assertions read git's state directly
//! rather than pumping `run_until_parked` between the crossings (which trips
//! `test_scheduler.rs::detect_non_determinism`).
//!
//! What the git behaviour *is* — that `stash branch` consumes the stash, that
//! a restore stages what it writes, that a bracketed path does not drag its
//! glob sibling along — is proved against the binary in
//! `crates/git/tests/stash_power_verbs.rs`. What these add is that the panel
//! refuses a bad name before anything mutating runs, and that the ops are
//! reachable from the panel at all.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
};
use oximux_app::shell::stash_panel::StashPanel;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(p: &Path, args: &[&str]) {
    Command::new("git")
        .args(args)
        .current_dir(p)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@oximux.dev")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@oximux.dev")
        .status()
        .expect("git on PATH");
}

fn git_out(p: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(p)
        .output()
        .expect("git on PATH");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn status(p: &Path) -> Vec<String> {
    let mut lines: Vec<String> = git_out(p, &["status", "--porcelain=v1"])
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

struct Harness {
    inner: Entity<StashPanel>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

fn seed(p: &Path) {
    git(p, &["init", "-b", "main"]);
    git(p, &["config", "user.email", "test@oximux.dev"]);
    git(p, &["config", "user.name", "Test"]);
    std::fs::write(p.join("alpha.txt"), "a\n").expect("write");
    std::fs::write(p.join("keep.txt"), "k\n").expect("write");
    git(p, &["add", "-A"]);
    git(p, &["commit", "-m", "base"]);
}

fn mount(repo: &Repository, cx: &mut TestAppContext) -> gpui::WindowHandle<Harness> {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    cx.add_window(|_win, cx| {
        let panel = cx.new(|cx2| {
            StashPanel::new(
                repo.clone(),
                gpui::px(oximux_app::scm_layout_settings::DEFAULT_STASH_HEIGHT),
                None,
                Theme::default(),
                Density::default(),
                Typography::default(),
                cx2,
            )
        });
        Harness { inner: panel }
    })
}

/// Long enough for the `git` subprocess the op shells out to. The existing
/// stash tests use the same figure.
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[gpui::test]
async fn branch_from_stash_creates_the_branch_and_consumes_the_stash(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "a2\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    rt.block_on(repo.stash_push(Some("wip"), false, &[]))
        .expect("stash push");
    let sha = rt.block_on(repo.stash_list(true)).expect("list")[0]
        .sha
        .clone();

    let window = mount(&repo, cx);
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.branch_from_stash("fix-parser".into(), sha, 0, None, cx);
            });
        })
        .expect("dispatch branch_from_stash");
    settle();

    assert_eq!(git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]), "fix-parser");
    assert!(
        rt.block_on(repo.stash_list(true))
            .expect("list")
            .is_empty(),
        "stash branch consumes the stash on success",
    );
}

#[gpui::test]
async fn an_invalid_branch_name_never_reaches_a_mutating_git_call(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "a2\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    rt.block_on(repo.stash_push(Some("wip"), false, &[]))
        .expect("stash push");
    let sha = rt.block_on(repo.stash_list(true)).expect("list")[0]
        .sha
        .clone();
    let head_before = git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]);

    // `x..y` is refused by `check-ref-format`. The op asks it FIRST, so the
    // stash is untouched and HEAD has not moved — the user gets a toast, not
    // a half-done branch.
    window_dispatch(cx, &repo, sha.clone(), "x..y".into());
    settle();

    assert_eq!(git_out(p, &["rev-parse", "--abbrev-ref", "HEAD"]), head_before);
    assert_eq!(
        rt.block_on(repo.stash_list(true)).expect("list").len(),
        1,
        "an invalid name must leave the stash exactly where it was",
    );
    assert!(
        git_out(p, &["branch", "--format=%(refname:short)"])
            .lines()
            .all(|b| b.trim() == head_before),
        "no branch should have been created",
    );
}

/// Mount a panel and fire `branch_from_stash` on it. Split out because the
/// window handle has to be dropped before the assertions read git, and the
/// closure nesting obscures that in the test body.
fn window_dispatch(cx: &mut TestAppContext, repo: &Repository, sha: String, name: String) {
    let window = mount(repo, cx);
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.branch_from_stash(name, sha, 0, None, cx);
            });
        })
        .expect("dispatch branch_from_stash");
}

#[gpui::test]
async fn restore_file_confirmed_writes_one_file_and_keeps_the_stash(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "stashed\n").expect("write");
    std::fs::write(p.join("keep.txt"), "stashed-keep\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    rt.block_on(repo.stash_push(Some("wip"), false, &[]))
        .expect("stash push");
    let sha = rt.block_on(repo.stash_list(true)).expect("list")[0]
        .sha
        .clone();
    // Diverge both after the stash, so a restore of one is visible and a
    // stray restore of the other would be too.
    std::fs::write(p.join("alpha.txt"), "local\n").expect("write");
    std::fs::write(p.join("keep.txt"), "local-keep\n").expect("write");

    let window = mount(&repo, cx);
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.restore_file_confirmed(sha, PathBuf::from("alpha.txt"), cx);
            });
        })
        .expect("dispatch restore_file_confirmed");
    settle();

    assert_eq!(
        std::fs::read_to_string(p.join("alpha.txt")).expect("read"),
        "stashed\n",
    );
    assert_eq!(
        std::fs::read_to_string(p.join("keep.txt")).expect("read"),
        "local-keep\n",
        "the unnamed file must not have moved",
    );
    // Staged — which is why the confirm dialog says so.
    assert!(
        status(p).contains(&"M  alpha.txt".to_string()),
        "restored file should be staged, got {:?}",
        status(p),
    );
    assert_eq!(
        rt.block_on(repo.stash_list(true)).expect("list").len(),
        1,
        "restore copies out; the stash stays",
    );
}

// ── Phase 9: rename ───────────────────────────────────────────────────────
//
// The sequence itself — order preserved, dates preserved, an intruder pushed
// mid-loop surviving — is `crates/git/tests/stash_rename.rs`. What these two
// add is that the op is reachable from the panel and that it refuses a stash
// that has left the stack instead of rewriting a neighbour.

#[gpui::test]
async fn rename_confirmed_changes_the_message_and_keeps_the_position(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    for (file, msg) in [("alpha.txt", "first"), ("keep.txt", "second")] {
        std::fs::write(p.join(file), "changed\n").expect("write");
        git(p, &["stash", "push", "-m", msg]);
    }

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    // `second` is on top, so `first` is the BOTTOM entry — the case where the
    // rename has to take an entry off and put it back rather than just
    // re-storing the top one.
    let entries = rt.block_on(repo.stash_list(true)).expect("list");
    let target = entries[1].sha.clone();

    let window = mount(&repo, cx);
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.rename_confirmed(target.clone(), 1, "renamed".into(), cx);
            });
        })
        .expect("dispatch rename_confirmed");
    settle();

    let after = rt.block_on(repo.stash_list(true)).expect("list");
    assert_eq!(
        after.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
        vec!["second", "renamed"],
    );
    assert_eq!(after[1].sha, target, "the rename rewrote the commit");
}

#[gpui::test]
async fn rename_confirmed_on_a_vanished_stash_leaves_the_stack_alone(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "a2\n").expect("write");
    git(p, &["stash", "push", "-m", "wip"]);

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    let before = git_out(p, &["stash", "list", "--format=%gs"]);

    let window = mount(&repo, cx);
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                // A sha the stack does not hold — the shape a menu click takes
                // when something else dropped the entry first.
                panel.rename_confirmed(
                    "0000000000000000000000000000000000000000".into(),
                    0,
                    "nope".into(),
                    cx,
                );
            });
        })
        .expect("dispatch rename_confirmed");
    settle();

    assert_eq!(
        git_out(p, &["stash", "list", "--format=%gs"]),
        before,
        "a rename aimed at a missing stash rewrote a neighbour",
    );
}

// ── Apply one file ────────────────────────────────────────────────────────
//
// What `git apply` does to the worktree — that a conflicting local edit is
// refused with nothing written, that an untracked file comes out of `^3`,
// that a rename does not drag its old path along — is proved against the
// binary in `crates/git/tests/stash_apply_file.rs`. What these add is the
// panel's half: that the op reaches git at all, and that it refuses to guess
// when the panel cannot say where the file came from.

/// The panel resolves `origin` from its own cached file list, which only
/// exists once a row has been expanded. Seeded here in one synchronous update
/// — list, expand, result — rather than by driving the real fetch, for the
/// same reason the rest of this file reads git directly instead of pumping the
/// scheduler between crossings.
///
/// `toggle_expanded` is not decoration: `apply_files_result` lands a result
/// only where that expansion left its `Loading` marker, so without it the
/// injected list is dropped and the op silently does nothing — which is how
/// this helper was wrong the first time. The real fetch it also starts is
/// harmless; its result meets no marker and is discarded in turn.
fn seed_file_cache(
    window: &gpui::WindowHandle<Harness>,
    cx: &mut TestAppContext,
    sha: &str,
    path: &str,
    origin: oximux_core::StashFileOrigin,
) {
    let entry = oximux_core::StashEntry {
        stash_ref: oximux_core::StashRef { index: 0 },
        branch: "main".into(),
        message: "wip".into(),
        sha: sha.to_string(),
        created_at: 1_785_767_406,
        relative: "just now".into(),
    };
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.apply_list_result(Ok(vec![entry]), cx);
                panel.toggle_expanded(sha.to_string(), cx);
                panel.apply_files_result(
                    sha,
                    Ok(vec![oximux_core::StashFile {
                        path: PathBuf::from(path),
                        status: oximux_core::DiffStatus::Modified,
                        origin,
                    }]),
                    cx,
                );
                assert!(
                    matches!(
                        panel.files_for(sha),
                        Some(oximux_app::shell::stash_panel::StashFilesState::Ready(_))
                    ),
                    "the cache must actually hold a list, or the op under test is a no-op",
                );
            });
        })
        .expect("seed the panel's file cache");
}

#[gpui::test]
async fn apply_file_writes_the_worktree_and_leaves_the_index_alone(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "stashed\n").expect("write");
    std::fs::write(p.join("keep.txt"), "stashed-keep\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    rt.block_on(repo.stash_push(Some("wip"), false, &[]))
        .expect("stash push");
    let sha = rt.block_on(repo.stash_list(true)).expect("list")[0]
        .sha
        .clone();

    let window = mount(&repo, cx);
    seed_file_cache(
        &window,
        cx,
        &sha,
        "alpha.txt",
        oximux_core::StashFileOrigin::Tracked,
    );
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.apply_file(&sha, Path::new("alpha.txt"), cx);
            });
        })
        .expect("dispatch apply_file");
    settle();

    assert_eq!(
        std::fs::read_to_string(p.join("alpha.txt")).expect("read"),
        "stashed\n",
    );
    assert_eq!(
        std::fs::read_to_string(p.join("keep.txt")).expect("read"),
        "k\n",
        "the unnamed file must not have moved",
    );
    // The line that separates this verb from restore, which stages what it
    // writes — and what the toast tells the user. Asserted as two plumbing
    // queries rather than a porcelain XY column because `git_out` trims its
    // output, which eats exactly the leading space that means "unstaged".
    assert_eq!(
        git_out(p, &["diff", "--cached", "--name-only"]),
        "",
        "apply must not touch the index",
    );
    assert_eq!(git_out(p, &["diff", "--name-only"]), "alpha.txt");
    assert_eq!(
        rt.block_on(repo.stash_list(true)).expect("list").len(),
        1,
        "apply copies out; the stash stays",
    );
}

#[gpui::test]
async fn apply_file_refuses_to_guess_an_origin_it_was_never_given(cx: &mut TestAppContext) {
    // The file cache is deliberately NOT seeded. `origin` chooses which
    // revision the patch is read from — the stash commit for a tracked file,
    // the parentless `^3` for an untracked one — so a panel that cannot say
    // which must do nothing rather than pick one and write the wrong bytes.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "stashed\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    rt.block_on(repo.stash_push(Some("wip"), false, &[]))
        .expect("stash push");
    let sha = rt.block_on(repo.stash_list(true)).expect("list")[0]
        .sha
        .clone();

    let window = mount(&repo, cx);
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.apply_file(&sha, Path::new("alpha.txt"), cx);
            });
        })
        .expect("dispatch apply_file");
    settle();

    assert_eq!(
        std::fs::read_to_string(p.join("alpha.txt")).expect("read"),
        "a\n",
        "nothing should have been written",
    );
    assert!(status(p).is_empty(), "got {:?}", status(p));
}
