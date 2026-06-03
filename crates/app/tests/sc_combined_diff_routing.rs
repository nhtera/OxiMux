//! Regression test for combined-diff staging routing.
//!
//! In a combined multi-file view, the floating Stage/Discard card's action
//! side is resolved PER FILE from its group tag — not a single side for the
//! whole view. This pins the mapping so a regression can't silently make a
//! `Staged` file offer Stage (instead of Unstage) or a `Committed` file
//! offer any chip:
//!
//! - `Unstaged`  → Stage / Discard   (staged=false, untracked=false)
//! - `Staged`    → Unstage           (staged=true,  untracked=false)
//! - `Untracked` → whole-file stage  (staged=false, untracked=true)
//! - `Committed` → read-only, no card (`None`)
//!
//! `side_for_region(file_idx)` is what the renderer gates the card on, so
//! asserting it directly covers the routing without pumping the async load.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, TestAppContext, Window, div,
};
use oximux_app::shell::diff_view::DiffView;
use oximux_core::{
    CombinedDiffScope, DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff, FileGroup,
};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

struct Harness {
    inner: Entity<DiffView>,
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

fn empty_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    Command::new("git")
        .args(["init", "-b", "main", "--quiet"])
        .current_dir(dir.path())
        .status()
        .expect("git on PATH");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let repo = rt.block_on(Repository::open(dir.path())).unwrap();
    (dir, repo)
}

/// A trivial one-line modified `FileDiff` — `side_for_region` only reads the
/// group tag, so the body content is irrelevant.
fn file(name: &str) -> FileDiff {
    FileDiff {
        path: PathBuf::from(name),
        status: DiffStatus::Modified,
        hunks: vec![DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            header_suffix: String::new(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Added,
                content: "x".to_string(),
            }],
        }],
        large: false,
    }
}

#[gpui::test]
fn combined_side_resolves_per_file_group(cx: &mut TestAppContext) {
    let (_dir, repo) = empty_repo();
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));

    // Four files, one per group, in a fixed order.
    let diffs = vec![
        file("unstaged.txt"),
        file("staged.txt"),
        file("untracked.txt"),
        file("committed.txt"),
    ];
    let groups = vec![
        FileGroup::Unstaged,
        FileGroup::Staged,
        FileGroup::Untracked,
        FileGroup::Committed,
    ];

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
            v.seed_combined_for_test(CombinedDiffScope::AllChanges, diffs.clone(), groups.clone());
        });
        Harness { inner: view }
    });

    window
        .update(cx, |harness, _win, cx| {
            harness.inner.update(cx, |view, _cx| {
                // Unstaged → Stage/Discard.
                let s0 = view.side_for_region(0).expect("unstaged has a side");
                assert!(!s0.staged && !s0.untracked, "unstaged → stage side");

                // Staged → Unstage.
                let s1 = view.side_for_region(1).expect("staged has a side");
                assert!(s1.staged && !s1.untracked, "staged → unstage side");

                // Untracked → whole-file stage.
                let s2 = view.side_for_region(2).expect("untracked has a side");
                assert!(!s2.staged && s2.untracked, "untracked → untracked side");

                // Committed → read-only, no card.
                assert!(
                    view.side_for_region(3).is_none(),
                    "committed → no staging card"
                );

                // Out-of-range → None.
                assert!(view.side_for_region(99).is_none());
            });
        })
        .expect("inspect side_for_region");
}
