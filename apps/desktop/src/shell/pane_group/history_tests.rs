//! Per-terminal shell history, app side: the moments only the app sees.
//!
//! A split and a new tab in a pane must start from the source terminal's
//! history file, a new top-level tab from its worktree's most recently
//! focused terminal; a closed terminal's file must go; a torn-off (detached)
//! terminal's must stay. Real PTYs are spawned, running `/bin/sh` so no shell
//! history block races the files these tests plant and read.

use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use gpui::{Entity, TestAppContext};
use tempfile::TempDir;

use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::SplitSubPaneRight;
use crate::notifier::null::NullNotifier;
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal::shell_history;
use crate::shell::terminal_view::{TerminalView, set_spawn_shell};

fn make_group(cx: &mut TestAppContext) -> (gpui::WindowHandle<PaneGroup>, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    (group_at(dir.path().to_path_buf(), cx), dir)
}

fn group_at(cwd: PathBuf, cx: &mut TestAppContext) -> gpui::WindowHandle<PaneGroup> {
    cx.add_window(|_win, cx| {
        PaneGroup::new(
            cwd,
            Theme::default(),
            Density::default(),
            Typography::default(),
            Arc::new(CliRuntime::new()),
            Arc::new(NullNotifier),
            Arc::new(AtomicBool::new(true)),
            cx,
        )
    })
}

/// Every terminal view in the active tab, all leaves and per-pane tabs.
fn views(window: &gpui::WindowHandle<PaneGroup>, cx: &mut TestAppContext) -> Vec<Entity<TerminalView>> {
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        let Some(PaneContent::Terminal(tree)) = group.active_tab().map(|t| &t.content) else {
            return Vec::new();
        };
        tree.iter_all_views().map(|(_, _, v)| v.clone()).collect()
    })
}

fn tab_id(view: &Entity<TerminalView>, cx: &mut TestAppContext) -> String {
    cx.read(|app| view.read(app).tab_id().to_string())
}

fn zsh_file(tab: &str) -> PathBuf {
    shell_history::history_dir().expect("history dir").join(format!("{tab}.zsh_history"))
}

/// Run `f` with plain `/bin/sh` as the spawn shell, restoring the default.
/// Serialized with every other test that touches the process-wide shell.
fn with_plain_sh(f: impl FnOnce()) {
    let _serial = crate::platform::serialize_input_state();
    set_spawn_shell("/bin/sh".to_string());
    f();
    set_spawn_shell(String::new());
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[gpui::test]
async fn a_split_and_a_new_tab_in_the_pane_start_from_the_source_history(cx: &mut TestAppContext) {
    with_plain_sh(|| {
        let (window, _dir) = make_group(cx);
        window
            .update(cx, |g, win, cx| g.open_terminal_tab(win, cx))
            .unwrap()
            .expect("PTY spawn");
        cx.run_until_parked();
        let parent = tab_id(&views(&window, cx)[0], cx);
        std::fs::write(zsh_file(&parent), ": 1:0;echo from-parent\n").unwrap();

        window
            .update(cx, |g, win, cx| g.on_split_sub_pane_right(&SplitSubPaneRight, win, cx))
            .unwrap();
        window.update(cx, |g, win, cx| g.add_tab_to_leaf(0, win, cx)).unwrap();
        cx.run_until_parked();

        let children: Vec<String> = views(&window, cx)
            .iter()
            .map(|v| tab_id(v, cx))
            .filter(|id| *id != parent)
            .collect();
        assert_eq!(children.len(), 2, "split + new tab in pane");
        for child in children {
            assert_eq!(
                std::fs::read_to_string(zsh_file(&child)).unwrap(),
                ": 1:0;echo from-parent\n",
                "{child} did not start from its parent's history"
            );
        }
    });
}

#[gpui::test]
async fn closing_a_terminal_deletes_its_history_but_a_detach_does_not(cx: &mut TestAppContext) {
    with_plain_sh(|| {
        let (window, _dir) = make_group(cx);
        for _ in 0..2 {
            window
                .update(cx, |g, win, cx| g.open_terminal_tab(win, cx))
                .unwrap()
                .expect("PTY spawn");
        }
        cx.run_until_parked();
        let (closed, torn_off) = cx.read(|app| {
            let group = window.read(app).unwrap();
            let id = |i: usize| match &group.tabs()[i].content {
                PaneContent::Terminal(tree) => tree.active_view().unwrap().read(app).tab_id().to_string(),
                _ => unreachable!(),
            };
            (id(0), id(1))
        });
        for tab in [&closed, &torn_off] {
            std::fs::write(zsh_file(tab), "x\n").unwrap();
        }
        // Tab 1 moves to another window: detached first, then dropped.
        cx.read(|app| match &window.read(app).unwrap().tabs()[1].content {
            PaneContent::Terminal(tree) => tree.active_view().unwrap().read(app).detach(),
            _ => unreachable!(),
        });
        window
            .update(cx, |g, _win, cx| {
                let _ = g.take_tab(1, cx);
                let _ = g.take_tab(0, cx);
            })
            .unwrap();
        cx.run_until_parked();

        wait_until("the closed terminal's history to go", || !zsh_file(&closed).exists());
        // Past the close-settle window, the detached terminal's is intact.
        std::thread::sleep(Duration::from_secs(3));
        assert!(zsh_file(&torn_off).exists(), "a tear-off deleted live history");
        assert!(!zsh_file(&closed).exists(), "the delayed re-delete must not resurrect");
    });
}

#[gpui::test]
async fn a_new_tab_starts_from_its_own_worktree_not_the_last_one_used(cx: &mut TestAppContext) {
    with_plain_sh(|| {
        // Two worktrees of one repo (a checkout and a linked one), plus an
        // unrelated repo with no terminal yet.
        let root = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("a/.git")).unwrap();
        std::fs::create_dir_all(root.path().join("b")).unwrap();
        std::fs::write(root.path().join("b/.git"), "gitdir: ../a/.git/worktrees/b\n").unwrap();
        std::fs::create_dir_all(root.path().join("c/.git")).unwrap();
        let open = |window: &gpui::WindowHandle<PaneGroup>, cx: &mut TestAppContext| {
            // Focus events only fire in the active window.
            window.update(cx, |_, win, _| win.activate_window()).unwrap();
            window.update(cx, |g, win, cx| g.open_terminal_tab(win, cx)).unwrap().expect("PTY spawn");
            cx.run_until_parked();
            tab_id(&views(window, cx)[0], cx)
        };
        // Work in B, then in A: A's terminal is the most recently focused.
        let (a, b) = (group_at(root.path().join("a"), cx), group_at(root.path().join("b"), cx));
        let in_b = open(&b, cx);
        let in_a = open(&a, cx);
        std::fs::write(zsh_file(&in_b), ": 1:0;echo from-b\n").unwrap();
        std::fs::write(zsh_file(&in_a), ": 1:0;echo from-a\n").unwrap();
        // The newest terminal in B keeps no history (an agent CLI, a shell
        // that stood down): it has nothing to give, so B's older one wins.
        let no_history = open(&b, cx);
        let _ = std::fs::remove_file(zsh_file(&no_history));

        let new_in_b = open(&b, cx);
        assert_eq!(
            std::fs::read_to_string(zsh_file(&new_in_b)).ok().as_deref(),
            Some(": 1:0;echo from-b\n"),
            "a new tab in B must start from B's terminal, not A's"
        );
        // No terminal in C yet: it starts from the user's own history.
        let new_in_c = open(&group_at(root.path().join("c"), cx), cx);
        assert!(!zsh_file(&new_in_c).exists(), "C copied another worktree's history");
    });
}
