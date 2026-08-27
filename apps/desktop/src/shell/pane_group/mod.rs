//! `PaneGroup` — one tab-strip leaf in the workspace's group layout tree.
//!
//! Each `PaneGroup` is a single tab strip with one active tab + its
//! content. The workspace owns a tree of these via `PaneGroupManager`;
//! splitting creates a new sibling group beside (or above/below) the
//! focused one. Each group is independent: opening a file in one group
//! does NOT affect any other group's tab list.

pub mod file_drag;
pub mod layout_presets;
pub mod render;
pub mod sub_pane;
pub mod tab_drag;
pub mod tab_drag_zones;

#[cfg(test)]
mod e2e_tests;
mod actions;
mod state;
mod tabs;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, Point, ScrollHandle, SharedString,
    Subscription, Task, WeakEntity, Window, px,
};
use oximux_agents::{
    AgentRuntime, AgentStatusStream, CliRuntime, SharedBackend, agent_label_from_title,
    classify_agent_title,
};
use oximux_core::{AgentAdapter, AgentSessionId};
use std::rc::Rc;

use crate::shell::agent_presentation::AmbientAgent;
use oximux_pty::TerminalSessionId;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    CloseTab, FocusNextSubPane, FocusPrevSubPane, NewAgent, NewBrowserTab, NewTab, NextTab,
    PrevTab, RequestOpenAdapterPicker, Search, SendTextToActiveAgent, SplitSubPaneDown,
    SplitSubPaneRight, ToggleZoomSubPane,
};
use crate::notifier::{Notifier, TabId};
use crate::shell::agent_status_task::spawn_status_task;
use crate::shell::agent_tab_label;
use crate::shell::confirm_dialog::{
    ConfirmCallback, ConfirmDialog, ConfirmPrompt, ConfirmSecondary,
};
use crate::shell::context_env::SurfaceIds;
use crate::shell::divider::{ActiveDivider, DividerBoundsCache};
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::sub_pane::TerminalSplitTree;
use crate::shell::pane_tree::{Axis, SplitInsert};
use crate::shell::terminal_view::{TerminalView, TerminalViewEvent, spawn_local_pty};

/// Discriminator for `PaneGroupTab` carrying any per-kind metadata.
pub enum PaneGroupTabKind {
    Terminal,
    Agent {
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        /// Named launch profile this agent was spawned under (`None` = the
        /// adapter's plain entry). Held so a session snapshot can persist it
        /// and a restore respawns against the same endpoint/account.
        profile: Option<String>,
    },
    Editor {
        path: PathBuf,
    },
    /// Read-only diff tab. `staged` distinguishes index-vs-worktree so
    /// the same `path` can have BOTH a "staged" diff tab and an
    /// "unstaged" diff tab open simultaneously without conflict. Not
    /// persisted across restarts — diff tabs regenerate from current
    /// `git diff` state when the user clicks the SCM row again.
    Diff {
        path: PathBuf,
        staged: bool,
    },
    /// Commit-detail tab — every file a single commit touches.
    /// Dedup key is the full SHA so the same commit clicked twice
    /// reactivates the existing tab rather than opening a duplicate.
    /// Not persisted across restarts (same lifecycle as `Diff`).
    Commit {
        sha: String,
    },
    /// Read-only range diff for one file from the "Committed on Branch"
    /// section (`merge_base..HEAD`). Dedup key is the path — clicking the
    /// same branch file reactivates its tab. Not persisted (regenerates
    /// from current branch state on click).
    BranchFile {
        path: PathBuf,
    },
    /// Combined multi-file diff (all-changes / staged / untracked / branch),
    /// opened from a "View all" CTA in the SCM panel. Dedup key is the
    /// scope's title so re-clicking the same CTA reactivates the tab. Not
    /// persisted (regenerates from current git state on click).
    CombinedDiff {
        scope_key: SharedString,
    },
    /// Embedded browser tab. `url` is the seed/last-known address —
    /// persistence reads the live URL from the `BrowserView` at snapshot,
    /// and restore re-navigates here. No relay, no PTY.
    Browser {
        url: String,
    },
    /// GitHub issue / PR browser. Singleton: the nav rail opens exactly one
    /// Tasks tab per workspace session; a second click re-activates it.
    /// Not persisted — the nav re-opens it after a session restore.
    Tasks,
    /// Scheduled-run browser. Singleton on the same terms as [`Self::Tasks`]:
    /// the nav rail opens exactly one per group and re-activates it after.
    /// Not persisted — the nav re-opens it after a session restore.
    Automations,
    /// Structured Agent Chat session (Claude `stream-json`). Backed by its own
    /// headless subprocess; `cwd`/`model` are the launch context (retained for a
    /// future `--resume`). Sibling of `Agent` but rendered as chat, not a PTY.
    AgentChat {
        cwd: PathBuf,
        model: Option<String>,
    },
}

