//! The stash section's expansion state machine: what a row's file list
//! caches, when it refetches, and what an expansion survives.
//!
//! # Why no subprocess runs here
//!
//! The panel's fetches cross the tokio↔GPUI boundary, and `#[gpui::test]`
//! panics the moment a tokio worker wakes a GPUI task — "Your test is not
//! deterministic" — so a test that drove a real `git stash files` through the
//! panel would be flaky by construction, and flaky in a way that names an
//! unrelated test. (Same constraint `sc_stash_push_and_drop.rs` documents.)
//!
//! So the two halves are proved where each can be proved honestly. That git
//! reports a stash's tracked and untracked files correctly is
//! `crates/git/tests/stash_ops.rs`; that the two revisions those files need
//! actually render is `crates/git/tests/stash_diff.rs`. What is left — the
//! cache, the expansion set, and the reconciliation a refresh performs — is
//! the state machine below, driven through the completion handlers the real
//! fetches call, with no runtime entered.
//!
//! Not covered here, because it needs pixels: the chevron's hit area, the
//! file rows' layout, and the drag handle. Those are live-verified.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
    px,
};
use oximux_app::scm_layout_settings::DEFAULT_STASH_HEIGHT;
use oximux_app::shell::stash_panel::{StashFilesState, StashPanel};
use oximux_core::{DiffStatus, StashEntry, StashFile, StashFileOrigin, StashRef};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Harness {
    inner: Entity<StashPanel>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

/// An empty repo is enough: nothing below shells out to git. The panel needs
/// a `Repository` to exist, not to answer.
fn seed_repo(p: &Path) {
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.email", "test@oximux.dev"],
        &["config", "user.name", "Test"],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(p)
            .status()
            .expect("git on PATH");
    }
    std::fs::write(p.join("alpha.txt"), "base\n").expect("write");
    for args in [&["add", "-A"][..], &["commit", "-m", "base"]] {
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
}

/// Mount a panel. The tokio runtime opens the repo and is then **dropped**,
/// so nothing the panel does afterwards can spawn onto a foreign thread —
/// which is what keeps the GPUI test scheduler deterministic.
fn mounted(p: &Path, cx: &mut TestAppContext) -> Entity<StashPanel> {
    let repo = {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(Repository::open(p)).expect("open repo")
    };
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|_win, cx| {
        let panel = cx.new(|cx2| {
            StashPanel::new(
                repo,
                px(DEFAULT_STASH_HEIGHT),
                // No settings repo: heights are in-memory and nothing is
                // persisted out of a test.
                None,
                Theme::default(),
                Density::default(),
                Typography::default(),
                cx2,
            )
        });
        Harness { inner: panel }
    });
    window
        .update(cx, |harness, _win, _cx| harness.inner.clone())
        .expect("harness")
}

fn entry(index: usize, sha: &str, message: &str) -> StashEntry {
    StashEntry {
        stash_ref: StashRef { index },
        branch: "main".into(),
        message: message.into(),
        sha: sha.into(),
        created_at: 1_785_767_406,
        relative: "7 weeks ago".into(),
    }
}

fn file(path: &str, origin: StashFileOrigin) -> StashFile {
    StashFile {
        path: PathBuf::from(path),
        status: match origin {
            StashFileOrigin::Tracked => DiffStatus::Modified,
            StashFileOrigin::Untracked => DiffStatus::Added,
        },
        origin,
    }
}

/// `newer` at stash@{0}, `older` at stash@{1}.
fn two_stashes() -> Vec<StashEntry> {
    vec![entry(0, "aaa1111", "newer"), entry(1, "bbb2222", "older")]
}

/// The three files the `newer` stash touches — two tracked, one untracked.
fn newer_files() -> Vec<StashFile> {
    vec![
        file("alpha.txt", StashFileOrigin::Tracked),
        file("sub/beta.txt", StashFileOrigin::Tracked),
        file("fresh.txt", StashFileOrigin::Untracked),
    ]
}

#[gpui::test]
async fn nothing_is_fetched_until_a_row_is_expanded(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        // The list has landed; the file lists must not have. This is the
        // whole point of the lazy fetch — a 50-stash repo pays nothing for
        // rows nobody opened.
        for e in two_stashes() {
            assert!(p.files_for(&e.sha).is_none(), "{} fetched eagerly", e.sha);
            assert_eq!(p.file_count(&e.sha), None, "and claimed a count");
        }
    });
}

#[gpui::test]
async fn expanding_marks_loading_then_populates_the_count(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        // An expansion in flight must paint SOMETHING — a row that renders
        // nothing is indistinguishable from a stash with no files.
        assert!(matches!(
            p.files_for("aaa1111"),
            Some(StashFilesState::Loading)
        ));
        assert_eq!(p.file_count("aaa1111"), None, "no count while loading");

        p.apply_files_result("aaa1111", Ok(newer_files()), cx);
        let Some(StashFilesState::Ready(files)) = p.files_for("aaa1111") else {
            panic!("expected Ready, got {:?}", p.files_for("aaa1111"));
        };
        assert_eq!(files.len(), 3);
        assert_eq!(p.file_count("aaa1111"), Some(3), "the parent row's count");
        // The untracked file keeps its origin, which is what decides the
        // revision its diff is read from. Lose it and the tab is blank.
        assert_eq!(files[2].origin, StashFileOrigin::Untracked);
    });
}

