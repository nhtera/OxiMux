//! GPUI headless E2E tests for per-pane tab strip and Cmd+W cascade.
//!
//! These live inside the crate (not in `crates/app/tests/`) so they can
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
use crate::shell::terminal_view::{TerminalView, spawn_local_pty_dormant};

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
    let spawned = window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok");
    assert!(
        spawned.is_some(),
        "open_terminal_tab must succeed (PTY fallback)"
    );

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
        .expect("window update ok");
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
        .expect("window update ok");
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

// ── split produces 2 live sub-panes ───────────────────────────────────────

#[gpui::test]
async fn split_sub_pane_right_creates_second_live_pane(cx: &mut TestAppContext) {
    let (window, _dir) = make_group(cx);

    // One group tab → one leaf.
    window
        .update(cx, |group, win, cx| group.open_terminal_tab(win, cx))
        .expect("window update ok");
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
        .expect("window update ok");
    cx.run_until_parked();
    window
        .update(cx, |group, win, cx| group.add_tab_to_leaf(0, win, cx))
        .expect("window update ok");
    cx.run_until_parked();

    // cmd-w drives the close cascade: per-pane tab first, group tab survives.
    cx.simulate_keystrokes(window.into(), "cmd-w");

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
        .expect("window update ok");
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
            "cmd-e".to_string(),
        )]);
        let warnings = crate::keymap_registry::apply_live(cx, &overrides);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    });

    // Old chord must be dead (shadowed), leaving both per-pane tabs alive.
    cx.simulate_keystrokes(window.into(), "cmd-w");
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
    cx.simulate_keystrokes(window.into(), "cmd-e");
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