pub struct PaneGroupTab {
    pub label: SharedString,
    pub content: PaneContent,
    pub kind: PaneGroupTabKind,
    /// User-assigned color tag, set via the right-click "Tab Color"
    /// palette. Renders as a 2px left-edge accent bar on the chip.
    /// `None` = no color (default chrome color).
    pub color: Option<TabColor>,
    /// User-assigned custom title from the "Change Title" menu row.
    /// When `Some`, the chip and persistence use this in place of the
    /// default label (e.g. "Terminal 5").
    pub custom_title: Option<SharedString>,
    /// `true` once the user picks "Pin Tab" in the right-click menu.
    /// Pinned tabs cluster at the front of `tab_order`, can't be moved
    /// across the pinned/unpinned boundary by drag-reorder, can't be
    /// torn out by drag-to-split, and are skipped by Close Others /
    /// Close to Right.
    pub pinned: bool,
    /// `true` while this is a reusable single-click "preview" tab (rendered
    /// with an italic label). Promoted to a permanent tab on edit,
    /// double-click, or pin. Persisted so a preview tab restores as a
    /// preview across a relaunch instead of silently becoming permanent.
    pub is_preview: bool,
    /// Set when the FSEvents watcher reports this editor's backing file was
    /// deleted or renamed on disk. Drives a strikethrough badge; the tab is
    /// NOT auto-closed so the buffer stays available to review/save.
    /// Runtime-only.
    pub external_mutation: Option<ExternalMutation>,
    /// Saved visual position carried only during a restore. Async-mounted
    /// tabs (agents) would otherwise append at the tail after the persisted
    /// `tab_order` was already applied; sorting `tab_order` by this rank lets
    /// every tab settle into its saved slot regardless of mount order. `None`
    /// outside of restore — cleared once the strip has settled.
    pub restore_rank: Option<usize>,
    pub _observer: Option<Subscription>,
    pub _status_task: Option<Task<()>>,
}

/// Saved per-tab state re-applied during a restore. Bundled so the many
/// restore push sites (editor/terminal/browser/agent, single- and multi-
/// group) thread one value instead of five positional args.
#[derive(Clone, Debug, Default)]
pub struct RestoredTabMeta {
    /// Visual position in the saved strip — drives `restore_rank`.
    pub rank: usize,
    pub is_preview: bool,
    pub pinned: bool,
    pub color: Option<TabColor>,
    pub custom_title: Option<SharedString>,
}

/// On-disk fate of an open editor file as detected by the file-tree watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalMutation {
    /// The file no longer exists at its path.
    Deleted,
    /// The file's path disappeared but it likely moved (a sibling appeared).
    Renamed,
}

/// Default chip label for an editor tab: the file name (or `"untitled"`).
fn editor_tab_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("untitled")
        .to_string()
}

/// Closed enum of user-pickable tab colors. Renders to a concrete
/// theme-independent RGB at chip-paint time. Picked to match the
/// reference editor's 9-swatch palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabColor {
    Blue,
    Purple,
    Pink,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Gray,
}

impl TabColor {
    /// All swatches in palette order — drives the color pickers.
    pub const ALL: [TabColor; 9] = [
        TabColor::Blue,
        TabColor::Purple,
        TabColor::Pink,
        TabColor::Red,
        TabColor::Orange,
        TabColor::Yellow,
        TabColor::Green,
        TabColor::Teal,
        TabColor::Gray,
    ];

