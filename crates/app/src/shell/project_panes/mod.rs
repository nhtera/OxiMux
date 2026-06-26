//! `ProjectPanes` — workspace-level container for one project's pane
//! groups.
//!
//! The workspace owns one `ProjectPanes` per open project; each holds:
//!
//! - a `PaneGroupManager` (pure-data layout tree)
//! - a `HashMap<PaneGroupId, Entity<PaneGroup>>` (live entities)
//! - the per-project notifier + save-callback + window-activation
//!   observer (single source of truth shared down to each group's
//!   per-tab status watcher via `Arc<AtomicBool>`).
//!
//! Splits at the workspace level create new sibling pane groups via the
//! manager. File-open and split actions target the active group.

mod render;
mod ops;
mod state;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, SharedString, Subscription, Window,
};
use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};

use oximux_agents::{AgentStatusStream, SharedBackend};
use oximux_core::{AgentAdapter, AgentSessionId};
use oximux_pty::TerminalSessionId;
use oximux_storage::{PaneBufferRepo, PaneRelayIdRepo};

use crate::notifier::{Notifier, TabId};
use crate::persisted_terminals::{
    PersistedAgentTab, PersistedGroup, PersistedLeafTab, PersistedSubPane, PersistedTab,
    PersistedTabKind, PersistedTabs, PersistedTree, snapshot_tree,
};
use crate::shell::divider::{ActiveDivider, DividerBoundsCache};
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::layout_presets::Preset;
use crate::shell::pane_group::tab_drag_zones::Zone;
use crate::shell::pane_group::{PaneGroup, PaneGroupTabKind, RestoredTabMeta};
use crate::shell::pane_group_manager::{CloseGroupError, GroupSplitOutcome, PaneGroupManager};
use crate::shell::pane_tree::{Axis, PaneGroupId, SplitInsert};

/// Hovered drop target during a cross-group tab drag — drives the
/// pane-body 5-zone overlay render. Set by `on_drag_move` on the body
/// container; cleared on drop or when the drag exits every body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabDragHoveredTarget {
    pub group_id: PaneGroupId,
    pub zone: Zone,
}
use crate::shell::terminal_view::TerminalView;

/// Persistence sink invoked after every tab/topology change. Captures
/// `SettingsRepo` + `project_id`; serializes the snapshot to JSON +
/// writes one row in `settings`.
pub type SaveCallback = Arc<dyn Fn(PersistedTabs) + Send + Sync>;

pub struct ProjectPanes {
    manager: PaneGroupManager,
    groups: HashMap<PaneGroupId, Entity<PaneGroup>>,
    /// One observer per group entity so layout / focus changes inside a
    /// group bubble up to the workspace render + trigger a save.
    _observers: HashMap<PaneGroupId, Subscription>,
    /// Focus-in subscription per group so the manager's `active_group_id`
    /// follows wherever the user actually puts focus. Without this,
    /// split / close-group actions target whichever group was last
    /// explicitly activated, not the one the user is in.
    _focus_observers: HashMap<PaneGroupId, Subscription>,
    focus_handle: FocusHandle,
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    /// Canonical window-activation flag — updated by the observer below
    /// and read by every per-tab status watcher across all groups via a
    /// shared `Arc`.
    window_active: Arc<AtomicBool>,
    _window_activation_observer: Subscription,
    /// Snapshot sink. `None` during tests / construction.
    save_callback: Option<SaveCallback>,
    /// Surrounding chrome width. Forwarded to every group on set so
    /// PTY grid math stays current.
    chrome_w_px: f32,
    /// Active workspace's identifier tint, pushed down each render by
    /// `WorkspaceRoot`. Drives the active tab's edge accent. `None` = default.
    workspace_tint: Option<crate::shell::pane_group::TabColor>,
    /// Hovered drop target during a cross-group tab drag (Phase D).
    /// `None` whenever no drag is active OR the cursor sits outside every
    /// pane body. The body's `on_drag_move` sets it to (group, zone) and
    /// the matching render pass paints the overlay.
    hovered_drop_target: Option<TabDragHoveredTarget>,
    /// Workspace-global counter for default terminal labels. Bumped on
    /// every shell spawn (initial group, split-spawn, +-new-tab) so
    /// labels stay unique across panes (global, not per-group).
    next_terminal_n: u64,
    /// Active group at the last render, so a focus move between groups can be
    /// detected and trigger the focused-pane rim flash. `None` until first
    /// render.
    last_active_group: Option<PaneGroupId>,
    /// Bumped each time focus moves to a different group while split. Keys the
    /// rim-flash animation id so a fresh focus restarts the flash; a stable
    /// token lets it settle (the animation ends transparent).
    rim_flash_token: u64,
    /// The workspace divider currently being resized via mouse capture.
    /// `Some` only while the button is held; the capture overlay's
    /// move/up handlers read and clear it.
    active_divider: Option<ActiveDivider>,
    /// Per-render cache of split-row bounds, keyed by `split_path`. Each
    /// split row records its bounds here so the divider MouseDown can seed
    /// the [`ActiveDivider`] with the parent container's geometry.
    divider_bounds: DividerBoundsCache,
    /// Path of the most-recently-armed workspace divider. Lets the capture
    /// overlay resolve a double-click-reset target even after the first
    /// click's arm was already disarmed on its mouse-up (the overlay may
    /// still be the element under the second click).
    last_divider_path: Option<Vec<usize>>,
}



