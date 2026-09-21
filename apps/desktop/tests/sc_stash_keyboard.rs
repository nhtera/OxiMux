//! The stash section's keyboard cursor and its list/tree layout toggle.
//!
//! Same constraint as `sc_stash_expand.rs`: nothing here enters a tokio
//! runtime, because `#[gpui::test]` panics the moment a tokio worker wakes a
//! GPUI task. The cursor and the layout are pure panel state driven through
//! the same entry points the real key bindings and the real toggle button call,
//! so what is proved is the state machine, not the keymap.
//!
//! What is NOT proved here, and is live-verified instead: that the five
//! bindings actually reach the panel (a `key_context` question, and on macOS a
//! `performKeyEquivalent:` question for `Cmd+Backspace`), and that the focus
//! the cursor needs survives a dialog round-trip.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
    px,
};
use oximux_app::scm_layout_settings::DEFAULT_STASH_HEIGHT;
use oximux_app::shell::stash_panel::keyboard::StashCursor;
use oximux_app::shell::stash_panel::{DropStashRequested, RenameStashRequested, StashPanel};
use oximux_core::{DiffStatus, StashEntry, StashFile, StashFileOrigin, StashRef, ViewMode};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Mutex;

struct Harness {
    inner: Entity<StashPanel>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

/// An empty repo is enough: nothing below shells out to git.
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
}

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

fn file(path: &str) -> StashFile {
    StashFile {
        path: PathBuf::from(path),
        status: DiffStatus::Modified,
        origin: StashFileOrigin::Tracked,
    }
}

fn two_stashes() -> Vec<StashEntry> {
    vec![entry(0, "aaa1111", "newer"), entry(1, "bbb2222", "older")]
}

/// Two files under one folder plus one at the top, so flat and tree differ.
fn nested_files() -> Vec<StashFile> {
    vec![file("src/a.txt"), file("src/b.txt"), file("top.txt")]
}

fn stash(sha: &str) -> StashCursor {
    StashCursor::Stash(sha.into())
}

fn file_at(sha: &str, path: &str) -> StashCursor {
    StashCursor::File {
        sha: sha.into(),
        path: PathBuf::from(path),
    }
}

// ── The visible-row list is what the cursor walks ─────────────────────────

#[gpui::test]
async fn a_collapsed_list_offers_only_its_stash_rows(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        assert_eq!(p.visible_rows(), vec![stash("aaa1111"), stash("bbb2222")]);
    });
}

#[gpui::test]
async fn an_expanded_stash_puts_its_files_into_the_walk(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        // Still loading: a note row is painted, but there is nothing on it to
        // act on, so it is not a cursor target.
        assert_eq!(p.visible_rows(), vec![stash("aaa1111"), stash("bbb2222")]);

        p.apply_files_result("aaa1111", Ok(nested_files()), cx);
        assert_eq!(
            p.visible_rows(),
            vec![
                stash("aaa1111"),
                file_at("aaa1111", "src/a.txt"),
                file_at("aaa1111", "src/b.txt"),
                file_at("aaa1111", "top.txt"),
                stash("bbb2222"),
            ],
        );
    });
}

// ── Movement ──────────────────────────────────────────────────────────────

#[gpui::test]
async fn the_first_keystroke_enters_the_list_from_the_end_it_is_heading_for(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        assert!(p.cursor().is_none());
        p.cursor_move(1, cx);
        assert_eq!(p.cursor(), Some(&stash("aaa1111")), "↓ into an empty cursor");
    });

    // A second panel rather than clearing the first: the panel exposes no way
    // to un-set a cursor, and adding one only so a test could would be an API
    // that exists for nobody.
    let fresh = mounted(tmp.path(), cx);
    fresh.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.cursor_move(-1, cx);
        assert_eq!(p.cursor(), Some(&stash("bbb2222")), "↑ into an empty cursor");
    });
}

#[gpui::test]
async fn arrowing_stops_at_both_ends_rather_than_wrapping(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.set_cursor(stash("aaa1111"), cx);
        // Wrapping a list that can trigger a destructive key from the cursor
        // is how `Cmd+Backspace` lands on the wrong end of the stack.
        p.cursor_move(-1, cx);
        assert_eq!(p.cursor(), Some(&stash("aaa1111")));
        p.cursor_move(1, cx);
        assert_eq!(p.cursor(), Some(&stash("bbb2222")));
        p.cursor_move(1, cx);
        assert_eq!(p.cursor(), Some(&stash("bbb2222")));
    });
}

#[gpui::test]
async fn the_cursor_walks_into_and_out_of_an_expansion(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);
        p.set_cursor(stash("aaa1111"), cx);

        p.cursor_move(1, cx);
        assert_eq!(p.cursor(), Some(&file_at("aaa1111", "src/a.txt")));
        for _ in 0..3 {
            p.cursor_move(1, cx);
        }
        assert_eq!(p.cursor(), Some(&stash("bbb2222")), "walked past the files");
    });
}

// ── Expand / collapse ─────────────────────────────────────────────────────

