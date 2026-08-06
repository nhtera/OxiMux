//! GPUI headless E2E tests for per-pane tab strip and Cmd+W cascade.
//!
//! These live inside the crate (not in `apps/desktop/tests/`) so they can
//! reach `pub(crate)` handlers on `PaneGroup` — specifically
//! `on_close_tab` and `on_split_sub_pane_right`.
//!
//! Each test boots a minimal `PaneGroup` in a `TestAppContext` window,
//! drives it through the GPUI synchronous-effect machinery (no real frame
//! render, no display), and asserts on structural state only — tab counts,
//! leaf-tab counts, live sub-pane counts. PTY output bytes are NOT checked
//! here to avoid timing-sensitive flake; the relay integration suite covers
//! that path.
//!
//! Real PTY children ARE spawned (via the fallback `PortablePtyBackend`)
//! because `spawn_local_pty` falls through to it when the relay shared
//! backend is not installed. Keeping the tab count small (≤ 3 shells) caps
//! CI resource use.
//!
//! Which is why every `open_terminal_tab` call here asserts its return value.
//! It is an `Option`, `None` when the PTY spawn failed, and a spawn can fail on
//! a loaded machine for reasons that have nothing to do with the code under
//! test. Discarding it does not make the test pass — it makes the test fail
//! somewhere else, several assertions later, as a tab that is simply absent.
//! That cost real diagnosis time once: a CI run reported
//! `["Terminal 1"] != ["Terminal 1", "Terminal 2"]` from a restore-ordering
//! assertion, which reads as a reordering bug and was a failed spawn.

use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};

use gpui::{AppContext, Context, Entity, Subscription, TestAppContext, Window};
use tempfile::TempDir;

use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{CloseTab, SplitSubPaneRight};
use crate::keymap_registry::default_bindings as default_key_bindings;
use crate::notifier::null::NullNotifier;
use crate::persisted_terminals::{PersistedAxis, PersistedTree};
use crate::shell::context_env::SurfaceIds;
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::PaneGroup;
use crate::shell::pane_group::sub_pane::TerminalSplitTree;
use crate::shell::terminal_view::{TerminalView, TerminalViewEvent, spawn_local_pty_dormant};

/// Convenience: boot a `PaneGroup` entity inside a GPUI test window.
/// Returns `(window, temp_dir)`. `temp_dir` must stay alive for the
/// duration of the test so the cwd path remains valid.
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

// ── open_terminal_tab produces exactly one group tab ──────────────────────

#[gpui::test]
async fn open_terminal_tab_increments_tab_count(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    cx.run_until_parked();

    // Confirm the group starts empty.
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(group.tab_count(), 0, "new group should have 0 tabs");
    });

    // Open one terminal tab. spawn_local_pty falls back to PortablePtyBackend
    // since install_shared_backend is not called in tests.
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("open_terminal_tab must succeed (PTY fallback)");

    cx.run_until_parked();

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.tab_count(),
            1,
            "tab_count should be 1 after open_terminal_tab"
        );
        let tab = group.active_tab().expect("active tab");
        assert!(
            matches!(tab.content, PaneContent::Terminal(_)),
            "content should be PaneContent::Terminal"
        );
    });
}

// ── add_tab_to_leaf grows the leaf's per-pane tab strip ───────────────────

#[gpui::test]
async fn add_tab_to_leaf_grows_leaf_tab_count(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);

    // Open first group tab (creates one terminal leaf with 1 per-pane tab).
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();

    // Add a second per-pane tab to leaf 0.
    window
        .update(cx, |group, win, cx| group.add_tab_to_leaf(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        // Still one group tab.
        assert_eq!(group.tab_count(), 1, "group tab count must not change");

        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("content must be Terminal");
        };
        let leaf = tree
            .active_leaf()
            .expect("active leaf must exist after add_tab_to_leaf");
        assert_eq!(
            leaf.len(),
            2,
            "leaf should hold 2 per-pane tabs after add_tab_to_leaf"
        );
    });
}

// ── Cmd+W cascade: per-pane tab closes first, group tab survives ──────────