    /// Resolve to a concrete hex (`u32` RGB). Theme-independent so the
    /// user's color choice stays recognizable across light/dark modes.
    pub fn rgb(self) -> u32 {
        match self {
            TabColor::Blue => 0x3B82F6,
            TabColor::Purple => 0xA855F7,
            TabColor::Pink => 0xEC4899,
            TabColor::Red => 0xEF4444,
            TabColor::Orange => 0xF97316,
            TabColor::Yellow => 0xEAB308,
            TabColor::Green => 0x22C55E,
            TabColor::Teal => 0x14B8A6,
            TabColor::Gray => 0x9CA3AF,
        }
    }

    /// Stable lowercase slug for persistence (matches the swatch picker
    /// labels). Round-trips through [`TabColor::from_slug`].
    pub fn slug(self) -> &'static str {
        match self {
            TabColor::Blue => "blue",
            TabColor::Purple => "purple",
            TabColor::Pink => "pink",
            TabColor::Red => "red",
            TabColor::Orange => "orange",
            TabColor::Yellow => "yellow",
            TabColor::Green => "green",
            TabColor::Teal => "teal",
            TabColor::Gray => "gray",
        }
    }

    /// Parse a persisted slug back to a swatch. `None` for an unknown slug
    /// (forward-compat: an unrecognized value degrades to "no tint").
    pub fn from_slug(s: &str) -> Option<TabColor> {
        match s {
            "blue" => Some(TabColor::Blue),
            "purple" => Some(TabColor::Purple),
            "pink" => Some(TabColor::Pink),
            "red" => Some(TabColor::Red),
            "orange" => Some(TabColor::Orange),
            "yellow" => Some(TabColor::Yellow),
            "green" => Some(TabColor::Green),
            "teal" => Some(TabColor::Teal),
            "gray" => Some(TabColor::Gray),
            _ => None,
        }
    }
}

/// Hover state during an in-progress tab drag. Drives the 2px blue
/// insertion bar rendered on the targeted chip's edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabDragHover {
    /// Visible position the dragged tab would land relative to.
    pub target_visible_idx: usize,
    /// Whether the insertion bar paints on the left edge (Before) or
    /// the right edge (After) of the target chip.
    pub side: TabInsertSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabInsertSide {
    Before,
    After,
}