#[gpui::test]
async fn a_failed_fetch_is_remembered_as_a_failure(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Err("git exploded".into()), cx);

        assert!(
            matches!(p.files_for("aaa1111"), Some(StashFilesState::Failed(e)) if e == "git exploded"),
            "a failure has to survive as a failure — rendered as an empty \
             expansion it reads as a stash that touched no files"
        );
        assert_eq!(
            p.file_count("aaa1111"),
            None,
            "a failed fetch must not claim a count"
        );
    });
}

#[gpui::test]
async fn collapsing_and_re_expanding_does_not_refetch(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(newer_files()), cx);

        p.toggle_expanded("aaa1111".into(), cx);
        assert!(!p.is_expanded("aaa1111"));
        p.toggle_expanded("aaa1111".into(), cx);
        // Re-expanding must land on the cached list, not back on Loading — a
        // refetch here would cost a subprocess every time the chevron is
        // poked.
        assert!(
            matches!(p.files_for("aaa1111"), Some(StashFilesState::Ready(_))),
            "re-expand should be served from cache, got {:?}",
            p.files_for("aaa1111")
        );
    });
}

/// A result that arrives after its row was collapsed is still cached. If it
/// were dropped, the `Loading` marker would stay behind — and since a sha
/// that already has an entry is never refetched, that row would be stuck on
/// "Loading…" with no way back short of a refresh.
#[gpui::test]
async fn a_result_landing_after_a_collapse_is_still_cached(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.toggle_expanded("aaa1111".into(), cx); // collapsed mid-flight
        p.apply_files_result("aaa1111", Ok(newer_files()), cx);

        assert!(matches!(
            p.files_for("aaa1111"),
            Some(StashFilesState::Ready(_))
        ));
        p.toggle_expanded("aaa1111".into(), cx);
        assert!(
            matches!(p.files_for("aaa1111"), Some(StashFilesState::Ready(_))),
            "re-expanding a row whose fetch landed late must not strand it"
        );
    });
}

/// The bug a single shared `Option<Task>` would cause. Two expands in flight
/// at once each need their own slot; with one slot the second cancels the
/// first, whose row then keeps a `Loading` marker nothing will ever replace.
#[gpui::test]
async fn two_rows_expanded_at_once_both_resolve(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.toggle_expanded("bbb2222".into(), cx);
        // Results arrive out of order, which is the realistic case — two
        // subprocesses race.
        p.apply_files_result("bbb2222", Ok(vec![file("alpha.txt", StashFileOrigin::Tracked)]), cx);
        p.apply_files_result("aaa1111", Ok(newer_files()), cx);

        assert_eq!(p.file_count("aaa1111"), Some(3));
        assert_eq!(p.file_count("bbb2222"), Some(1));
    });
}

/// An expansion is keyed by sha, so dropping a *different* stash — which
/// shifts every index below it — leaves it pointing at the same commit.
/// Keyed by index it would silently re-point at a neighbour.
#[gpui::test]
async fn an_expansion_survives_a_drop_that_shifts_the_stack(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("bbb2222".into(), cx);
        p.apply_files_result("bbb2222", Ok(vec![file("alpha.txt", StashFileOrigin::Tracked)]), cx);

        // stash@{0} is dropped out of band: `bbb2222` slides from {1} to {0}.
        p.apply_list_result(Ok(vec![entry(0, "bbb2222", "older")]), cx);

        assert!(p.is_expanded("bbb2222"), "the surviving row stayed open");
        assert!(
            !p.is_expanded("aaa1111"),
            "a dropped stash's expansion must be forgotten, not inherited by \
             whatever moves into its address"
        );
    });
}

#[gpui::test]
async fn a_refresh_invalidates_the_cache_and_reloads_open_rows(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(newer_files()), cx);
        assert_eq!(p.file_count("aaa1111"), Some(3));

        // A second list lands — which is what every op ends with.
        p.apply_list_result(Ok(two_stashes()), cx);

        // The stale list is gone, and the open row is reloading rather than
        // sitting blank behind an emptied cache.
        assert!(
            matches!(p.files_for("aaa1111"), Some(StashFilesState::Loading)),
            "an open row should refetch after a refresh, got {:?}",
            p.files_for("aaa1111")
        );
        assert!(
            p.files_for("bbb2222").is_none(),
            "a closed row should stay unfetched"
        );
    });
}

#[gpui::test]
async fn a_failed_refresh_leaves_the_expansion_alone(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(newer_files()), cx);

        // git failed; we learned nothing about the stack, so forgetting which
        // rows were open would be throwing away state for no reason.
        p.apply_list_result(Err("fatal: not a git repository".into()), cx);
        assert!(p.is_expanded("aaa1111"));
        assert_eq!(p.file_count("aaa1111"), Some(3));
    });
}

/// A collapsed section costs its sibling nothing, and an expanded one is
/// budgeted at the height the user chose — not the height being painted,
/// which is what makes the two-section fit a fixpoint instead of a flicker.
#[gpui::test]
async fn the_budget_reads_the_chosen_height_and_paints_the_trimmed_one(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        assert!(p.is_collapsed(), "the section starts collapsed");
        assert_eq!(p.chosen_height(), None, "and so asks for no height");

        p.toggle_collapsed(cx);
        assert_eq!(p.chosen_height(), Some(DEFAULT_STASH_HEIGHT));

        p.set_section_ceiling(120.0);
        assert_eq!(f32::from(p.painted_height()), 120.0, "the paint is trimmed");
        assert_eq!(
            p.chosen_height(),
            Some(DEFAULT_STASH_HEIGHT),
            "but the request is kept, so the section returns to size when \
             the room does"
        );
    });
}