#[gpui::test]
async fn close_tab_cascade_closes_per_pane_tab_before_group_tab(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);

    // Build the state: one group tab, leaf 0 has 2 per-pane tabs.
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.add_tab_to_leaf(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    // Confirm pre-condition: 2 per-pane tabs in the leaf.
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal");
        };
        assert_eq!(
            tree.active_leaf().expect("leaf").len(),
            2,
            "pre-condition: leaf must have 2 tabs"
        );
    });

    // Fire the close cascade. With 2 per-pane tabs in the active leaf,
    // on_close_tab should close the per-pane tab first (step 1 in the
    // cascade) and leave the group tab alive.
    window
        .update(cx, |group, win, cx| {
            group.on_close_tab(&CloseTab, win, cx);
        })
        .expect("window update ok");
    cx.run_until_parked();

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");

        // The group tab must survive (cascade stopped at per-pane tab).
        assert_eq!(
            group.tab_count(),
            1,
            "group tab must survive after closing one per-pane tab"
        );

        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal content");
        };
        assert_eq!(
            tree.active_leaf().expect("leaf").len(),
            1,
            "leaf should have exactly 1 per-pane tab after cascade close"
        );
    });
}

// ── auto-close: a clean exit closes a lone-view terminal tab ───────────────

/// Read the active tab's sole terminal view + its session id.
fn active_lone_view(
    window: &gpui::WindowHandle<PaneGroup>,
    cx: &mut TestAppContext,
) -> (Entity<TerminalView>, oximux_pty::TerminalSessionId) {
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal content");
        };
        let view = tree.active_view().expect("active view").clone();
        let session = view.read(app).session_id();
        (view, session)
    })
}

#[gpui::test]
async fn clean_exit_auto_closes_lone_terminal_tab(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();

    let (view, session) = active_lone_view(&window, cx);
    assert_eq!(
        cx.read(|app| window.read(app).expect("alive").tab_count()),
        1,
        "pre-condition: exactly one terminal tab"
    );

    // Emit the clean-exit signal the way `tick` does on status 0, then drain.
    cx.update(|cx| {
        view.update(cx, |_v, cx| {
            cx.emit(TerminalViewEvent::CleanExit {
                session_id: session,
            })
        })
    });
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.close_lone_exited_tabs(win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    assert_eq!(
        cx.read(|app| window.read(app).expect("alive").tab_count()),
        0,
        "lone-view terminal tab must auto-close on a clean exit"
    );
}

#[gpui::test]
async fn clean_exit_closes_split_leaf_keeps_tab(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();
    // Split → the tab now hosts two live sub-panes (leaves).
    window
        .update(cx, |group, win, cx| {
            group.on_split_sub_pane_right(&SplitSubPaneRight, win, cx);
        })
        .expect("window update ok");
    cx.run_until_parked();

    // The exited view's leaf must drop, but the group tab survives.
    let (view, session) = active_lone_view(&window, cx);
    cx.update(|cx| {
        view.update(cx, |_v, cx| {
            cx.emit(TerminalViewEvent::CleanExit {
                session_id: session,
            })
        })
    });
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.close_lone_exited_tabs(win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(group.tab_count(), 1, "split tab itself must survive");
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal content");
        };
        assert_eq!(
            tree.live_count(),
            1,
            "the exited sub-pane leaf must be closed, leaving one"
        );
    });
}

#[gpui::test]
async fn clean_exit_closes_stacked_leaf_tab_keeps_tab(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();
    // Add a second per-pane tab stacked in the same leaf (no split).
    window
        .update(cx, |group, win, cx| group.add_tab_to_leaf(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    // The newly added tab is active; exit it → just that leaf-tab drops.
    let (view, session) = active_lone_view(&window, cx);
    cx.update(|cx| {
        view.update(cx, |_v, cx| {
            cx.emit(TerminalViewEvent::CleanExit {
                session_id: session,
            })
        })
    });
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.close_lone_exited_tabs(win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(group.tab_count(), 1, "group tab must survive");
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal content");
        };
        assert_eq!(
            tree.active_leaf().expect("leaf").len(),
            1,
            "the exited leaf-tab must be closed, leaving one"
        );
    });
}

// ── split produces 2 live sub-panes ───────────────────────────────────────