#[allow(clippy::too_many_arguments)]
fn build_group(
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window_active: Arc<AtomicBool>,
    cx: &mut Context<ProjectPanes>,
) -> Entity<PaneGroup> {
    cx.new(|cx| {
        PaneGroup::new(
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
            notifier,
            window_active,
            cx,
        )
    })
}

fn observe_group(group: &Entity<PaneGroup>, cx: &mut Context<ProjectPanes>) -> Subscription {
    cx.observe(group, |_this, _g, cx| cx.notify())
}

/// Mirror the GPUI focus subtree of `group` onto the manager so that
/// any action routed via `active_group_id` (split / close-group / open
/// editor) lands on whichever group actually has the user's focus —
/// whether they got there by click, keyboard, or typing into a
/// terminal. Cheap: only re-notifies when the active id actually moves.
fn observe_group_focus(
    group: &Entity<PaneGroup>,
    id: PaneGroupId,
    window: &mut Window,
    cx: &mut Context<ProjectPanes>,
) -> Subscription {
    let handle = group.read(cx).focus_handle_clone();
    cx.on_focus_in(&handle, window, move |this, _window, cx| {
        if this.manager.set_active(id) {
            cx.notify();
        }
    })
}

impl Focusable for ProjectPanes {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Capture a terminal tab's `TerminalSplitTree` for persistence: the
/// shape (axes + weights via `snapshot_tree`), the per-leaf cwd via
/// `os_pid()` → `cwd_of_pid`, and the active leaf's DFS position. The
/// DFS-position contract pairs with `from_persisted` on the restore side:
/// the restorer walks the persisted tree in DFS order, allocating fresh
/// slot ids 0..N — `sub_panes[i]` lands on the i-th DFS leaf, and
/// `active_sub_pane` is interpreted the same way.
fn snapshot_sub_pane_tree(
    tab: &crate::shell::pane_group::PaneGroupTab,
    cx: &App,
) -> (PersistedTree, Vec<PersistedSubPane>, usize) {
    let PaneContent::Terminal(tree) = &tab.content else {
        return (PersistedTree::Leaf, Vec::new(), 0);
    };
    let shape = snapshot_tree(tree.tree());
    let leaves = tree.tree().in_order_leaves();
    let mut sub_panes: Vec<PersistedSubPane> = Vec::with_capacity(leaves.len());
    for slot in &leaves {
        // Capture every per-pane tab in the leaf. Prefer the OSC 7 hint
        // (F4.7) before paying for `proc_pidinfo` on each — snapshotting a
        // workspace with many panes becomes O(N) syscalls otherwise. The
        // same read pulls the stable surface/tab ids so they round-trip.
        let tabs: Vec<PersistedLeafTab> = match tree.leaf(*slot) {
            Some(leaf) => leaf
                .tabs()
                .iter()
                .map(|lt| {
                    let view = lt.view().read(cx);
                    let cwd = view
                        .cwd_hint()
                        .or_else(|| {
                            view.os_pid()
                                .and_then(crate::shell::cwd_resolver::cwd_of_pid)
                        })
                        .map(|p| p.display().to_string());
                    PersistedLeafTab {
                        cwd,
                        surface_id: view.surface_id().to_string(),
                        tab_id: view.tab_id().to_string(),
                    }
                })
                .collect(),
            None => Vec::new(),
        };
        let active_tab = tree.leaf(*slot).map(|l| l.active()).unwrap_or(0);
        // Top-level fields mirror the ACTIVE tab so a legacy reader (or a
        // single-tab leaf) restores the visible terminal.
        let active = tabs.get(active_tab);
        let cwd = active.and_then(|t| t.cwd.clone());
        let surface_id = active.map(|t| t.surface_id.clone()).unwrap_or_default();
        let tab_id = active.map(|t| t.tab_id.clone()).unwrap_or_default();
        // Only emit the `tabs` list for genuine multi-tab leaves so
        // single-tab leaves keep the compact legacy blob shape.
        let (tabs, active_tab) = if tabs.len() > 1 {
            (tabs, active_tab)
        } else {
            (Vec::new(), 0)
        };
        sub_panes.push(PersistedSubPane {
            cwd,
            surface_id,
            tab_id,
            tabs,
            active_tab,
        });
    }
    let active_dfs_pos = leaves
        .iter()
        .position(|&slot| slot == tree.active())
        .unwrap_or(0);
    (shape, sub_panes, active_dfs_pos)
}