#[gpui::test]
async fn right_expands_and_left_collapses_a_stash_row(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.set_cursor(stash("aaa1111"), cx);

        p.cursor_expand(cx);
        assert!(p.is_expanded("aaa1111"));
        // Idempotent: a second `→` on an open row must not close it.
        p.cursor_expand(cx);
        assert!(p.is_expanded("aaa1111"));

        p.cursor_collapse(cx);
        assert!(!p.is_expanded("aaa1111"));
    });
}

#[gpui::test]
async fn left_on_a_file_row_climbs_to_the_stash_that_owns_it(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);
        p.set_cursor(file_at("aaa1111", "top.txt"), cx);

        p.cursor_collapse(cx);
        assert_eq!(p.cursor(), Some(&stash("aaa1111")));
        // And the stash is still open — climbing out is not closing.
        assert!(p.is_expanded("aaa1111"));
    });
}

// ── Cmd+Backspace goes through the same confirm step the glyph does ───────

#[gpui::test]
async fn the_drop_key_emits_a_request_rather_than_dropping(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    let seen: Rc<Mutex<Vec<String>>> = Rc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&panel, move |_panel, ev: &DropStashRequested, _cx| {
            sink.lock().unwrap().push(ev.sha.clone());
        })
    });

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.set_cursor(stash("bbb2222"), cx);
        p.cursor_drop(cx);
    });
    cx.run_until_parked();
    assert_eq!(*seen.lock().unwrap(), vec!["bbb2222".to_string()]);
}

#[gpui::test]
async fn the_drop_key_on_a_file_row_targets_the_stash_that_owns_it(cx: &mut TestAppContext) {
    // There is no per-file delete in a stash. Doing nothing on half the rows
    // would teach the user the binding is unreliable; dropping the owner is
    // the reading the confirm dialog then names out loud.
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    let seen: Rc<Mutex<Vec<String>>> = Rc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&panel, move |_panel, ev: &DropStashRequested, _cx| {
            sink.lock().unwrap().push(ev.sha.clone());
        })
    });

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);
        p.set_cursor(file_at("aaa1111", "top.txt"), cx);
        p.cursor_drop(cx);
    });
    cx.run_until_parked();
    assert_eq!(*seen.lock().unwrap(), vec!["aaa1111".to_string()]);
}

// ── The cursor survives what it has to survive ────────────────────────────

#[gpui::test]
async fn a_refresh_keeps_the_cursor_where_it_was(cx: &mut TestAppContext) {
    // The reason the cursor is panel state and not focus state: every op ends
    // in a forced refresh, and a cursor rebuilt from the new list each time
    // would snap back to the top after every keystroke that did anything.
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.set_cursor(stash("bbb2222"), cx);
        p.apply_list_result(Ok(two_stashes()), cx);
        assert_eq!(p.cursor(), Some(&stash("bbb2222")));
    });
}

#[gpui::test]
async fn a_cursor_on_a_dropped_stash_falls_back_to_the_first_row(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.set_cursor(stash("bbb2222"), cx);
        // `bbb2222` was dropped out from under the cursor.
        p.apply_list_result(Ok(vec![entry(0, "aaa1111", "newer")]), cx);
        assert_eq!(
            p.cursor(),
            Some(&stash("aaa1111")),
            "the keyboard path must not die with the row it was on",
        );
    });
}

#[gpui::test]
async fn an_emptied_list_leaves_no_cursor(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.set_cursor(stash("aaa1111"), cx);
        p.apply_list_result(Ok(Vec::new()), cx);
        assert!(p.cursor().is_none());
        assert!(p.visible_rows().is_empty());
    });
}

// ── Layout toggle ─────────────────────────────────────────────────────────

#[gpui::test]
async fn the_tree_layout_hides_the_files_under_a_collapsed_folder(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);
        assert_eq!(p.view_mode(), ViewMode::Flat);

        p.toggle_view_mode(cx);
        assert_eq!(p.view_mode(), ViewMode::Tree);
        // Folder rows are not cursor targets, so the walk is unchanged while
        // everything is open.
        assert_eq!(
            p.visible_rows(),
            vec![
                stash("aaa1111"),
                file_at("aaa1111", "src/a.txt"),
                file_at("aaa1111", "src/b.txt"),
                file_at("aaa1111", "top.txt"),
                stash("bbb2222"),
            ],
        );

        p.toggle_dir("aaa1111", PathBuf::from("src"), cx);
        assert_eq!(
            p.visible_rows(),
            vec![
                stash("aaa1111"),
                file_at("aaa1111", "top.txt"),
                stash("bbb2222"),
            ],
            "the cursor must not walk onto a row that is not painted",
        );
    });
}

#[gpui::test]
async fn collapsing_a_folder_in_one_stash_does_not_collapse_it_in_another(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_view_mode(cx);
        for sha in ["aaa1111", "bbb2222"] {
            p.toggle_expanded(sha.into(), cx);
            p.apply_files_result(sha, Ok(nested_files()), cx);
        }
        p.toggle_dir("aaa1111", PathBuf::from("src"), cx);

        assert!(p.is_dir_collapsed("aaa1111", &PathBuf::from("src")));
        assert!(
            !p.is_dir_collapsed("bbb2222", &PathBuf::from("src")),
            "two stashes sharing a folder name are not one folder",
        );
    });
}