#[gpui::test]
async fn split_sub_pane_right_creates_second_live_pane(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);

    // One group tab → one leaf.
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();

    // Split the active leaf horizontally.
    window
        .update(cx, |group, win, cx| {
            group.on_split_sub_pane_right(&SplitSubPaneRight, win, cx);
        })
        .expect("window update ok");
    cx.run_until_parked();

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.tab_count(),
            1,
            "group tab count must remain 1 after split"
        );

        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal after split");
        };
        assert_eq!(
            tree.live_count(),
            2,
            "split must produce exactly 2 live sub-panes"
        );
    });
}

// ── from_persisted rebuilds a split with a multi-tab leaf ──────────────────
//
// The boot-time restore primitive: given a persisted tree shape + per-leaf
// tab lists, `TerminalSplitTree::from_persisted` must rebuild the exact
// structure — a 2-leaf split where leaf 0 carries a 2-tab per-pane strip
// (active = the second tab) and leaf 1 is single-tab. This is the seam
// `build_multi_sub_pane_tree` drives on every multi-sub-pane restore; the
// fast single-view path would collapse leaf 0's strip to one tab, so the
// per-leaf tab count + active index + the round-tripped surface/tab ids are
// the contract worth pinning. Views are spawned DORMANT (grid emulator, no
// PTY child) exactly as the restore path does, so the test stays cheap.
#[gpui::test]
async fn from_persisted_rebuilds_split_with_multi_tab_leaf(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);

    window
        .update(cx, |_group, win, cx| {
            // Build one dormant terminal view carrying a restored identity.
            let build = |surface: &str,
                         tab: &str,
                         win: &mut Window,
                         cx: &mut Context<PaneGroup>|
             -> (Entity<TerminalView>, Subscription) {
                let (backend, session_id) =
                    spawn_local_pty_dormant(80, 24).expect("dormant spawn (PTY fallback)");
                let ids = SurfaceIds::restored(
                    "/proj/root".to_string(),
                    surface.to_string(),
                    tab.to_string(),
                );
                let view = cx.new(|cx| {
                    TerminalView::mount_dormant(
                        backend,
                        session_id,
                        ids,
                        PathBuf::from("/tmp"),
                        &[],
                        Theme::default(),
                        Density::default(),
                        Typography::default(),
                        win,
                        cx,
                    )
                });
                let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
                (view, observer)
            };

            // Leaf 0: two-tab strip, active = second tab. Leaf 1: single tab.
            let leaves = vec![
                (
                    vec![
                        build("s0-0", "t0-0", win, cx),
                        build("s0-1", "t0-1", win, cx),
                    ],
                    1usize,
                ),
                (vec![build("s1-0", "t1-0", win, cx)], 0usize),
            ];

            let proto = PersistedTree::Split {
                axis: PersistedAxis::Horizontal,
                children: vec![PersistedTree::Leaf, PersistedTree::Leaf],
                weights: vec![0.5, 0.5],
            };
            let tree = TerminalSplitTree::from_persisted(&proto, leaves, 0);

            // Structure: two live leaves.
            assert_eq!(tree.live_count(), 2, "split must restore 2 live leaves");

            // Leaf 0 keeps its full 2-tab strip + restored active index.
            let leaf0 = tree.leaf(0).expect("leaf 0 present");
            assert_eq!(leaf0.len(), 2, "leaf 0 must keep both per-pane tabs");
            assert_eq!(leaf0.active(), 1, "leaf 0 active tab index preserved");

            // Leaf 1 is single-tab.
            let leaf1 = tree.leaf(1).expect("leaf 1 present");
            assert_eq!(leaf1.len(), 1, "leaf 1 must be single-tab");

            // Identity round-trips onto the rebuilt views (leaf 0, tab 1).
            let v01 = leaf0.tabs()[1].view().read(cx);
            assert_eq!(v01.surface_id(), "s0-1", "surface id must round-trip");
            assert_eq!(v01.tab_id(), "t0-1", "tab id must round-trip");
        })
        .expect("window update ok");
}

