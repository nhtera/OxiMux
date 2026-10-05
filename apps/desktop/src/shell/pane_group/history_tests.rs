//! Per-terminal shell history, app side: the moments only the app sees.
//!
//! A split and a new tab in a pane must start from the source terminal's
//! history file; a closed terminal's file must go; a torn-off (detached)
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
    let cwd = dir.path().to_path_buf();
    let window = cx.add_window(|_win, cx| {
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
    });
    (window, dir)
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