#[gpui::test]
async fn flipping_to_flat_and_back_returns_the_tree_the_user_left(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);
        p.toggle_view_mode(cx);
        p.toggle_dir("aaa1111", PathBuf::from("src"), cx);

        p.toggle_view_mode(cx); // → flat
        p.toggle_view_mode(cx); // → tree again
        assert!(
            p.is_dir_collapsed("aaa1111", &PathBuf::from("src")),
            "the toggle re-opened folders the user had closed",
        );
    });
}

// ── Rename requests carry what the dialog needs ───────────────────────────

#[gpui::test]
async fn a_rename_request_carries_the_current_message_and_the_rewrite_depth(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    let seen: Rc<Mutex<Vec<(String, String, usize)>>> = Rc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&panel, move |_panel, ev: &RenameStashRequested, _cx| {
            sink.lock()
                .unwrap()
                .push((ev.sha.clone(), ev.message.clone(), ev.depth));
        })
    });

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        // The second row: its depth is 1, which is how many entries the
        // rename takes off and puts back, and what the dialog's copy counts.
        p.request_rename_stash("bbb2222", 1, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![("bbb2222".to_string(), "older".to_string(), 1)],
    );
}

#[gpui::test]
async fn a_rename_request_for_a_stash_that_has_left_the_stack_is_a_no_op(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    let seen: Rc<Mutex<usize>> = Rc::new(Mutex::new(0));
    let sink = seen.clone();
    let _sub = cx.update(|cx| {
        cx.subscribe(&panel, move |_panel, _ev: &RenameStashRequested, _cx| {
            *sink.lock().unwrap() += 1;
        })
    });

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.request_rename_stash("not-on-the-stack", 0, cx);
    });
    cx.run_until_parked();
    assert_eq!(*seen.lock().unwrap(), 0);
}

// ── Scroll-into-view accounting ───────────────────────────────────────────
//
// The body is bounded, so a cursor the list does not scroll to is a cursor
// the user cannot see. `painted_index` is what the scroll arithmetic reads,
// and it has to count the rows the cursor never lands on — folder rows and
// the one-line loading/failed/empty notes — because those still occupy
// vertical space. Found live with 30 stashes: arrowing past the tenth row
// moved the cursor off the bottom and nothing scrolled.

#[gpui::test]
async fn painted_index_counts_stash_rows_in_a_collapsed_list(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        assert_eq!(p.painted_index(&stash("aaa1111")), Some(0));
        assert_eq!(p.painted_index(&stash("bbb2222")), Some(1));
    });
}

#[gpui::test]
async fn painted_index_counts_an_expansions_file_rows(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);

        // 0: stash, 1..3: the three files, 4: the second stash.
        assert_eq!(p.painted_index(&stash("aaa1111")), Some(0));
        assert_eq!(p.painted_index(&file_at("aaa1111", "src/a.txt")), Some(1));
        assert_eq!(p.painted_index(&file_at("aaa1111", "top.txt")), Some(3));
        assert_eq!(p.painted_index(&stash("bbb2222")), Some(4));
    });
}

#[gpui::test]
async fn painted_index_counts_tree_folder_rows_the_cursor_skips(cx: &mut TestAppContext) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        p.toggle_view_mode(cx);
        p.toggle_expanded("aaa1111".into(), cx);
        p.apply_files_result("aaa1111", Ok(nested_files()), cx);

        // Painted: 0 stash, 1 `src`, 2 a.txt, 3 b.txt, 4 top.txt, 5 stash.
        // The cursor never lands on `src`, but it still takes a row.
        assert_eq!(p.painted_index(&file_at("aaa1111", "src/a.txt")), Some(2));
        assert_eq!(p.painted_index(&file_at("aaa1111", "top.txt")), Some(4));
        assert_eq!(p.painted_index(&stash("bbb2222")), Some(5));
        // The cursor's own walk is shorter — it skips the folder row. The two
        // indices differing is the whole reason `painted_index` exists.
        assert_eq!(
            p.visible_rows().iter().position(|r| *r == file_at("aaa1111", "top.txt")),
            Some(3),
        );
    });
}

#[gpui::test]
async fn painted_index_counts_the_one_line_note_a_loading_expansion_paints(
    cx: &mut TestAppContext,
) {
    let tmp = tempfile::tempdir().expect("tempdir");
    seed_repo(tmp.path());
    let panel = mounted(tmp.path(), cx);

    panel.update(cx, |p, cx| {
        p.apply_list_result(Ok(two_stashes()), cx);
        // Expanded, fetch still in flight: the body paints "Loading files…",
        // one row tall, which pushes the next stash down by one.
        p.toggle_expanded("aaa1111".into(), cx);
        assert_eq!(p.painted_index(&stash("bbb2222")), Some(2));
    });
}