// ── tear-off eligibility blocks multi-tab leaves and multi-leaf splits ─────
//
// Cross-window tear-off moves exactly ONE relay-backed PTY, and the source
// collect path follows only each leaf's ACTIVE view. So a multi-tab leaf or
// a multi-leaf split MUST report ineligible — otherwise tearing one off
// would orphan the background/other-leaf PTYs in the daemon (a data-loss
// edge). This pins `tab_can_tear_off` against that regression. Views are
// dormant (in-process fallback), so the relay-backed condition is supplied
// explicitly via the bool argument.
#[gpui::test]
async fn tear_off_eligibility_blocks_multi_tab_and_multi_leaf(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);

    window
        .update(cx, |_group, win, cx| {
            let build = |win: &mut Window,
                         cx: &mut Context<PaneGroup>|
             -> (Entity<TerminalView>, Subscription) {
                let (backend, session_id) =
                    spawn_local_pty_dormant(80, 24).expect("dormant spawn (PTY fallback)");
                let ids = SurfaceIds::restored("/w".to_string(), String::new(), String::new());
                let view = cx.new(|cx| {
                    TerminalView::mount_dormant(
                        backend,
                        session_id,
                        ids,
                        PathBuf::from("/tmp"),
                        &[],
                        Theme::default(),
                        Density::default(),
                        Typography::default(),
                        win,
                        cx,
                    )
                });
                let observer = cx.observe(&view, |_t, _v, cx| cx.notify());
                (view, observer)
            };

            // Single leaf, single tab: eligible IFF relay-backed.
            let single = TerminalSplitTree::from_persisted(
                &PersistedTree::Leaf,
                vec![(vec![build(win, cx)], 0)],
                0,
            );
            assert!(
                crate::workspace_root::tab_can_tear_off(&single, true),
                "single relay-backed leaf is eligible"
            );
            assert!(
                !crate::workspace_root::tab_can_tear_off(&single, false),
                "in-process tab (no external id) is ineligible"
            );

            // Single leaf, TWO per-pane tabs: blocked — moving it would orphan
            // the background tab's PTY.
            let multi_tab = TerminalSplitTree::from_persisted(
                &PersistedTree::Leaf,
                vec![(vec![build(win, cx), build(win, cx)], 0)],
                0,
            );
            assert!(
                !crate::workspace_root::tab_can_tear_off(&multi_tab, true),
                "multi-tab leaf must be ineligible"
            );

            // TWO leaves (split): blocked — multi-leaf tear-off is out of scope.
            let split = TerminalSplitTree::from_persisted(
                &PersistedTree::Split {
                    axis: PersistedAxis::Horizontal,
                    children: vec![PersistedTree::Leaf, PersistedTree::Leaf],
                    weights: vec![0.5, 0.5],
                },
                vec![(vec![build(win, cx)], 0), (vec![build(win, cx)], 0)],
                0,
            );
            assert!(
                !crate::workspace_root::tab_can_tear_off(&split, true),
                "multi-leaf split must be ineligible"
            );
        })
        .expect("window update ok");
}

// ── keymap-driven E2E: the real keystroke → keymap → action dispatch path ──
//
// The tests above call handlers directly; these drive the PRODUCTION keymap
// via `simulate_keystrokes`, so they also prove the binding itself is wired
// (a regression that rebinds or drops cmd-w would slip past a direct-handler
// test but fail here). The bindings use a global (None) context, and the
// action handlers sit on the PaneGroup root div — an ancestor of the focused
// terminal view — so the action bubbles up to them.

#[gpui::test]
async fn keymap_cmd_w_closes_per_pane_tab_first(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    cx.update(|cx| cx.bind_keys(default_key_bindings()));

    // Set up: one group tab whose active leaf has two per-pane tabs.
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.add_tab_to_leaf(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    // cmd-w drives the close cascade: per-pane tab first, group tab survives.
    cx.simulate_keystrokes(window.into(), "secondary-w");

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.tab_count(),
            1,
            "group tab must survive the first cmd-w"
        );
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal");
        };
        assert_eq!(
            tree.active_leaf().expect("leaf").len(),
            1,
            "cmd-w must close a per-pane tab first via the keymap"
        );
    });
}