pub struct PaneGroup {
    tabs: Vec<PaneGroupTab>,
    /// Visible tab order. Each entry is an index into `tabs` (the
    /// insertion-order vector). `tabs.len() == tab_order.len()` is an
    /// invariant maintained by every mutator. Drag-reorder mutates only
    /// this vector; entity refs in `tabs` stay put.
    tab_order: Vec<usize>,
    /// `Some` while a tab inside this group is being dragged AND the
    /// cursor is over one of its chips. Reset to `None` on drop or when
    /// drag leaves the strip.
    drag_hover: Option<TabDragHover>,
    active: usize,
    /// Session ids of terminal views that reported a clean exit (status 0) and
    /// want their hosting tab auto-closed. Pushed by the `CleanExit`
    /// subscription (no `&mut Window` there) and drained at the top of
    /// `render` (which has one) — see [`close_lone_exited_tabs`]. Only
    /// lone-view terminal tabs are closed; split / stacked panes keep the exit
    /// banner instead.
    pending_clean_exit_closes: Vec<TerminalSessionId>,
    /// Last set of on-screen terminal view ids pushed to `set_visible(true)`.
    /// Render diffs against this so the per-view visibility sweep only runs
    /// when the shown set actually changes (tab/leaf-tab switch, split, zoom),
    /// not on every steady-state PTY-output frame.
    last_visible_ids: std::collections::HashSet<gpui::EntityId>,
    focus_handle: FocusHandle,
    /// Monotonic counter for default terminal labels. `ProjectPanes`
    /// overrides this via `set_next_terminal_n` before each spawn so the
    /// numbering is global across panes, not per-group.
    next_terminal_n: u64,
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) typography: Typography,
    pub(crate) cwd: PathBuf,
    pub(crate) cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    /// Shared with the owning `ProjectPanes` window-activation observer
    /// so per-tab status watchers read the same flag.
    window_active: Arc<AtomicBool>,
    /// Chrome width in window pixels (rail + sidebar) — forwarded by the
    /// workspace so terminal grid dispatch can compute target area.
    chrome_w_px: f32,
    /// Scroll state for the tab-strip viewport. Render attaches via
    /// `.track_scroll(handle)`; the auto-pin logic below sets the offset
    /// to a large negative x after every tab append so the strip's paint
    /// phase clamps to the right edge (keeping new + active tabs visible).
    tab_strip_scroll: ScrollHandle,
    /// Most-recently-used tab order. `mru[0]` is the current active tab,
    /// `mru[1]` is the previously-active tab (Cmd+Tab default target),
    /// etc. Kept in sync with `tabs` via `bump_mru` / `forget_mru`;
    /// indices stay aligned with the (sometimes-shifted) `tabs` Vec.
    mru: Vec<usize>,
    /// Snapshot of `mru` captured at the first Ctrl+Tab press of a
    /// switching session. `None` outside of a switch. The HUD reads
    /// this list directly; cursor commits AT MODIFIER RELEASE rather
    /// than at each Tab press, so repeated Tabs can walk all the way
    /// through the list without each press disrupting future MRU order.
    mru_switcher: Option<MruSwitcher>,
    /// Lazy focus-out subscription. Installed on first render (which is
    /// when we first have access to `&mut Window`). Closes the MRU HUD
    /// when focus leaves this group — otherwise the `on_modifiers_changed`
    /// listener would never fire in a multi-group layout where the user
    /// releases Ctrl while focus is on a sibling group.
    _mru_focus_out_sub: Option<Subscription>,
    /// The within-tab sub-pane divider currently being resized via mouse
    /// capture. `Some` only while the button is held.
    active_sub_divider: Option<ActiveDivider>,
    /// Per-render cache of sub-pane split-row bounds, keyed by `split_path`,
    /// so the sub-pane divider MouseDown can seed the resize geometry.
    sub_divider_bounds: DividerBoundsCache,
    /// Path of the most-recently-armed sub-pane divider — lets the capture
    /// overlay resolve a double-click-reset target after the first click's
    /// arm was already disarmed.
    last_sub_divider_path: Option<Vec<usize>>,
    /// Unsaved-changes confirm dialog, mounted while a dirty editor tab close
    /// awaits the user's Save / Discard / Cancel choice. `None` when idle.
    /// Rendered as a modal overlay over this group's body.
    dirty_close_dialog: Option<Entity<ConfirmDialog>>,
    /// Observer that drops [`Self::dirty_close_dialog`] the moment the user
    /// resolves it (confirm or cancel). Reset on each mount so a stale
    /// observer never lingers.
    _dirty_close_observer: Option<Subscription>,
    /// Periodic sweep that flags editor tabs whose backing file vanished on
    /// disk (external delete). Lazily started on first render; lives on the
    /// group so it drops cleanly when the group is torn down (no orphaned
    /// subscription across a project switch).
    _external_mutation_task: Option<Task<()>>,
    /// User-opened prompt composer for the active agent tab (`⌘I`). `Some`
    /// only while open; dropped on submit / dismiss.
    compose_bar: Option<Entity<crate::shell::compose_bar::ComposerBar>>,
    /// Subscription to the composer's submit/dismiss events. Held alongside the
    /// composer; dropped with it.
    _compose_sub: Option<Subscription>,
    /// Agent session the open composer belongs to. The composer renders (and a
    /// draft submits) ONLY while this exact agent is the active tab — switching
    /// to another tab hides it (the draft survives) and guarantees a submit can
    /// never misroute to a different agent.
    compose_session: Option<AgentSessionId>,
}

/// Active MRU-switcher state. Lives only while the user holds Ctrl
/// after pressing Ctrl+Tab — the snapshot freezes so each Tab press
/// advances the cursor through the SAME list (otherwise the first
/// `set_active` would `bump_mru` and re-shuffle the underlying order).
#[derive(Clone, Debug)]
pub struct MruSwitcher {
    /// Frozen MRU at switch start. `snapshot[0]` is the tab that was
    /// active when the user first pressed Ctrl+Tab.
    pub snapshot: Vec<usize>,
    /// Highlighted row index. Wraps modulo `snapshot.len()`.
    pub cursor: usize,
}

