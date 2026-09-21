//! End-to-end tests for `StashPanel::push_paths` — the partial stash the
//! CHANGES panel fires with a file selection.
//!
//! Same tokio/GPUI trade-off as `sc_stash_push_and_drop.rs`: the panel's op
//! crosses the tokio boundary and comes back through a GPUI callback, so the
//! assertions read git's state directly rather than pumping
//! `run_until_parked` between the crossings (which trips
//! `test_scheduler.rs::detect_non_determinism`).
//!
//! The pathspec semantics themselves — literal matching, the staged/unstaged
//! split, what a rename does — are proved against the git binary in
//! `crates/git/tests/stash_partial.rs`. What these tests add is that the panel
//! actually runs the *sequence*: unstage the rename pair, then push.

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

/// `git status --porcelain=v1`, sorted — XY columns included.
fn status(p: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(p)
        .output()
        .expect("git on PATH");
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
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
async fn push_paths_stashes_only_the_named_paths(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "a2\n").expect("write");
    std::fs::write(p.join("keep.txt"), "k2\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    let window = mount(&repo, cx);

    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.push_paths(
                    Some("partial".into()),
                    false,
                    vec![PathBuf::from("alpha.txt")],
                    vec![],
                    None,
                    cx,
                );
            });
        })
        .expect("dispatch push_paths");
    settle();

    let list = rt.block_on(repo.stash_list(true)).expect("stash_list");
    assert_eq!(list.len(), 1, "expected one stash, got {list:?}");
    assert!(list[0].message.contains("partial"), "{:?}", list[0].message);
    assert_eq!(
        status(p),
        vec![" M keep.txt".to_string()],
        "the unselected file must still be dirty"
    );
}

#[gpui::test]
async fn push_paths_unstages_a_rename_pair_before_pushing(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    git(p, &["mv", "alpha.txt", "beta.txt"]);
    std::fs::write(p.join("keep.txt"), "k2\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    let window = mount(&repo, cx);

    // Naming both sides of a STAGED rename in a pathspec fails outright; the
    // panel has to unstage the pair first. Passing `rename_pairs` is what asks
    // it to, and `-u` is mandatory because `beta.txt` is untracked by then.
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.push_paths(
                    Some("renamed".into()),
                    true,
                    vec![PathBuf::from("alpha.txt"), PathBuf::from("beta.txt")],
                    vec![(PathBuf::from("alpha.txt"), PathBuf::from("beta.txt"))],
                    None,
                    cx,
                );
            });
        })
        .expect("dispatch push_paths");
    settle();

    let list = rt.block_on(repo.stash_list(true)).expect("stash_list");
    assert_eq!(list.len(), 1, "expected one stash, got {list:?}");
    let after = status(p);
    assert_eq!(
        after,
        vec![" M keep.txt".to_string()],
        "the rename must be gone from the worktree with nothing orphaned in the index"
    );
    // The rename is stashed as a DELETION of `alpha.txt` plus an untracked
    // `beta.txt`, so stashing it reverts the worktree to HEAD: `alpha.txt` is
    // back and `beta.txt` is gone. Both sides of the rename left, which is
    // exactly what "no orphaned index entry" looks like on disk.
    assert!(
        p.join("alpha.txt").exists(),
        "the rename's source should be restored by the stash"
    );
    assert!(
        !p.join("beta.txt").exists(),
        "the rename's target should have gone into the stash"
    );
}

#[gpui::test]
async fn push_paths_restores_the_rename_staging_when_the_push_fails(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    git(p, &["mv", "alpha.txt", "beta.txt"]);

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    let window = mount(&repo, cx);

    // `-u` omitted, so the untracked `beta.txt` makes git refuse the pathspec
    // and stash nothing — the exact window the rollback exists for. Without
    // it the user would be left holding an unstaged rename they never asked
    // for and no stash to show for it.
    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.push_paths(
                    Some("doomed".into()),
                    false,
                    vec![PathBuf::from("alpha.txt"), PathBuf::from("beta.txt")],
                    vec![(PathBuf::from("alpha.txt"), PathBuf::from("beta.txt"))],
                    None,
                    cx,
                );
            });
        })
        .expect("dispatch push_paths");
    settle();

    assert!(
        rt.block_on(repo.stash_list(true))
            .expect("stash_list")
            .is_empty(),
        "the push must have failed"
    );
    assert_eq!(
        status(p),
        vec!["R  alpha.txt -> beta.txt".to_string()],
        "the rename's staging must be back exactly as the user left it"
    );
}

#[gpui::test]
async fn push_paths_with_no_paths_is_a_no_op(cx: &mut TestAppContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    let tmp = tempfile::tempdir().expect("tempdir");
    let p = tmp.path();
    seed(p);
    std::fs::write(p.join("alpha.txt"), "a2\n").expect("write");

    let repo = rt.block_on(Repository::open(p)).expect("open repo");
    let window = mount(&repo, cx);

    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |panel, cx| {
                panel.push_paths(None, false, vec![], vec![], None, cx);
            });
        })
        .expect("dispatch push_paths");
    settle();

    assert!(
        rt.block_on(repo.stash_list(true))
            .expect("stash_list")
            .is_empty(),
        "an empty path list must not stash the whole worktree"
    );
    assert_eq!(status(p), vec![" M alpha.txt".to_string()]);
}