/// Live rebind through the production path: `apply_live` must make the new
/// chord dispatch AND dead-stop the old one via its NoAction shadow — the
/// boot keymap is never cleared, so shadow precedence is what guarantees a
/// moved chord stops firing.
#[gpui::test]
async fn keymap_apply_live_moves_a_binding_and_kills_the_old_chord(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    cx.update(|cx| cx.bind_keys(default_key_bindings()));

    // Two per-pane tabs so each close has an observable effect.
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok")
        .expect("PTY spawn must succeed, or the tab this test needs is missing");
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.add_tab_to_leaf(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    // Move close_tab cmd-w → cmd-e (sync the registry's effective map to
    // defaults first so the diff is exactly this one change regardless of
    // other tests sharing the process-global state).
    cx.update(|cx| {
        crate::keymap_registry::apply_live(cx, &std::collections::BTreeMap::new());
        let overrides = std::collections::BTreeMap::from([(
            "close_tab".to_string(),
            "secondary-e".to_string(),
        )]);
        let warnings = crate::keymap_registry::apply_live(cx, &overrides);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    });

    // Old chord must be dead (shadowed), leaving both per-pane tabs alive.
    cx.simulate_keystrokes(window.into(), "secondary-w");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal");
        };
        assert_eq!(
            tree.active_leaf().expect("leaf").len(),
            2,
            "cmd-w must stop dispatching after the rebind"
        );
    });

    // New chord drives the same close cascade.
    cx.simulate_keystrokes(window.into(), "secondary-e");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        let tab = group.active_tab().expect("active tab");
        let PaneContent::Terminal(tree) = &tab.content else {
            panic!("expected Terminal");
        };
        assert_eq!(
            tree.active_leaf().expect("leaf").len(),
            1,
            "cmd-e must close a per-pane tab after the rebind"
        );
    });

    // Restore defaults so the shared effective map can't surprise another
    // test in this binary.
    cx.update(|cx| {
        crate::keymap_registry::apply_live(cx, &std::collections::BTreeMap::new());
    });
}

// ── cross-group drop slot landing: append-then-move-to-slot ───────────────
//
// `ProjectPanes::transfer_tab_at` appends the stolen tab to the END of the
// destination's visible order (via `push_existing_tab`) then slides it back
// to the slot the insertion bar previewed with `move_tab(last, slot)`. This
// pins the visible ordering that operation produces for slots 0 / middle /
// end so a cross-group strip drop lands where the cursor aimed.

/// Visible tab labels in order — the strip's left-to-right reading.
fn visible_labels(group: &PaneGroup) -> Vec<String> {
    group
        .visible_tabs()
        .map(|(_, t)| t.label.to_string())
        .collect()
}

#[gpui::test]
async fn move_appended_tab_lands_at_requested_slot(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    for _ in 0..3 {
        window
            .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
            .expect("window update ok")
            .expect("PTY spawn must succeed, or the tab this test needs is missing");
    }
    cx.run_until_parked();

    // Baseline visible order is the spawn order.
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            visible_labels(group),
            vec!["Terminal 1", "Terminal 2", "Terminal 3"],
        );
    });

    // The just-appended tab sits at the last visible slot (idx 2). Sliding
    // it to slot 0 mirrors a cross-group drop on the FIRST chip.
    window
        .update(cx, |group, _win, _cx| group.move_tab(2, 0))
        .expect("window update ok");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            visible_labels(group),
            vec!["Terminal 3", "Terminal 1", "Terminal 2"],
            "drop on first chip must land the moved tab at slot 0",
        );
    });

    // Slide it to a middle slot (1) — drop on the SECOND chip.
    window
        .update(cx, |group, _win, _cx| group.move_tab(0, 1))
        .expect("window update ok");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            visible_labels(group),
            vec!["Terminal 1", "Terminal 3", "Terminal 2"],
            "drop on the middle chip must land the moved tab at slot 1",
        );
    });
}

#[gpui::test]
async fn move_into_pinned_cluster_clamps_to_unpinned_zone(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    for _ in 0..3 {
        window
            .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
            .expect("window update ok")
            .expect("PTY spawn must succeed, or the tab this test needs is missing");
    }
    cx.run_until_parked();

    // Pin the first tab (insertion idx 0). It clusters at the front; an
    // unpinned tab can't slide ahead of it.
    window
        .update(cx, |group, _win, cx| group.toggle_pin(0, cx))
        .expect("window update ok");
    cx.run_until_parked();

    // Try to drop the last tab (Terminal 3) ahead of the pinned one (slot 0).
    // `move_tab` clamps to the unpinned bucket → it lands at slot 1, just
    // after the pinned cluster, never inside it.
    window
        .update(cx, |group, _win, _cx| group.move_tab(2, 0))
        .expect("window update ok");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        let labels = visible_labels(group);
        assert_eq!(labels[0], "Terminal 1", "pinned tab stays at the front");
        assert_eq!(
            labels[1], "Terminal 3",
            "unpinned tab clamps to the first unpinned slot, not into the pinned cluster",
        );
    });
}