fn terminal_view_cwd(
    view: &crate::shell::terminal_view::TerminalView,
    fallback: &std::path::Path,
) -> PathBuf {
    view.cwd_hint()
        .or_else(|| view.os_pid().and_then(crate::shell::cwd_resolver::cwd_of_pid))
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// The agent the user is currently looking at, resolved from the active group's
/// active tab. Either a tracked agent session (keyed by id) or a focused plain
/// terminal that is running a hand-launched (ambient) agent (keyed by its PTY
/// id, the same per-pane identity the rail rows use). `WorkspaceRoot` maps this
/// to a `RailAgentTarget` so the rail lights the matching disclosure row.
#[derive(Clone, Debug)]
pub enum FocusedRailAgent {
    Session(AgentSessionId),
    AmbientTerminal { pty_id: String },
}

/// One hand-launched (ambient) agent detected in a plain terminal, keyed by the
/// terminal's PTY id so each pane is its own rail row — mirroring the reference
/// cockpit's per-pane agent identity instead of collapsing every agent in a
/// worktree to a single row. `cwd` is carried so the rail can group the row
/// under the owning workspace; `agent` is the status/label/detail reading.
#[derive(Clone, Debug, PartialEq)]
pub struct AmbientAgentEntry {
    pub pty_id: String,
    pub cwd: PathBuf,
    pub agent: AmbientAgent,
}


impl Focusable for PaneGroup {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Pure helper isolated from `PaneGroup` state so it's trivially testable.
/// Returns the insertion-order index of the tab `step` slots away in
/// visual order from `active`, wrapping at both ends. Returns `None` when
/// the list is too short or `active` doesn't appear in `tab_order`.
fn resolve_adjacent_visible_tab(tab_order: &[usize], active: usize, step: isize) -> Option<usize> {
    let len = tab_order.len();
    if len < 2 {
        return None;
    }
    let here = tab_order.iter().position(|&i| i == active)?;
    let next = ((here as isize + step).rem_euclid(len as isize)) as usize;
    tab_order.get(next).copied()
}

/// Pure helper isolated from `Context` so the MRU cursor-wrap math is
/// unit-testable. Returns the new cursor after advancing `step` slots
/// through a snapshot of length `len`. Wraps via `rem_euclid` so negative
/// `step` also wraps cleanly. `None` when the snapshot is too short.
/// Pure helper isolated for unit-test reach. Build the canonical
/// `tab_order` vector from a possibly-corrupted snapshot input:
/// 1. Drop out-of-range indices (`i >= tabs_len`).
/// 2. Drop duplicates (keep first occurrence).
/// 3. Append any missing insertion-index at the end.
///
/// Result invariant: `output.len() == tabs_len`, each value in
/// `0..tabs_len` appears exactly once.
fn canonicalize_tab_order(order: Vec<usize>, tabs_len: usize) -> Vec<usize> {
    let mut seen = std::collections::HashSet::with_capacity(tabs_len);
    let mut next: Vec<usize> = Vec::with_capacity(tabs_len);
    for i in order {
        if i < tabs_len && seen.insert(i) {
            next.push(i);
        }
    }
    for i in 0..tabs_len {
        if seen.insert(i) {
            next.push(i);
        }
    }
    next
}

fn advance_mru_cursor(cursor: usize, step: isize, len: usize) -> Option<usize> {
    if len < 2 {
        return None;
    }
    let len_i = len as isize;
    Some(((cursor as isize + step).rem_euclid(len_i)) as usize)
}

#[cfg(test)]
mod canonicalize_tab_order_tests {
    use super::canonicalize_tab_order;

    #[test]
    fn happy_path_passes_through() {
        let order = vec![2_usize, 0, 1];
        assert_eq!(canonicalize_tab_order(order, 3), vec![2, 0, 1]);
    }

    #[test]
    fn out_of_range_indices_are_dropped() {
        // Old snapshot referenced 4 tabs; current group has only 3.
        let order = vec![3_usize, 1, 0];
        // 3 dropped → [1, 0], then missing index 2 appended → [1, 0, 2].
        assert_eq!(canonicalize_tab_order(order, 3), vec![1, 0, 2]);
    }

    #[test]
    fn duplicates_collapse_to_first_occurrence() {
        // Corrupted snapshot — `1` appears twice. Without dedup, len would
        // exceed tabs_len and the same tab would render twice. With dedup:
        // first `1` kept, second dropped; missing `2` appended.
        let order = vec![1_usize, 0, 1];
        assert_eq!(canonicalize_tab_order(order, 3), vec![1, 0, 2]);
    }

    #[test]
    fn missing_indices_append_at_end() {
        // Snapshot only mentions [0]. Tab 1 + 2 must still appear.
        assert_eq!(canonicalize_tab_order(vec![0_usize], 3), vec![0, 1, 2]);
    }

    #[test]
    fn empty_input_yields_insertion_order() {
        // Pre-v2 snapshot (no tab_order) → caller passes empty vec → all
        // tabs land in insertion order.
        assert_eq!(canonicalize_tab_order(vec![], 4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn empty_tabs_returns_empty() {
        // Edge: no tabs at all (post-close-all) — both axes empty.
        assert!(canonicalize_tab_order(vec![], 0).is_empty());
        assert!(canonicalize_tab_order(vec![5], 0).is_empty());
    }
}

#[cfg(test)]
mod mru_switcher_tests {
    use super::advance_mru_cursor;

    #[test]
    fn first_press_lands_on_row_1() {
        // Snapshot length 4 — starting cursor 0 + step 1 → 1.
        assert_eq!(advance_mru_cursor(0, 1, 4), Some(1));
    }

    #[test]
    fn forward_wraps_at_end() {
        // cursor at last → next is 0 (back to current active).
        assert_eq!(advance_mru_cursor(3, 1, 4), Some(0));
    }

    #[test]
    fn backward_wraps_at_start() {
        // cursor at 0 + step -1 → last.
        assert_eq!(advance_mru_cursor(0, -1, 4), Some(3));
    }

    #[test]
    fn single_entry_returns_none() {
        // Switcher is meaningless with one tab; helper signals "drop the
        // switcher" via None.
        assert_eq!(advance_mru_cursor(0, 1, 1), None);
        assert_eq!(advance_mru_cursor(0, -1, 0), None);
    }

    #[test]
    fn deep_wrap_via_rem_euclid() {
        // Verify that very negative steps still produce a valid index
        // (would panic on plain `%` with negative LHS in some idioms).
        assert_eq!(advance_mru_cursor(0, -7, 4), Some(1));
    }
}

#[cfg(test)]
mod adjacent_visible_tab_tests {
    use super::resolve_adjacent_visible_tab;

    #[test]
    fn next_walks_visual_order_after_reorder() {
        // tabs inserted as 0,1,2,3 → user drag-moved insertion idx 3 to
        // visual slot 1, so visual layout is 0,3,1,2.
        let order = [0_usize, 3, 1, 2];
        // active = 0 → next should be insertion-idx 3 (visual slot 1).
        assert_eq!(resolve_adjacent_visible_tab(&order, 0, 1), Some(3));
        // active = 3 (visual slot 1) → next is insertion-idx 1 (visual slot 2).
        assert_eq!(resolve_adjacent_visible_tab(&order, 3, 1), Some(1));
        // active = 2 (last visual) → wraps to insertion-idx 0.
        assert_eq!(resolve_adjacent_visible_tab(&order, 2, 1), Some(0));
    }

    #[test]
    fn prev_walks_visual_order_after_reorder() {
        let order = [0_usize, 3, 1, 2];
        // active = 0 (first visual) → wraps to last visual (insertion-idx 2).
        assert_eq!(resolve_adjacent_visible_tab(&order, 0, -1), Some(2));
        // active = 1 (visual slot 2) → prev is insertion-idx 3 (visual slot 1).
        assert_eq!(resolve_adjacent_visible_tab(&order, 1, -1), Some(3));
    }

    #[test]
    fn single_tab_returns_none() {
        let order = [5_usize];
        assert_eq!(resolve_adjacent_visible_tab(&order, 5, 1), None);
        assert_eq!(resolve_adjacent_visible_tab(&order, 5, -1), None);
    }

    #[test]
    fn untracked_active_returns_none() {
        let order = [0_usize, 1, 2];
        assert_eq!(resolve_adjacent_visible_tab(&order, 99, 1), None);
    }
}