// ── Preview tab: single-click reuses one tab; edit/double-click promotes ───

#[gpui::test]
async fn preview_editor_tab_is_reused_then_promoted(cx: &mut TestAppContext) {
    let (window, dir) = make_group(cx);
    // Editor views are gpui-component `Input` widgets, which need the
    // component theme global the production app installs at boot.
    cx.update(gpui_component::init);
    let file_a = dir.path().join("alpha.txt");
    let file_b = dir.path().join("beta.txt");
    std::fs::write(&file_a, "alpha\n").expect("write alpha");
    std::fs::write(&file_b, "beta\n").expect("write beta");

    // Single-click file A → exactly one italic preview tab.
    window
        .update(cx, |group, win, cx| {
            group.open_preview_editor_tab(file_a.clone(), win, cx);
        })
        .expect("window update ok");
    cx.run_until_parked();
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(group.tab_count(), 1, "preview open creates exactly one tab");
        assert!(
            group.active_tab().expect("active").is_preview,
            "a single-click file opens as a preview tab",
        );
    });

    // Single-click file B → reuses the SAME preview slot (browsing the tree
    // must not pile up tabs).
    window
        .update(cx, |group, win, cx| {
            group.open_preview_editor_tab(file_b.clone(), win, cx);
        })
        .expect("window update ok");
    cx.run_until_parked();
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.tab_count(),
            1,
            "a second preview reuses the slot instead of opening a new tab",
        );
        let tab = group.active_tab().expect("active");
        assert!(tab.is_preview, "the reused tab stays a preview");
        assert!(
            group.editor_tab_index(&file_b).is_some(),
            "the preview now hosts file B",
        );
        assert!(
            group.editor_tab_index(&file_a).is_none(),
            "file A is no longer open (its preview slot was reused)",
        );
    });

    // Promote (the edit / double-click affordance) → permanent.
    window
        .update(cx, |group, _win, cx| group.promote_tab_to_permanent(0, cx))
        .expect("window update ok");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert!(
            !group.active_tab().expect("active").is_preview,
            "promotion clears the preview flag",
        );
    });
}

// ── External mutation: deleting an open file on disk flags the tab, never
//    auto-closes it; restoring the file clears the flag. ────────────────────

#[gpui::test]
async fn external_mutation_sweep_flags_deleted_and_clears_on_restore(cx: &mut TestAppContext) {
    use crate::shell::pane_group::ExternalMutation;

    let (window, dir) = make_group(cx);
    // Editor views need the gpui-component theme global (see preview test).
    cx.update(gpui_component::init);
    let file = dir.path().join("watched.txt");
    std::fs::write(&file, "content\n").expect("write file");

    window
        .update(cx, |group, win, cx| {
            group.open_or_activate_editor_tab(file.clone(), win, cx);
        })
        .expect("window update ok");
    cx.run_until_parked();
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.active_tab().expect("active").external_mutation,
            None,
            "a live file is unflagged",
        );
    });

    // Delete on disk, then sweep → flagged Deleted, tab + buffer preserved.
    std::fs::remove_file(&file).expect("remove file");
    window
        .update(cx, |group, _win, cx| group.sweep_external_mutations(cx))
        .expect("window update ok");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.tab_count(),
            1,
            "a deleted-on-disk file is NOT auto-closed",
        );
        assert_eq!(
            group.active_tab().expect("active").external_mutation,
            Some(ExternalMutation::Deleted),
            "a vanished file is flagged Deleted",
        );
    });

    // Restore on disk, sweep again → the flag clears.
    std::fs::write(&file, "back\n").expect("rewrite file");
    window
        .update(cx, |group, _win, cx| group.sweep_external_mutations(cx))
        .expect("window update ok");
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(
            group.active_tab().expect("active").external_mutation,
            None,
            "a restored file clears the Deleted flag",
        );
    });
}

// ── Restore: tabs settle into the saved visual order regardless of mount
//    order, and saved cosmetics (preview/pin/color/title) come back ───────

#[gpui::test]
async fn restored_tabs_settle_into_saved_order_and_cosmetics(cx: &mut TestAppContext) {
    use crate::shell::pane_group::{RestoredTabMeta, TabColor};

    let (window, _dir) = make_group(cx);
    // Two tabs spawned in order: insertion idx 0 then 1. This mirrors a
    // restore where a synchronous tab lands first and an async-mounted tab
    // (an agent) lands LAST — yet the saved strip had the async tab FIRST.
    for _ in 0..2 {
        window
            .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
            .expect("window update ok")
            .expect("PTY spawn must succeed, or the tab this test needs is missing");
    }
    cx.run_until_parked();
    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        assert_eq!(visible_labels(group), vec!["Terminal 1", "Terminal 2"]);
    });

    // Replay restore placement. The tab at insertion idx 1 ("Terminal 2",
    // standing in for the agent that mounted last) was saved at visual rank
    // 0; the idx-0 tab was saved at rank 1. Sorting by rank must reorder the
    // strip so the late arrival lands in its saved slot.
    window
        .update(cx, |group, _win, cx| {
            group.place_restored_tab(
                0,
                RestoredTabMeta {
                    rank: 1,
                    ..Default::default()
                },
                cx,
            );
            group.place_restored_tab(
                1,
                RestoredTabMeta {
                    rank: 0,
                    is_preview: true,
                    pinned: true,
                    color: Some(TabColor::Teal),
                    custom_title: Some("Claude #2".into()),
                },
                cx,
            );
        })
        .expect("window update ok");

    cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        // Order: the late-mounted tab settled into saved slot 0.
        assert_eq!(
            visible_labels(group),
            vec!["Terminal 2", "Terminal 1"],
            "a tab placed last must still land in its saved visual slot",
        );
        // Cosmetics restored on the idx-1 tab.
        let restored = &group.tabs()[1];
        assert!(restored.is_preview, "preview state survives restore");
        assert!(restored.pinned, "pin survives restore");
        assert_eq!(restored.color, Some(TabColor::Teal), "color survives restore");
        assert_eq!(
            restored.custom_title.as_deref(),
            Some("Claude #2"),
            "custom title survives restore",
        );
    });
}

// ── active terminal tab drains fast; background tabs throttle ──────────────
//
// The visibility sweep marks the active tab's view visible (foreground PTY
// drain → low echo latency) and every other tab's view hidden (throttled).
// This is the invariant behind responsive typing in the foreground terminal;
// a regression here resurfaces the laggy post-restore typing symptom where a
// reconnected tab stays stuck at the slow background drain cadence.
#[gpui::test]
async fn active_terminal_tab_drains_fast_background_throttles(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);
    for _ in 0..2 {
        window
            .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
            .expect("window update ok")
            .expect("PTY spawn must succeed, or the tab this test needs is missing");
    }
    cx.run_until_parked();

    // Grab the active view of each tab (single-leaf trees → one view per tab).
    let views: Vec<Entity<TerminalView>> = cx.read(|app| {
        let group = window.read(app).expect("PaneGroup alive");
        group
            .tabs()
            .iter()
            .map(|t| match &t.content {
                PaneContent::Terminal(tree) => {
                    tree.active_view().expect("active view").clone()
                }
                _ => panic!("expected Terminal content"),
            })
            .collect()
    });
    assert_eq!(views.len(), 2, "two terminal tabs expected");

    // Last-opened tab (idx 1) is active → visible; idx 0 is backgrounded.
    cx.read(|app| {
        assert!(
            !views[0].read(app).is_visible(),
            "background tab must throttle its PTY poll",
        );
        assert!(
            views[1].read(app).is_visible(),
            "active tab must drain at the fast cadence",
        );
    });

    // Switch active to tab 0 — visibility (and thus drain cadence) flips.
    window
        .update(cx, |group, win, cx| group.set_active(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();
    cx.read(|app| {
        assert!(
            views[0].read(app).is_visible(),
            "newly-active tab must drain fast",
        );
        assert!(
            !views[1].read(app).is_visible(),
            "now-background tab must throttle",
        );
    });
}
