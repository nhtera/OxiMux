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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, Point, ScrollHandle, SharedString,
    Subscription, Task, Window, px,
};
use std::rc::Rc;
use oximux_agents::{
    AgentRuntime, AgentStatusStream, CliRuntime, SharedBackend, agent_label_from_title,
    classify_agent_title,
};
use oximux_core::{AgentAdapter, AgentSessionId};

use crate::shell::agent_presentation::AmbientAgent;
use oximux_pty::TerminalSessionId;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    CloseTab, FocusNextSubPane, FocusPrevSubPane, NewAgent, NewBrowserTab, NewTab, NextTab,
    PrevTab, RequestOpenAdapterPicker, Search, SendTextToActiveAgent, SplitSubPaneDown,
    SplitSubPaneRight,
    ToggleZoomSubPane,
};
use crate::notifier::{Notifier, TabId};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt, ConfirmSecondary};
use crate::shell::divider::{ActiveDivider, DividerBoundsCache};
use crate::shell::agent_status_task::spawn_status_task;
use crate::shell::agent_tab_label;
use crate::shell::context_env::SurfaceIds;
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
    /// only while open; dropped on submit / dismiss. Suppressed while the
    /// approval card is showing (one overlay at a time).
    compose_bar: Option<Entity<crate::shell::compose_bar::ComposerBar>>,
    /// Subscription to the composer's submit/dismiss events. Held alongside the
    /// composer; dropped with it.
    _compose_sub: Option<Subscription>,
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

impl PaneGroup {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window_active: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            tabs: Vec::new(),
            tab_order: Vec::new(),
            drag_hover: None,
            active: 0,
            pending_clean_exit_closes: Vec::new(),
            last_visible_ids: std::collections::HashSet::new(),
            focus_handle: cx.focus_handle(),
            next_terminal_n: 1,
            theme,
            density,
            typography,
            cwd,
            cli_runtime,
            notifier,
            window_active,
            chrome_w_px: density.w_left_rail,
            tab_strip_scroll: ScrollHandle::new(),
            mru: Vec::new(),
            mru_switcher: None,
            _mru_focus_out_sub: None,
            active_sub_divider: None,
            sub_divider_bounds: DividerBoundsCache::default(),
            last_sub_divider_path: None,
            dirty_close_dialog: None,
            _dirty_close_observer: None,
            _external_mutation_task: None,
            compose_bar: None,
            _compose_sub: None,
        }
    }

    /// Lazily start the external-mutation sweep (first render is the first
    /// place we have a `&mut Context`). Idempotent.
    pub(crate) fn ensure_external_mutation_sweep(&mut self, cx: &mut Context<Self>) {
        if self._external_mutation_task.is_some() {
            return;
        }
        self._external_mutation_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                if this
                    .update(cx, |group, cx| group.sweep_external_mutations(cx))
                    .is_err()
                {
                    break; // group dropped — stop sweeping.
                }
            }
        }));
    }

    /// Stat each editor tab's file; flag any that vanished as `Deleted` and
    /// clear the flag if a file reappeared (e.g. restored on disk). Rename is
    /// folded into `Deleted` here — the path is gone either way; distinguishing
    /// a true rename needs the watcher's paired event (a follow-up).
    fn sweep_external_mutations(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for tab in &mut self.tabs {
            let PaneGroupTabKind::Editor { path } = &tab.kind else {
                continue;
            };
            let new_state = if path.exists() {
                None
            } else {
                Some(ExternalMutation::Deleted)
            };
            if tab.external_mutation != new_state {
                tab.external_mutation = new_state;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Install the focus-out subscription that auto-dismisses the MRU
    /// HUD. Called once from the renderer on first paint (when we first
    /// have a `&mut Window`). Idempotent — subsequent calls no-op.
    pub fn ensure_mru_focus_out_sub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._mru_focus_out_sub.is_some() {
            return;
        }
        let handle = self.focus_handle.clone();
        let sub = cx.on_focus_out(&handle, window, |this, _ev, _window, cx| {
            this.cancel_mru_switch(cx);
        });
        self._mru_focus_out_sub = Some(sub);
    }

    pub fn tabs(&self) -> &[PaneGroupTab] {
        &self.tabs
    }

    /// Iterate tabs in their visible order (post-drag-reorder). Each
    /// item carries the insertion-order index alongside the tab so
    /// callers can keep using the canonical idx for click handlers and
    /// active-tracking.
    pub fn visible_tabs(&self) -> impl Iterator<Item = (usize, &PaneGroupTab)> + '_ {
        self.tab_order
            .iter()
            .filter_map(move |&idx| self.tabs.get(idx).map(|t| (idx, t)))
    }

    /// Walk `tab_order` directly (yields insertion indices in visual
    /// order). Used by the snapshot path that needs the raw indices,
    /// not the (idx, tab) pairs `visible_tabs` returns.
    pub fn tab_order_iter(&self) -> impl Iterator<Item = &usize> + '_ {
        self.tab_order.iter()
    }

    /// Replace `tab_order` with the supplied vector. The restorer uses
    /// this after pushing every restored tab to re-apply the saved
    /// visual sequence in one shot — sequencing N `move_tab` calls would
    /// be O(N²) and confuse the active-tracking guards in `move_tab`.
    ///
    /// Defensive: out-of-range indices are dropped, duplicates are
    /// dropped (keeping the first occurrence), and any insertion-index
    /// missing from `order` is appended at the end. These three guards
    /// keep a stale or corrupted snapshot from rendering the same tab
    /// twice / skipping a real tab — the invariant
    /// `tab_order.len() == tabs.len()` is preserved.
    pub fn set_tab_order(&mut self, order: Vec<usize>) {
        let next = canonicalize_tab_order(order, self.tabs.len());
        debug_assert_eq!(next.len(), self.tabs.len());
        self.tab_order = next;
    }

    /// Stamp a freshly-restored tab (at insertion index `idx`) with its
    /// saved cosmetic state and visual rank, then re-sort `tab_order` so
    /// every ranked tab sits in its saved slot. Idempotent and order-
    /// independent: called once per restored tab as it mounts (synchronously
    /// for editor/terminal/browser, asynchronously for agents), it always
    /// converges to the saved strip order. Unranked tabs (`None`) keep their
    /// relative position at the tail via the stable sort.
    pub fn place_restored_tab(
        &mut self,
        idx: usize,
        meta: RestoredTabMeta,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.restore_rank = Some(meta.rank);
            tab.is_preview = meta.is_preview;
            tab.pinned = meta.pinned;
            tab.color = meta.color;
            tab.custom_title = meta.custom_title;
        }
        self.tab_order
            .sort_by_key(|&i| self.tabs.get(i).and_then(|t| t.restore_rank).unwrap_or(usize::MAX));
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        cx.notify();
    }

    /// Move a tab from one visible position to another. Mutates only
    /// `tab_order`; entity refs in `tabs` and the `active` insertion
    /// index are preserved so the active highlight follows the moved
    /// tab automatically. No-op when indices are out of range or
    /// identical.
    ///
    /// Pinned tabs cluster at the front of `tab_order` — drag-reorder
    /// clamps the destination to stay inside the moved tab's bucket
    /// (pinned tabs can't slide into the unpinned zone and vice versa).
    pub fn move_tab(&mut self, from_visible_idx: usize, to_visible_idx: usize) {
        if from_visible_idx >= self.tab_order.len() {
            return;
        }
        let moved_insertion = self.tab_order[from_visible_idx];
        let pinned = self
            .tabs
            .get(moved_insertion)
            .map(|t| t.pinned)
            .unwrap_or(false);
        let split = self.pinned_count();
        let (min_idx, max_idx) = if pinned {
            (0, split.saturating_sub(1))
        } else {
            (split, self.tab_order.len().saturating_sub(1))
        };
        let clamped_to = to_visible_idx.clamp(min_idx, max_idx);
        if from_visible_idx == clamped_to {
            return;
        }
        let moved = self.tab_order.remove(from_visible_idx);
        self.tab_order.insert(clamped_to, moved);
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
    }

    /// `true` if the tab at insertion index `idx` is pinned.
    pub fn is_pinned(&self, idx: usize) -> bool {
        self.tabs.get(idx).map(|t| t.pinned).unwrap_or(false)
    }

    /// Number of pinned tabs — also the visible index where the
    /// unpinned cluster starts. Walks `tab_order` so the answer
    /// reflects the actual cluster boundary (pinned tabs always sort
    /// to the front via `toggle_pin`'s re-cluster step).
    pub fn pinned_count(&self) -> usize {
        self.tab_order
            .iter()
            .take_while(|&&i| self.tabs.get(i).map(|t| t.pinned).unwrap_or(false))
            .count()
    }

    /// Toggle the pinned flag on tab `idx`. After flipping the flag we
    /// re-cluster the chip inside `tab_order` so pinned tabs stay
    /// packed at the front and unpinned tabs at the back. No-op when
    /// `idx` is out of range. Notifies on every successful flip.
    pub fn toggle_pin(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.pinned = !tab.pinned;
        let now_pinned = tab.pinned;
        // Pinning a preview tab promotes it to permanent (a pinned tab the user
        // wants to keep is no longer a throwaway browse tab).
        if now_pinned {
            tab.is_preview = false;
        }
        let Some(visible_from) = self.tab_order.iter().position(|&i| i == idx) else {
            cx.notify();
            return;
        };
        // Pop the chip out, then re-insert at the cluster boundary —
        // last slot of the pinned cluster (newly pinned) or first slot
        // of the unpinned cluster (newly unpinned). pinned_count() is
        // re-read AFTER the pop so the destination index lines up with
        // the shifted vector.
        let moved = self.tab_order.remove(visible_from);
        // Same dest for both directions: pin → end of pinned cluster
        // (= split, since other pinned tabs sit at [0..split)); unpin
        // → start of unpinned cluster (= split, right after the still-
        // pinned tabs).
        let _ = now_pinned;
        let dest = self.pinned_count();
        self.tab_order.insert(dest, moved);
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        cx.notify();
    }

    pub fn drag_hover(&self) -> Option<TabDragHover> {
        self.drag_hover
    }

    /// Update the drag hover indicator. Triggers a re-render only when
    /// the value actually changes — `on_drag_move` fires on every
    /// pointer move, so naive `cx.notify()` would thrash.
    pub fn set_drag_hover(&mut self, hover: Option<TabDragHover>, cx: &mut Context<Self>) {
        if self.drag_hover == hover {
            return;
        }
        self.drag_hover = hover;
        cx.notify();
    }

    /// Visible position of the tab with `insertion_idx`, if any. Used
    /// by drop handlers to translate the drag payload's insertion-idx
    /// into the visible-idx that `move_tab` expects.
    pub fn visible_position_of(&self, insertion_idx: usize) -> Option<usize> {
        self.tab_order.iter().position(|&i| i == insertion_idx)
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> Option<&PaneGroupTab> {
        self.tabs.get(self.active)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn agent_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.kind, PaneGroupTabKind::Agent { .. }))
            .count()
    }

    /// The active tab's agent session id, if the active tab is an agent.
    /// Used by "send to active agent" actions to route directly to the
    /// agent in the focused tab — the most common layout has terminal +
    /// agent side by side, so the active tab IS the routing target.
    pub fn active_agent_session(&self) -> Option<AgentSessionId> {
        match &self.active_tab()?.kind {
            PaneGroupTabKind::Agent { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// First agent session id found anywhere in this group's tab list,
    /// regardless of active state. Fallback target when no active agent
    /// tab is available (e.g. terminal in active tab, agent in a
    /// background tab of the same group).
    pub fn first_agent_session(&self) -> Option<AgentSessionId> {
        self.tabs.iter().find_map(|t| match &t.kind {
            PaneGroupTabKind::Agent { session_id, .. } => Some(*session_id),
            _ => None,
        })
    }

    /// Count of TTY-backed tabs (terminals + agents) in this group.
    /// Excludes editor tabs.
    pub fn tty_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    PaneGroupTabKind::Terminal | PaneGroupTabKind::Agent { .. }
                )
            })
            .count()
    }

    /// Index of an existing editor tab for `path`, if any. Used by
    /// `ProjectPanes` to activate-rather-than-reopen across groups.
    pub fn editor_tab_index(&self, path: &std::path::Path) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == path))
    }

    /// Worktree paths this group keeps "live" for the sidebar status dot.
    /// A worktree is live when it owns an open PTY-backed tab — an agent
    /// (keyed by its `worktree_path`) or a plain terminal (keyed by the
    /// group's cwd, since terminals run at the project/worktree root).
    /// Mirrors the upstream rule that any live terminal session marks the
    /// worktree active (green), not just agent sessions.
    pub fn live_worktree_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut has_terminal = false;
        for tab in &self.tabs {
            match &tab.kind {
                PaneGroupTabKind::Agent { worktree_path, .. } => {
                    paths.push(worktree_path.clone());
                }
                PaneGroupTabKind::Terminal => has_terminal = true,
                _ => {}
            }
        }
        if has_terminal {
            paths.push(self.cwd.clone());
        }
        paths
    }

    /// Agent statuses inferred live from the OSC titles of *plain* terminal
    /// tabs — a hand-launched agent CLI (typed `claude`/`codex`/… into a
    /// terminal) the spawn machinery never minted a tracked session for.
    /// Keyed by the group's `cwd` (the path plain terminals run at, matching
    /// [`live_worktree_paths`]). `Agent`-kind tabs are excluded: their
    /// `StatusMachine` is authoritative and already feeds the sidebar.
    ///
    /// Multiple views in one tab can each classify; the strongest reading
    /// (most attention-worthy) wins so a working sub-pane is not masked by an
    /// idle sibling. The winning view's title also resolves the agent's
    /// display name when recognizable.
    pub fn ambient_agent_statuses(&self, cx: &gpui::App) -> Vec<(PathBuf, AmbientAgent)> {
        use crate::shell::agent_presentation::ambient_status_rank;
        let mut best: Option<AmbientAgent> = None;
        for tab in &self.tabs {
            if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                continue;
            }
            let PaneContent::Terminal(tree) = &tab.content else {
                continue;
            };
            for (_, _, view) in tree.iter_all_views() {
                let view = view.read(cx);
                let Some(title) = view.title() else { continue };
                let Some(status) = classify_agent_title(title) else {
                    continue;
                };
                let label = agent_label_from_title(title);
                if best
                    .as_ref()
                    .is_none_or(|cur| ambient_status_rank(&status) > ambient_status_rank(&cur.status))
                {
                    best = Some(AmbientAgent { status, label });
                }
            }
        }
        best.map(|agent| vec![(self.cwd.clone(), agent)])
            .unwrap_or_default()
    }

    /// Count of plain-terminal tabs running a recognizable agent (any view's
    /// title classifies). Feeds the status-bar "N agents" total alongside
    /// spawned `Agent` tabs, so a hand-launched agent registers there too.
    pub fn ambient_agent_count(&self, cx: &gpui::App) -> usize {
        self.tabs
            .iter()
            .filter(|tab| matches!(tab.kind, PaneGroupTabKind::Terminal))
            .filter(|tab| {
                let PaneContent::Terminal(tree) = &tab.content else {
                    return false;
                };
                tree.iter_all_views().any(|(_, _, view)| {
                    view.read(cx)
                        .title()
                        .and_then(classify_agent_title)
                        .is_some()
                })
            })
            .count()
    }

    /// Worktree path of the agent tab matching `tab_id`, if this group
    /// owns it. Read-only counterpart of `set_active_by_tab_id` — the
    /// notification click router uses it to resolve which project (and
    /// rail workspace) a clicked banner belongs to.
    pub fn agent_worktree_for_tab_id(&self, tab_id: TabId) -> Option<PathBuf> {
        self.tabs.iter().find_map(|t| match &t.kind {
            PaneGroupTabKind::Agent {
                session_id,
                worktree_path,
                ..
            } if TabId::from(*session_id) == tab_id => Some(worktree_path.clone()),
            _ => None,
        })
    }

    /// The workspace cwd this group spawns terminals into. The bell-banner
    /// click router uses it to resolve the owning rail workspace.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// Index of the tab whose terminal split tree hosts `session`, if this
    /// group owns it. Walks every leaf and tab in each tree (not just live
    /// views) so a bell from a background sub-pane tab still resolves.
    pub fn tab_index_for_terminal_session(
        &self,
        session: TerminalSessionId,
        cx: &gpui::App,
    ) -> Option<usize> {
        self.tabs.iter().position(|t| match &t.content {
            PaneContent::Terminal(tree) => tree
                .iter_all_views()
                .any(|(_, _, v)| v.read(cx).session_id() == session),
            PaneContent::Editor(_) | PaneContent::Diff(_) | PaneContent::Browser(_) => false,
        })
    }

    /// Dispatch a terminal-bell banner for `session` through the
    /// notification pipeline. Called by the ringing `TerminalView` (which
    /// owns the per-pane debounce); this end contributes the request
    /// context the view can't see: the tab label, the workspace key for
    /// burst collapse, and the live window-active flag. The dispatcher
    /// applies the master/source gates, visible-pane suppression, and the
    /// focus gate.
    pub fn notify_terminal_bell(
        &self,
        session: TerminalSessionId,
        pane_visible: bool,
        cx: &gpui::App,
    ) {
        let Some(idx) = self.tab_index_for_terminal_session(session, cx) else {
            return;
        };
        let tab = &self.tabs[idx];
        let label = tab
            .custom_title
            .clone()
            .unwrap_or_else(|| tab.label.clone());
        self.notifier.notify(crate::notifier::NotificationRequest {
            source: crate::notifier::NotificationSource::TerminalBell,
            kind: crate::notifier::NotificationKind::Bell,
            // Bell banners carry the raw terminal session id in the tab_id
            // slot; the `bell:` identifier namespace keeps it from ever
            // being read as an agent tab id.
            tab_id: TabId(session.0),
            workspace_key: self.cwd.to_string_lossy().into_owned(),
            label: label.to_string(),
            body: String::new(),
            window_active: self.window_active.load(Ordering::Relaxed),
            pane_visible,
        });
    }

    /// Index of an existing agent tab whose worktree matches `path`, if
    /// any. Lets the sidebar focus the tab already running in a clicked
    /// workspace's worktree instead of spawning a duplicate.
    pub fn agent_tab_index_for_worktree(&self, path: &std::path::Path) -> Option<usize> {
        self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Agent { worktree_path, .. } if worktree_path == path)
        })
    }

    /// Retained for the sidebar-width tracking plumbing (`set_chrome_width`
    /// still fires a repaint when a side panel toggles). Grid sizing no
    /// longer reads it — that moved to each TerminalView's canvas-bounds
    /// resize — so the getter is currently unused but kept for symmetry
    /// with the setter and any future chrome-aware layout.
    #[allow(dead_code)]
    pub(crate) fn chrome_w_px(&self) -> f32 {
        self.chrome_w_px
    }

    /// ScrollHandle the render layer should attach to the tab-strip
    /// viewport via `.track_scroll(...)`. Exposed so the strip builder
    /// (free function in `render.rs`) can wire it up.
    pub(crate) fn tab_strip_scroll_handle(&self) -> ScrollHandle {
        self.tab_strip_scroll.clone()
    }

    /// Snap the tab-strip viewport to its right edge. Called after every
    /// tab append so the newly-added (and now-active) tab is visible —
    /// matches the reference editor's `stickToEndRef` behavior. The raw
    /// offset value is intentionally far-negative; the strip's paint
    /// phase clamps it to the actual `max_offset` once the new tab is
    /// measured. Idempotent if the strip already fits without overflow.
    fn pin_tab_strip_to_end(&self) {
        self.tab_strip_scroll
            .set_offset(Point::new(px(-100_000.0), px(0.0)));
    }

    /// Returns true when the tab strip viewport is currently snapped to
    /// its rightmost extent (i.e. the user is "pinned to end"). Used by
    /// the label-change re-pin path so widening a chip after a tab is
    /// already on screen doesn't kick the active tab + `+` button off
    /// the right edge.
    ///
    /// `max_offset.x` is the absolute maximum negative-x the viewport
    /// can scroll to (positive value). When `|offset.x| >= max_x - 1`,
    /// the strip is at its right-edge limit. Zero `max_x` means no
    /// overflow → vacuously pinned (anything appended afterwards must
    /// stay visible since there's no slack to scroll).
    pub(crate) fn was_pinned_to_end(&self) -> bool {
        let offset = self.tab_strip_scroll.offset();
        let max_x = f32::from(self.tab_strip_scroll.max_offset().x);
        if max_x.abs() < 1.0 {
            return true;
        }
        f32::from(offset.x).abs() >= max_x - 1.0
    }

    /// Schedule a one-frame-deferred re-pin to the strip's right edge.
    /// Used after any mutation that may have widened a tab chip (custom
    /// title set, agent status badge appears) — the new `max_offset`
    /// isn't known until after the next paint, so we sleep 16 ms then
    /// re-pin. No-op when `was_pinned` is false (user had manually
    /// scrolled left → respect their position).
    pub(crate) fn schedule_repin_if_was_pinned(&self, was_pinned: bool, cx: &mut Context<Self>) {
        if !was_pinned {
            return;
        }
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let _ = weak.update(cx, |g, _cx| {
                g.pin_tab_strip_to_end();
            });
        })
        .detach();
    }

    /// Wraps `cx.notify()` with a snapshot of the strip's pin state and
    /// a deferred re-pin if the user was pinned. Called by the agent
    /// status watcher (which mutates the badge dot's visibility,
    /// changing chip width) and by any other label-mutating path that
    /// goes through `cx.notify()` without explicitly handling the pin
    /// snapshot itself.
    pub(crate) fn notify_with_label_change_check(&self, cx: &mut Context<Self>) {
        let was_pinned = self.was_pinned_to_end();
        cx.notify();
        self.schedule_repin_if_was_pinned(was_pinned, cx);
    }

    pub(crate) fn focus_handle_clone(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_chrome_width(&mut self, new_chrome: f32, cx: &mut Context<Self>) {
        if (self.chrome_w_px - new_chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = new_chrome;
        cx.notify();
    }

    /// Override the next-terminal-label seed. `ProjectPanes` uses this to
    /// route a workspace-global counter into each group right before a
    /// spawn, keeping terminal numbering monotonic across splits.
    pub fn set_next_terminal_n(&mut self, n: u64) {
        self.next_terminal_n = n;
    }

    /// Mark every terminal view in this group hidden (background-project
    /// throttle). Output keeps draining; only the poll cadence + repaints drop.
    /// Clears the visibility cache so the next render reconciles back to the
    /// real shown-set when this group is on screen again.
    ///
    /// Also hides any browser tab's native webview — an off-screen project's
    /// PaneGroups never render again until reactivated, so the per-render
    /// visibility sweep can't fire; without this the webview (a native view
    /// above the GPU canvas) keeps floating over the incoming project's pane.
    pub fn hide_all_terminals(&mut self, cx: &mut Context<Self>) {
        for tab in self.tabs.iter() {
            if let PaneContent::Terminal(tree) = &tab.content {
                for (_, _, view) in tree.iter_all_views() {
                    view.update(cx, |v, vcx| v.set_visible(false, vcx));
                }
            }
            if let PaneContent::Browser(view) = &tab.content {
                view.update(cx, |v, _| v.set_active(false));
            }
        }
        // No cx.notify() — this group is off-screen, so a repaint would be
        // wasted. Clearing the cache guarantees the post-reactivation render's
        // `desired != last_visible_ids` check is true, so the sweep re-shows
        // the real set instead of skipping on a stale-equal cache.
        self.last_visible_ids.clear();
    }

    /// Peek at the next-terminal seed (without mutating). Used by
    /// `ProjectPanes::take_next_terminal_n` to compute the workspace-wide
    /// floor across every group's local counter.
    pub fn next_terminal_n_peek(&self) -> u64 {
        self.next_terminal_n
    }

    /// Append a freshly-spawned shell terminal as a new tab. Returns the
    /// index of the new tab; `None` if PTY spawn failed.
    pub fn open_terminal_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let (backend, session_id) = spawn_local_pty(self.cwd.clone(), ids.env())?;
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        let n = self.next_terminal_n;
        self.next_terminal_n += 1;
        let tab = PaneGroupTab {
            label: SharedString::from(format!("Terminal {n}")),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            pinned: false,
            // Tab-level observer is unused for terminal tabs — sub-pane
            // observers inside TerminalSplitTree drive re-renders.
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        Some(self.active)
    }

    /// Open a terminal tab rooted at `cwd` that runs `script` once, leaving
    /// the interactive shell live afterward (so a short setup script's tab
    /// stays usable and a long-running `run` stays attached). Used by the
    /// per-project lifecycle scripts surface.
    ///
    /// The script is fed as if typed at the prompt + Enter rather than passed
    /// as shell args: the relay spawn path honors only `shell` and drops extra
    /// args, so feeding input is the one path that runs a command on both the
    /// relay-backed and in-process backends. Input queues in the PTY master
    /// and the shell consumes it once it starts reading stdin.
    ///
    /// Multi-line scripts work as written: each embedded `\n` is delivered to
    /// the shell's line discipline as an Enter, so every line executes in
    /// order. There is no single-line restriction.
    pub fn open_script_terminal_tab(
        &mut self,
        cwd: PathBuf,
        title: SharedString,
        script: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let ids = SurfaceIds::fresh(cwd.to_string_lossy().into_owned());
        let (backend, session_id) = spawn_local_pty(cwd, ids.env())?;
        {
            let mut guard = backend.lock().expect("shared backend poisoned");
            // Trailing `\n` runs the (possibly multi-line) script. Each
            // newline is an Enter to the shell's line discipline.
            let line = format!("{}\n", script.trim());
            if let Err(err) = guard.write(session_id, line.as_bytes()) {
                // Keep the tab: a live interactive shell at the worktree cwd
                // is still useful (the user can re-run the script by hand).
                // Only the in-process fallback can fail here; the relay path
                // enqueues without blocking.
                tracing::warn!(?err, "failed to feed lifecycle script into pty");
            }
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        let n = self.next_terminal_n;
        self.next_terminal_n += 1;
        let tab = PaneGroupTab {
            label: SharedString::from(format!("Terminal {n}")),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: Some(title),
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        Some(self.active)
    }

    pub fn open_or_activate_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        self.open_or_activate_editor_tab_at(path, None, false, window, cx)
    }

    /// Build an `EditorView` for `path` with its language server attached and
    /// a promote-on-edit observer wired. Shared by the new-tab and the
    /// preview-replace paths. The observer clears `is_preview` on the owning
    /// tab the first time the buffer goes dirty, so editing a preview tab
    /// makes it permanent.
    fn build_editor_view(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<oximux_editor::EditorView>, Subscription) {
        let view = cx.new(|cx| oximux_editor::EditorView::new(path.to_path_buf(), window, cx));
        // Resolve + attach a language server (extension-keyed, PATH-resolved;
        // unsupported language or uninstalled server is a clean no-op).
        if let Some(server) = oximux_editor::resolve_lsp_server(path) {
            let workspace_root = self.cwd.clone();
            view.update(cx, |v, cx| {
                v.attach_lsp(
                    &server.program,
                    server.args,
                    server.language_id,
                    workspace_root,
                    cx,
                );
            });
        }
        let observer = cx.observe(&view, |this, view_entity, cx| {
            // Promote the preview tab to permanent on first edit.
            if view_entity.read(cx).is_dirty()
                && let Some(idx) = this
                    .tabs
                    .iter()
                    .position(|t| matches!(&t.content, PaneContent::Editor(v) if v == &view_entity))
            {
                this.tabs[idx].is_preview = false;
            }
            cx.notify();
        });
        (view, observer)
    }

    /// Clear the preview (italic/ephemeral) flag on the tab at `ix`, making it
    /// a permanent tab. The double-click-the-chip gesture: a preview tab opened
    /// by a single explorer click sticks instead of being replaced by the next
    /// preview open. No-op if the tab is already permanent or `ix` is stale.
    pub fn promote_tab_to_permanent(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(ix)
            && tab.is_preview
        {
            tab.is_preview = false;
            cx.notify();
        }
    }

    /// Open `path` as a reusable single-click preview tab (italic label). If
    /// the file is already open anywhere, just activate it. Otherwise reuse
    /// the active group's existing preview tab in place (so browsing the tree
    /// never piles up tabs); if none exists, open a fresh preview tab.
    pub fn open_preview_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self.editor_tab_index(&path) {
            self.set_active(idx, window, cx);
            return idx;
        }
        // Reuse an existing preview tab in place, keeping its slot + preview-ness.
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.is_preview && matches!(t.kind, PaneGroupTabKind::Editor { .. }))
        {
            let (view, observer) = self.build_editor_view(&path, window, cx);
            let label = editor_tab_label(&path);
            let tab = &mut self.tabs[idx];
            tab.content = PaneContent::Editor(view);
            tab.kind = PaneGroupTabKind::Editor { path };
            tab.label = SharedString::from(label);
            tab.external_mutation = None;
            // Reset per-tab decorations from the file we replaced — a fresh
            // preview carries no custom title or color tag.
            tab.custom_title = None;
            tab.color = None;
            tab._observer = Some(observer);
            self.set_active(idx, window, cx);
            return idx;
        }
        self.open_or_activate_editor_tab_at(path, None, true, window, cx)
    }

    /// Give a freshly-mounted terminal a weak handle back to this group so
    /// Cmd-click on a `path:line:col` link can open it in an editor tab. An
    /// associated fn (not `&self`) so it can run while a sub-pane tree is
    /// borrowed mutably at the split/add call sites.
    fn wire_opener(view: &gpui::Entity<TerminalView>, cx: &mut Context<Self>) {
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        // Auto-close: a clean child exit (status 0) asks the group to close the
        // hosting tab. Window-free here (subscribe has no `&mut Window`), so we
        // queue the session id and let `render` (which has a window) do the
        // actual `close_tab`.
        cx.subscribe(view, |this, _view, event, cx| match event {
            TerminalViewEvent::CleanExit { session_id } => {
                this.pending_clean_exit_closes.push(*session_id);
                cx.notify();
            }
        })
        .detach();
    }

    /// Drain `pending_clean_exit_closes`: for each session that reported a
    /// clean exit, close the pane it lived in. Cascades like Cmd+W —
    /// stacked leaf-tab → drop that tab; split leaf (its last tab) → drop the
    /// leaf; the tab's last remaining view → close the whole group tab.
    /// Called from `render`, the one place with a `&mut Window`.
    pub fn close_lone_exited_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_clean_exit_closes.is_empty() {
            return;
        }
        let sessions = std::mem::take(&mut self.pending_clean_exit_closes);
        for session in sessions {
            // Locate the exited view: (tab, leaf slot, leaf-tab idx) plus the
            // tab's total view count and that leaf's tab count, so we know
            // which rung of the cascade to take. Done first (immutable +
            // `view.read`) so the close mutation below holds no live borrow.
            let mut hit = None;
            for (tab_idx, tab) in self.tabs.iter().enumerate() {
                let PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                let mut total = 0usize;
                let mut found: Option<(usize, usize)> = None;
                for (slot, leaf_tab_idx, view) in tree.iter_all_views() {
                    total += 1;
                    if view.read(cx).session_id() == session {
                        found = Some((slot, leaf_tab_idx));
                    }
                }
                if let Some((slot, leaf_tab_idx)) = found {
                    let leaf_tab_count = tree.leaf(slot).map(|l| l.len()).unwrap_or(1);
                    hit = Some((tab_idx, slot, leaf_tab_idx, total, leaf_tab_count));
                    break;
                }
            }
            let Some((tab_idx, slot, leaf_tab_idx, total, leaf_tab_count)) = hit else {
                continue;
            };
            if total == 1 {
                // Lone view → close the whole group tab.
                self.close_tab(tab_idx, window, cx);
            } else if let PaneContent::Terminal(tree) = &mut self.tabs[tab_idx].content {
                if leaf_tab_count > 1 {
                    // Stacked leaf-tab → drop just that tab.
                    tree.close_tab_in_leaf(slot, leaf_tab_idx);
                } else {
                    // Split leaf holding only this view → drop the leaf.
                    tree.close_leaf(slot);
                }
                cx.notify();
            }
        }
    }

    /// Open `path` and move the editor cursor to the 1-based `line`/`col` as
    /// shown in terminal output (e.g. a clicked `src/foo.rs:42:7`). Converts
    /// to the editor's 0-based `Position`; the editor scrolls to it on the
    /// next paint via its focus-on-activation path. A `None` line just opens
    /// the file.
    pub fn open_editor_at_position(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_or_activate_editor_tab(path.clone(), window, cx);
        let Some(line) = line else {
            return;
        };
        let editor = self.tabs.iter().find_map(|t| match &t.content {
            PaneContent::Editor(v) if v.read(cx).file_path() == path.as_path() => Some(v.clone()),
            _ => None,
        });
        if let Some(view) = editor
            && let Some(state) = view.read(cx).state()
        {
            let position = gpui_component::input::Position {
                line: line.saturating_sub(1),
                character: col.unwrap_or(1).saturating_sub(1),
            };
            state.update(cx, |s, cx| s.set_cursor_position(position, window, cx));
        }
    }

    /// Open `path` as an editor tab, optionally inserting it at a specific
    /// VISUAL slot in the strip. `insert_at_visible_idx` semantics:
    /// `None` → append (tab lands at the end of the strip);
    /// `Some(idx)` → tab lands at visual position `idx` (clamped to the
    /// strip length, with pinned tabs always staying clustered at front).
    ///
    /// When the file is already open as a tab, the existing tab is moved
    /// to the requested slot AND activated — this matches the drag-onto-
    /// strip muscle memory: the user expects "drop here" to place the tab
    /// here regardless of whether it's new or pre-existing.
    pub fn open_or_activate_editor_tab_at(
        &mut self,
        path: PathBuf,
        insert_at_visible_idx: Option<usize>,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Already-open path: move + activate the existing tab.
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == &path))
        {
            // A permanent (non-preview) open of an already-previewed tab
            // promotes it to permanent.
            if !preview {
                self.tabs[idx].is_preview = false;
            }
            if let Some(visible_target) = insert_at_visible_idx
                && let Some(from) = self.visible_position_of(idx)
            {
                // `move_tab` removes `from` first then inserts at the
                // post-remove index. When `visible_target > from`, the
                // remove step shifts the destination left by one — so
                // adjust before clamping. Mirrors the same dance in
                // the chip-level TabDragPayload drop handler.
                let raw_to = visible_target.min(self.tab_order.len());
                let to = if raw_to > from { raw_to - 1 } else { raw_to };
                let bounded = to.min(self.tab_order.len().saturating_sub(1));
                self.move_tab(from, bounded);
            }
            // Route through `set_active` so `bump_mru` + `focus_active`
            // + `pin_tab_strip_to_end` + `cx.notify` all fire — same as
            // every other activation path in the file.
            self.set_active(idx, window, cx);
            return idx;
        }
        // New tab path — construct, push, then optionally re-slot inside
        // tab_order at the requested visible index.
        let (view, observer) = self.build_editor_view(&path, window, cx);
        let label = editor_tab_label(&path);
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Editor(view),
            kind: PaneGroupTabKind::Editor { path },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: preview,
            external_mutation: None,
            restore_rank: None,
            _observer: Some(observer),
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        // Keep MRU in step with every other tab-open path — without this
        // the new editor tab is invisible to the MRU switcher until the
        // user manually re-activates it.
        self.bump_mru(new_idx);
        if let Some(visible_target) = insert_at_visible_idx {
            let from = self.tab_order.len() - 1;
            let bounded = visible_target.min(from);
            // Skip the no-op (already at end) — `move_tab` would still
            // succeed, but `pin_tab_strip_to_end` below covers append case.
            if bounded < from {
                self.move_tab(from, bounded);
            }
        }
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        self.active
    }

    /// Open a read-only diff tab for `path` (staged-vs-HEAD when
    /// `staged=true`, worktree-vs-index otherwise). Idempotent: if a
    /// diff tab for the same (path, staged) pair already exists in this
    /// group, it's activated rather than duplicated.
    ///
    /// Constructs a fresh `DiffView` entity bound to `repo` and kicks
    /// off the patch fetch via `DiffView::load`. The DiffView is then
    /// mounted as `PaneContent::Diff`.
    pub fn open_or_activate_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Already-open (same path AND same staged flag) → activate.
        // `untracked` is not part of the tab key because a file can't be
        // both tracked and untracked at the same time; whichever variant
        // opened first wins until the user closes the tab.
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(
                &t.kind,
                PaneGroupTabKind::Diff { path: p, staged: s } if p == &path && *s == staged
            )
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        // New diff tab path. DiffView::new takes (repo, theme, density,
        // typography, cx). Then load(path, staged, untracked, cx) kicks
        // off the async fetch — the view paints a "Loading…" state until
        // the patch arrives.
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load(path_for_load, staged, untracked, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = {
            let leaf = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("diff")
                .to_string();
            // Suffix tells the user which side they're looking at. Kept
            // short so narrow tab strips don't truncate the filename.
            let suffix = if staged { " · staged" } else { " · diff" };
            SharedString::from(format!("{leaf}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::Diff { path, staged },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open a new embedded browser tab at `url`. Unlike editor/diff tabs,
    /// browser tabs don't dedup — every call appends a fresh tab, like a
    /// new terminal. Mirrors the diff opener's mount/activate sequence.
    pub fn open_browser_tab(
        &mut self,
        url: impl Into<String>,
        profile_id: Option<uuid::Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let url = url.into();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let url_for_view = url.clone();
        let view = cx.new(|cx| {
            crate::shell::browser_view::BrowserView::new(
                url_for_view,
                profile_id,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = crate::shell::browser_view::host_label(&url);
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Browser(view),
            kind: PaneGroupTabKind::Browser { url },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a commit-detail tab. Dedup key is the full
    /// SHA — clicking the same commit row twice activates the
    /// existing tab. `short_oid` and `subject` are display-only and
    /// land in the tab label.
    pub fn open_or_activate_commit_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        short_oid: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Commit { sha: s } if s == &sha)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let sha_for_load = sha.clone();
        let short_for_load = short_oid.clone();
        let subject_for_load = subject.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_commit(sha_for_load, short_for_load, subject_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        // Tab label: short SHA + truncated subject. The tab strip
        // truncates anything long, so we keep the subject readable up
        // to a sane bound rather than trying to fit the entire commit
        // message.
        let label = {
            let subject_trim: String = subject.chars().take(50).collect();
            let suffix = if subject.chars().count() > 50 { "…" } else { "" };
            SharedString::from(format!("{short_oid}: {subject_trim}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::Commit { sha },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a read-only range-diff tab for one file from the
    /// "Committed on Branch" section. Dedup key is the path. `base`/`head`
    /// are the `merge_base`/`HEAD` OIDs the section was computed against;
    /// the `DiffView` loads `diff_for_range(base, head, path)`.
    pub fn open_or_activate_branch_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        base: String,
        head: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::BranchFile { path: p } if p == &path)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let leaf = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("diff")
            .to_string();
        let title = leaf.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_range(base, head, path_for_load, title, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = SharedString::from(format!("{leaf} · branch"));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::BranchFile { path },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a combined multi-file diff tab for `scope`. Dedup
    /// key is the scope title ("All Changes" / "Staged Changes" /
    /// "Untracked" / "Branch Diff") so re-clicking the same "View all" CTA
    /// reactivates the existing tab. The new `DiffView` loads via
    /// `load_combined` — the same multi-file render path commit/branch tabs
    /// use, with per-file-group staging routing.
    pub fn open_or_activate_combined_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        scope: oximux_core::CombinedDiffScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Dedup by `tab_key` (range-aware for Branch) so switching branches
        // opens a fresh tab; the display label stays the shorter `title`.
        let scope_key = SharedString::from(scope.tab_key());
        let label = SharedString::from(scope.title());
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::CombinedDiff { scope_key: k } if k == &scope_key)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let scope_for_load = scope.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_combined(scope_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::CombinedDiff { scope_key },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push_agent_tab(
        &mut self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        label_override: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        // Agent PTYs are spawned by the CLI runtime, not via spawn_local_pty,
        // so their shell env isn't threaded here in v1; the view still gets a
        // fresh identity for persistence/respawn parity with plain terminals.
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, term_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let label = match label_override {
            Some(s) => SharedString::from(s),
            None => {
                let current_labels: Vec<SharedString> =
                    self.tabs.iter().map(|t| t.label.clone()).collect();
                agent_tab_label::next_label_for(adapter_id, &current_labels)
            }
        };
        let status_task = spawn_status_task(
            status_rx.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            TabId::from(session_id),
            label.clone(),
            worktree_path.to_string_lossy().into_owned(),
            view.downgrade(),
            cx,
        );
        // Agent tabs are terminal-backed: wrap the agent PTY view in a
        // single-leaf sub-pane tree so Cmd+D can later add side PTYs.
        let agent_observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        self.tabs.push(PaneGroupTab {
            label,
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, agent_observer)),
            kind: PaneGroupTabKind::Agent {
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
            },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: Some(status_task),
        });
        let _ = observer; // legacy single-view observer no longer used
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        cx.notify();
        self.active
    }

    /// Remove a tab from this group by its insertion-order idx and
    /// return its owning `PaneGroupTab`. Used by the drag-to-split path
    /// to transfer entity ownership across `PaneGroup`s without rebuilding
    /// the inner terminal/editor view (preserves PTY + scrollback).
    ///
    /// Fixes `tab_order` (drops the entry pointing at `idx`, shifts the
    /// higher indices down by one) and `active` (decrements past the
    /// removed slot) so the source group renders correctly afterwards.
    pub fn take_tab(&mut self, idx: usize, cx: &mut Context<Self>) -> Option<PaneGroupTab> {
        if idx >= self.tabs.len() {
            return None;
        }
        // Refuse to tear out a pinned tab — pinning is a "keep here"
        // promise to the user. Drag-to-split drop becomes a no-op when
        // this fires; the drag overlay clears via the standard cancel
        // path.
        if self.tabs[idx].pinned {
            return None;
        }
        let removed = self.tabs.remove(idx);
        if let Some(pos) = self.tab_order.iter().position(|&i| i == idx) {
            self.tab_order.remove(pos);
        }
        for entry in self.tab_order.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        self.forget_mru(idx);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active == idx {
            // Active tab moved out — clamp to the last visible position.
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
        } else if idx < self.active {
            self.active -= 1;
        }
        cx.notify();
        Some(removed)
    }

    /// Append a `PaneGroupTab` that was moved in from another group.
    /// The original `_observer` is re-attached against this `cx` so the
    /// destination group re-renders on inner view updates. Returns the
    /// insertion-order index in the destination group.
    pub fn push_existing_tab(
        &mut self,
        mut tab: PaneGroupTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Drop the prior cx's observer (would point at the source group's
        // entity) and re-subscribe under this group so notify() fires
        // here on inner-view changes. For terminal tabs we re-attach to
        // the ACTIVE leaf's active view only. Background leaves / per-pane
        // tabs in a moved tab still repaint on focus (their inner
        // observers fire the source group); live cross-group repaint of a
        // non-active moved sub-tab is a known v1 limit.
        tab._observer = match &tab.content {
            PaneContent::Terminal(tree) => tree
                .active_view()
                .map(|view| cx.observe(view, |_this, _v, cx| cx.notify())),
            PaneContent::Editor(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
            // Diff tabs notify the host the same way as editor tabs.
            PaneContent::Diff(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
            // Browser tabs notify the host on title/URL/loading changes.
            PaneContent::Browser(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        self.active
    }

    /// Append a pre-built terminal tab (used by the restore path).
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        view: gpui::Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        // Restored terminals also need a link opener, else Cmd-click is dead
        // after a session restore.
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        self.tabs.push(PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        });
        self.tab_order.push(self.tabs.len() - 1);
        self.pin_tab_strip_to_end();
        cx.notify();
    }

    /// Append a pre-built terminal tab with a fully-restored sub-pane
    /// tree. Restore path for multi-sub-pane tabs (Cmd+D splits): the
    /// caller constructs the `TerminalSplitTree` via `from_persisted`
    /// with all child views + observers already in place, then hands
    /// the whole tree over here. Observers stay inside the tree's
    /// per-leaf `observers` Vec; the tab-level `_observer` slot is
    /// unused for this path (single-leaf shape would use it, but the
    /// per-leaf observers in the tree drive notifications for every
    /// pane already).
    pub fn push_restored_terminal_tab_with_tree(
        &mut self,
        label: String,
        tree: TerminalSplitTree,
        cx: &mut Context<Self>,
    ) {
        // Wire the link opener into every restored leaf view (collect first to
        // drop the tree borrow before the per-view entity updates).
        let views: Vec<_> = tree.iter_all_views().map(|(_, _, v)| v.clone()).collect();
        for view in &views {
            Self::wire_opener(view, cx);
        }
        self.tabs.push(PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(tree),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        });
        self.tab_order.push(self.tabs.len() - 1);
        self.pin_tab_strip_to_end();
        cx.notify();
    }

    /// Close entry-point for user-initiated single closes (the chip "✕" and
    /// Cmd+W). An editor tab with an unsaved buffer first prompts
    /// Save / Discard / Cancel via a modal overlay; everything else closes
    /// immediately. Bulk closes (Close Others / Close to Right) and the
    /// post-confirm path call [`Self::close_tab`] directly.
    pub fn request_close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let dirty_editor = self.tabs.get(idx).and_then(|tab| match &tab.content {
            PaneContent::Editor(view) if view.read(cx).is_dirty() => Some(view.clone()),
            _ => None,
        });
        match dirty_editor {
            Some(view) => self.mount_dirty_close_dialog(view, window, cx),
            None => self.close_tab(idx, window, cx),
        }
    }

    /// `true` while the unsaved-changes prompt is up — drives the render
    /// overlay.
    pub(crate) fn dirty_close_dialog(&self) -> Option<Entity<ConfirmDialog>> {
        self.dirty_close_dialog.clone()
    }

    /// Mount the unsaved-changes prompt for a dirty editor tab. Save writes
    /// then closes; Discard closes losing edits; Cancel keeps the tab. The
    /// tab is re-resolved by file path at choice time so an unrelated tab
    /// close (e.g. a terminal clean-exit) between mount and choice can't
    /// shift the index onto the wrong tab.
    fn mount_dirty_close_dialog(
        &mut self,
        view: Entity<oximux_editor::EditorView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Re-entry guard: one prompt at a time.
        if self.dirty_close_dialog.is_some() {
            return;
        }
        let path = view.read(cx).file_path().to_path_buf();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "This file".to_string());
        let group = cx.weak_entity();

        // Save → write the buffer, then close the (re-resolved) tab.
        let on_confirm: ConfirmCallback = {
            let group = group.clone();
            let view = view.clone();
            let path = path.clone();
            Rc::new(move |window, cx| {
                view.update(cx, |v, cx| {
                    v.save_if_dirty(cx);
                });
                let _ = group.update(cx, |g, cx| {
                    if let Some(idx) = g.editor_tab_index(&path) {
                        g.close_tab(idx, window, cx);
                    }
                });
            })
        };
        // Discard → close the (re-resolved) tab without saving.
        let on_discard: ConfirmCallback = {
            let group = group.clone();
            let path = path.clone();
            Rc::new(move |window, cx| {
                let _ = group.update(cx, |g, cx| {
                    if let Some(idx) = g.editor_tab_index(&path) {
                        g.close_tab(idx, window, cx);
                    }
                });
            })
        };

        let prompt = ConfirmPrompt {
            title: "Unsaved changes".into(),
            body: format!("{file_name} has unsaved changes. Save before closing?").into(),
            // Plain confirm — no type-to-confirm; closing one tab isn't worth
            // a typed gate.
            expected: "".into(),
            on_confirm,
            confirm_label: Some("Save".into()),
            on_cancel: None,
            secondary: Some(ConfirmSecondary {
                label: "Discard".into(),
                on_click: on_discard,
            }),
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));

        // Focus the dialog so Enter (Save) / Escape (Cancel) work without a
        // click — the prompt has no inner Input to grab focus on its own.
        dialog.read(cx).focus_handle(cx).focus(window, cx);

        // Drop the dialog the moment the user resolves it. Replacing the
        // observer cancels any stale one.
        self._dirty_close_observer = Some(cx.observe_in(
            &dialog,
            window,
            |group, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    group.dirty_close_dialog = None;
                    group._dirty_close_observer = None;
                    cx.notify();
                }
            },
        ));
        self.dirty_close_dialog = Some(dialog);
        cx.notify();
    }

    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(idx);
        // Drop the removed index from `tab_order`, then decrement every
        // remaining entry that pointed past it so the vector stays
        // consistent with the now-shifted `tabs` vector.
        if let Some(pos) = self.tab_order.iter().position(|&i| i == idx) {
            self.tab_order.remove(pos);
        }
        for entry in self.tab_order.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        self.forget_mru(idx);
        if let PaneGroupTabKind::Agent { session_id, .. } = removed.kind {
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_this, _cx| {
                if let Err(err) = runtime.cancel(session_id).await {
                    tracing::warn!(?err, "pane-group close_tab: agent cancel failed");
                }
            })
            .detach();
        }
        if self.tabs.is_empty() {
            self.active = 0;
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Close every tab in this group except `keep_idx` and any pinned
    /// tabs. Iterates in reverse so each `close_tab` call sees stable
    /// indices for the untouched portion. No-op when `keep_idx` is out
    /// of range.
    pub fn close_others(&mut self, keep_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if keep_idx >= self.tabs.len() {
            return;
        }
        for idx in (0..self.tabs.len()).rev() {
            if idx == keep_idx {
                continue;
            }
            if self.tabs[idx].pinned {
                continue;
            }
            self.close_tab(idx, window, cx);
        }
    }

    /// Close every tab whose index is greater than `from_idx`, skipping
    /// pinned tabs. Reverse iteration keeps each `close_tab` index
    /// valid against the still-unprocessed tail.
    pub fn close_to_right(&mut self, from_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if from_idx + 1 >= len {
            return;
        }
        for idx in (from_idx + 1..len).rev() {
            if self.tabs[idx].pinned {
                continue;
            }
            self.close_tab(idx, window, cx);
        }
    }

    /// Close every tab in this group. The empty group is purged by
    /// `ProjectPanes::purge_empty_groups` on the next render frame.
    pub fn close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for idx in (0..self.tabs.len()).rev() {
            self.close_tab(idx, window, cx);
        }
    }

    /// Assign a color tag (or clear with `None`) to the tab at `idx`.
    /// The chip renders a 2px left-edge bar in the chosen color.
    pub fn set_tab_color(&mut self, idx: usize, color: Option<TabColor>, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.color = color;
            cx.notify();
        }
    }

    /// Override the tab's visible title with `title` (or restore the
    /// default by passing `None`). The chip and persistence read
    /// `custom_title.unwrap_or(label)`.
    ///
    /// Title changes can widen the chip; snapshot pin state and schedule
    /// a deferred re-pin so the strip stays anchored to the new active
    /// tab if the user was at the right edge.
    pub fn set_tab_title(
        &mut self,
        idx: usize,
        title: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let was_pinned = self.was_pinned_to_end();
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.custom_title = title;
            cx.notify();
        }
        self.schedule_repin_if_was_pinned(was_pinned, cx);
    }

    /// Resolve the tab's visible title — custom override if set, else
    /// the default label. Used by the chip render and persistence.
    pub fn visible_title(&self, idx: usize) -> Option<SharedString> {
        let tab = self.tabs.get(idx)?;
        Some(
            tab.custom_title
                .clone()
                .unwrap_or_else(|| tab.label.clone()),
        )
    }

    pub fn set_active(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        // Bump MRU first so the switcher overlay reflects the new
        // ordering even when set_active early-returns (e.g., clicking
        // the already-active tab still confirms it as most-recent).
        self.bump_mru(idx);
        if idx == self.active {
            return;
        }
        self.active = idx;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Move `idx` to the front of the MRU queue (deduped). Called from
    /// every path that activates a tab: set_active, new-tab spawn
    /// paths, drag-transfer push. Cheap O(n) — n = visible tab count.
    fn bump_mru(&mut self, idx: usize) {
        self.mru.retain(|&i| i != idx);
        self.mru.insert(0, idx);
    }

    /// Drop `idx` from the MRU queue and decrement every later entry
    /// so MRU indices stay aligned with the post-remove `tabs` Vec.
    /// Called from close_tab.
    fn forget_mru(&mut self, idx: usize) {
        self.mru.retain(|&i| i != idx);
        for entry in self.mru.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
    }

    /// Read-only MRU order — front is most-recently-used. The switcher
    /// overlay walks this to render its candidate list.
    pub fn mru_order(&self) -> &[usize] {
        &self.mru
    }

    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| match &t.kind {
            PaneGroupTabKind::Agent { session_id, .. } => TabId::from(*session_id) == tab_id,
            PaneGroupTabKind::Terminal
            | PaneGroupTabKind::Editor { .. }
            | PaneGroupTabKind::Diff { .. }
            | PaneGroupTabKind::Commit { .. }
            | PaneGroupTabKind::BranchFile { .. }
            | PaneGroupTabKind::CombinedDiff { .. }
            | PaneGroupTabKind::Browser { .. } => false,
        }) else {
            return false;
        };
        self.set_active(idx, window, cx);
        true
    }

    /// Cycle to the tab AFTER the active tab in VISUAL order. After the
    /// user drag-reorders chips, `tab_order` no longer matches the
    /// insertion-order `tabs` vector — walking `tabs.len()` directly
    /// would jump in the original insertion sequence, which feels broken.
    /// We locate the active tab's position inside `tab_order`, advance
    /// one slot (wrap), and map back to its insertion index.
    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.adjacent_visible_tab(1) {
            self.set_active(next, window, cx);
        }
    }

    /// Symmetric counterpart of `next_tab` walking backwards.
    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prev) = self.adjacent_visible_tab(-1) {
            self.set_active(prev, window, cx);
        }
    }

    /// Resolve the insertion-order index of the tab `step` slots away
    /// from the active tab in visual order. Returns `None` when fewer
    /// than 2 visible tabs exist or the active tab isn't tracked.
    fn adjacent_visible_tab(&self, step: isize) -> Option<usize> {
        resolve_adjacent_visible_tab(&self.tab_order, self.active, step)
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.focus_handle.focus(window, cx);
            return;
        };
        let handle = tab.content.focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn active_editor_path(&self, cx: &gpui::App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.active)?;
        match &tab.content {
            PaneContent::Editor(view) => Some(view.read(cx).file_path().to_path_buf()),
            // Diff/Browser tabs aren't editors and don't surface a file path
            // the host's editor-tracking flows treat as an editable target.
            PaneContent::Terminal(_) | PaneContent::Diff(_) | PaneContent::Browser(_) => None,
        }
    }

    /// Walk every tab (active + inactive) and yield the captured PTY
    /// scrollback bytes for the terminal kinds. Editor tabs contribute
    /// an empty buffer so the ordinal counter stays aligned with the
    /// tab vector.
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &gpui::App) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(tree) => {
                    // Persist the ACTIVE sub-pane's scrollback only.
                    // Multi-sub-pane persistence is a follow-up; v1
                    // restore restores a single sub-pane per tab.
                    let bytes = tree
                        .active_view()
                        .map(|v| v.read(cx).serialize_buffer(max_bytes))
                        .unwrap_or_default();
                    out.push(bytes);
                }
                // Editor / Diff / Browser tabs have no PTY scrollback, but we
                // still push an empty slot so the index alignment with `tabs`
                // is preserved for the duration of this collection call.
                PaneContent::Editor(_) | PaneContent::Diff(_) | PaneContent::Browser(_) => {
                    out.push(Vec::new())
                }
            }
        }
        out
    }

    pub fn collect_pane_external_ids(&self, cx: &gpui::App) -> Vec<Option<String>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(tree) => {
                    out.push(tree.active_view().and_then(|v| v.read(cx).external_id()));
                }
                PaneContent::Editor(_) | PaneContent::Diff(_) | PaneContent::Browser(_) => {
                    out.push(None)
                }
            }
        }
        out
    }

    pub(crate) fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_terminal_tab(window, cx);
    }

    pub(crate) fn on_new_browser_tab(
        &mut self,
        _: &NewBrowserTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_browser_tab(crate::shell::browser_view::DEFAULT_URL, None, window, cx);
    }

    /// Open the scrollback search overlay on the active tab's active terminal
    /// sub-pane. Used by the root-level Search fallback so the command palette
    /// "Search Pane" entry works while the palette (not a terminal) has focus.
    /// No-op when the active tab isn't terminal-backed.
    pub(crate) fn open_search_active_terminal(
        &mut self,
        action: &Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_tab) = self.tabs.get(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &active_tab.content else {
            return;
        };
        let Some(view) = tree.active_view().cloned() else {
            return;
        };
        view.update(cx, |v, cx| v.on_search(action, window, cx));
        // Re-home keyboard focus onto this terminal. The search overlay has no
        // input of its own — its keystroke handler runs inside the terminal's
        // `on_key_down`, which only fires while the terminal is focused. On the
        // palette path the palette queues a refocus of the workspace root as it
        // closes, so an inline focus here would be clobbered; defer it so it
        // lands AFTER that refocus (same two-step focus race as
        // `refocus_active_pane`). Without this the box paints but swallows no
        // keys until the user clicks the terminal.
        window.defer(cx, move |window, cx| {
            view.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    /// Cmd+Shift+T — add a terminal tab to the FOCUSED split pane's own
    /// tab strip (per-pane tabs). When the active workspace tab isn't a
    /// terminal, fall back to a workspace-level new terminal tab.
    pub(crate) fn on_new_tab_in_pane(
        &mut self,
        _: &crate::actions::NewTabInPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve the focused leaf (re-query focus like the split path
        // does — `tree.active` doesn't auto-track mouse focus). Immutable
        // borrow scoped so the mutable `add_tab_to_leaf` can follow.
        let target = match self.tabs.get(self.active).map(|t| &t.content) {
            Some(PaneContent::Terminal(tree)) => Some(
                tree.iter_live()
                    .find(|(_, v)| v.read(cx).focused())
                    .map(|(i, _)| i)
                    .unwrap_or_else(|| tree.active()),
            ),
            _ => None,
        };
        match target {
            Some(leaf) => self.add_tab_to_leaf(leaf, window, cx),
            None => {
                self.open_terminal_tab(window, cx);
            }
        }
    }

    /// Spawn a fresh shell and append it as a tab inside `leaf_idx` of the
    /// active terminal tab. Inherits the leaf's current cwd. Used by the
    /// per-pane `+` button and `on_new_tab_in_pane`.
    pub(crate) fn add_tab_to_leaf(
        &mut self,
        leaf_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback_cwd = self.cwd.clone();
        let workspace_id = self.cwd.to_string_lossy().into_owned();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Target the requested leaf so the tab lands there.
        tree.set_active(leaf_idx);
        // Inherit the leaf's active shell cwd (same three-tier lookup as
        // sub-pane splits).
        let inherited_cwd = tree
            .active_view()
            .and_then(|v| {
                let view = v.read(cx);
                view.cwd_hint().or_else(|| {
                    view.os_pid()
                        .and_then(crate::shell::cwd_resolver::cwd_of_pid)
                })
            })
            .unwrap_or(fallback_cwd);
        let ids = SurfaceIds::fresh(workspace_id);
        let Some((backend, session_id)) = spawn_local_pty(inherited_cwd, ids.env()) else {
            return;
        };
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        tree.add_tab_to_active(view, observer);
        if let Some(active_view) = tree.active_view() {
            active_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }


    pub(crate) fn on_close_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Cmd+W cascade for terminal tabs (most-granular first):
        //   1. focused leaf has >1 per-pane tab → close just that tab
        //   2. tab is split into >1 leaf → close the focused leaf
        //   3. otherwise → close the whole workspace (group) tab
        // The focused leaf is re-resolved here because `tree.active`
        // doesn't auto-track mouse-driven focus changes. Editor/diff tabs
        // (non-Terminal) skip straight to the group-tab close.
        let active_idx = self.active;
        if let Some(active_tab) = self.tabs.get_mut(active_idx)
            && let PaneContent::Terminal(tree) = &mut active_tab.content
        {
            let focused_idx = tree
                .iter_live()
                .find(|(_, v)| v.read(cx).focused())
                .map(|(i, _)| i);
            if let Some(idx) = focused_idx {
                tree.set_active(idx);
            }
            // 1. close a per-pane tab when the focused leaf has >1.
            if tree.close_active_tab() {
                if let Some(view) = tree.active_view() {
                    view.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
                return;
            }
            // 2. close the focused leaf when the tab is split.
            if tree.live_count() > 1 && tree.close_active() {
                if let Some(view) = tree.active_view() {
                    view.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
                return;
            }
        }
        // Editor/diff/browser group tabs (and lone-leaf terminals) close the
        // whole group tab — routed through the dirty-guard for unsaved editors.
        self.request_close_tab(active_idx, window, cx);
    }

    pub(crate) fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.next_tab(window, cx);
    }

    pub(crate) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.prev_tab(window, cx);
    }

    /// Ctrl+Tab while Ctrl is held — advance the MRU switcher cursor
    /// through a FROZEN MRU snapshot. The first press captures the
    /// snapshot and lands cursor on row 1 (the most-recent prior tab,
    /// the standard "jump back" behavior). Subsequent presses advance
    /// cursor through the list, wrapping at the end.
    ///
    /// The tab is NOT activated here — the commit happens at
    /// `commit_mru_switch` on Ctrl release. This matches the reference
    /// editor's "Switch Tab" HUD pattern: hold Ctrl, tap Tab N times to
    /// land N-th entry into focus, release to commit.
    pub(crate) fn on_mru_next(
        &mut self,
        _: &crate::actions::MruNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mru.len() < 2 {
            return;
        }
        self.advance_mru_switcher(1, cx);
    }

    /// Ctrl+Shift+Tab — advance the MRU switcher cursor BACKWARD.
    /// Same snapshot/HUD lifecycle as `on_mru_next`.
    pub(crate) fn on_mru_prev(
        &mut self,
        _: &crate::actions::MruPrev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mru.len() < 2 {
            return;
        }
        self.advance_mru_switcher(-1, cx);
    }

    fn advance_mru_switcher(&mut self, step: isize, cx: &mut Context<Self>) {
        // Capture snapshot on first press of the cycle.
        if self.mru_switcher.is_none() {
            self.mru_switcher = Some(MruSwitcher {
                snapshot: self.mru.clone(),
                cursor: 0,
            });
        }
        let Some(state) = self.mru_switcher.as_mut() else {
            return;
        };
        match advance_mru_cursor(state.cursor, step, state.snapshot.len()) {
            Some(next) => {
                state.cursor = next;
                cx.notify();
            }
            None => {
                // Snapshot too short to cycle — drop the switcher so the
                // HUD doesn't get stuck open over a now-empty MRU.
                self.mru_switcher = None;
                cx.notify();
            }
        }
    }

    /// Active MRU switcher (if any). View layer reads this to decide
    /// whether to paint the floating HUD.
    pub fn mru_switcher(&self) -> Option<&MruSwitcher> {
        self.mru_switcher.as_ref()
    }

    /// Called when the modifier key (Ctrl) that opened the MRU switcher
    /// is released. Activates the highlighted tab and clears the HUD.
    /// No-op when no switcher is active.
    pub fn commit_mru_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.mru_switcher.take() else {
            return;
        };
        let Some(&target) = state.snapshot.get(state.cursor) else {
            cx.notify();
            return;
        };
        if target == self.active {
            cx.notify();
            return;
        }
        // Defense against tab close during a cycle: the snapshot freezes
        // indices captured BEFORE any close, so `target` may now point
        // to a different (or no) tab after `forget_mru` shifted things.
        // The `mru` vec stays consistent across closes — if `target` is
        // no longer in `mru`, the underlying tab is gone or shifted.
        if !self.mru.contains(&target) {
            cx.notify();
            return;
        }
        // `set_active` does its own `bump_mru` so the activated tab
        // floats to mru[0] for the NEXT switching session.
        if self.tabs.get(target).is_some() {
            self.set_active(target, window, cx);
        } else {
            cx.notify();
        }
    }

    /// Forcibly clear an in-flight MRU switcher. Called when focus leaves
    /// the group (the `on_modifiers_changed` listener won't fire on a
    /// different focus path, so the HUD would otherwise stay open).
    pub fn cancel_mru_switch(&mut self, cx: &mut Context<Self>) {
        if self.mru_switcher.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn on_new_agent(
        &mut self,
        _: &NewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keyboard path — no cursor x available. `x: None` tells
        // `WorkspaceRoot` to use the post-rail fallback anchor.
        window.dispatch_action(Box::new(RequestOpenAdapterPicker { x: None }), cx);
    }

    /// Toggle the prompt composer over the active agent tab. A second press
    /// closes it; pressing over a non-agent tab is a no-op.
    pub(crate) fn on_open_composer_bar(
        &mut self,
        _: &crate::actions::OpenComposerBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compose_bar.is_some() {
            self.close_composer(cx);
            return;
        }
        let root = match self.active_tab().map(|t| &t.kind) {
            Some(PaneGroupTabKind::Agent { worktree_path, .. }) => worktree_path.clone(),
            _ => return,
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let composer = cx.new(|cx| {
            crate::shell::compose_bar::ComposerBar::new(root, theme, density, typography, window, cx)
        });
        let sub = cx.subscribe_in(
            &composer,
            window,
            |this, _composer, outcome: &crate::shell::compose_bar::ComposerOutcome, window, cx| {
                this.handle_composer_outcome(outcome, window, cx);
            },
        );
        // Focus the draft so the user types immediately.
        let handle = composer.read(cx).input_focus_handle(cx);
        window.focus(&handle, cx);
        self.compose_bar = Some(composer);
        self._compose_sub = Some(sub);
        cx.notify();
    }

    /// Send a submitted draft to the active agent and tear down the composer.
    fn handle_composer_outcome(
        &mut self,
        outcome: &crate::shell::compose_bar::ComposerOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let crate::shell::compose_bar::ComposerOutcome::Submit(draft) = outcome {
            // Wrap in bracketed paste only when the agent shell has DECSET-2004
            // on, so a multi-line prompt lands as a single insertion rather
            // than executing line-by-line. The bytes carry no trailing CR — the
            // draft lands in the agent's input and the user presses Enter.
            let bp_on = self.active_agent_bracketed_paste(cx);
            let bytes = crate::shell::compose_bar::send_formatter::format_send_bytes(draft, bp_on);
            let text = String::from_utf8_lossy(&bytes).into_owned();
            window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
            self.apply_first_prompt_title(draft);
        }
        self.close_composer(cx);
    }

    fn close_composer(&mut self, cx: &mut Context<Self>) {
        self.compose_bar = None;
        self._compose_sub = None;
        cx.notify();
    }

    /// Whether the active agent tab's terminal currently has bracketed paste on.
    fn active_agent_bracketed_paste(&self, cx: &App) -> bool {
        let Some(PaneContent::Terminal(tree)) = self.active_tab().map(|t| &t.content) else {
            return false;
        };
        tree.active_view()
            .map(|view| view.read(cx).bracketed_paste_active())
            .unwrap_or(false)
    }

    /// Name an agent tab after its first composed prompt — but never clobber a
    /// title the user set themselves (`custom_title.is_none()` is exactly the
    /// "first submit" gate, since this very call fills it in).
    fn apply_first_prompt_title(&mut self, draft: &str) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if !matches!(tab.kind, PaneGroupTabKind::Agent { .. }) || tab.custom_title.is_some() {
            return;
        }
        let title: String = draft.trim().chars().take(50).collect();
        if !title.is_empty() {
            tab.custom_title = Some(SharedString::from(title));
        }
    }

    /// Split the active terminal tab's CURRENTLY-FOCUSED sub-pane along
    /// `axis`. Before splitting we re-resolve which sub-pane actually
    /// has keyboard focus (by walking the tree and asking each
    /// `TerminalView`) — necessary because the stored `tree.active`
    /// only updates on previous splits/cycles, not when the user clicks
    /// into a different sub-pane to give it focus. Without this
    /// re-resolution Cmd+D would split whichever pane was last cycled
    /// to, not the one the user is actually typing in.
    ///
    /// Spawns a fresh PTY at the CWD of the active sub-pane's shell
    /// (via libproc on macOS, falling back to the group's cwd when the
    /// pid is unknown or the lookup fails). No-op when the active tab
    /// is not a terminal or PTY spawn fails.
    fn split_active_sub_pane(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback_cwd = self.cwd.clone();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        // Sub-panes only apply to terminal-backed tabs; editor tabs are
        // single-view.
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Re-resolve focused sub-pane: scan live entries and pick the one
        // whose view reports `focused()`. Fallback to stored active
        // (covers the keyboard-only case where focus query may race
        // against terminal mount; the cycle/split paths keep active
        // correct in that flow). Two-step borrow: collect the index
        // through an immutable borrow, then mutate.
        let focused_idx = tree
            .iter_live()
            .find(|(_, v)| v.read(cx).focused())
            .map(|(i, _)| i);
        if let Some(idx) = focused_idx {
            tree.set_active(idx);
        }
        // Exit zoom mode before splitting — a zoomed sub-pane fills the
        // body while siblings are collapsed to width 0. Splitting under
        // that state hands the new sub-pane a zero-width slot, so the
        // user sees nothing until they zoom back out. Toggling zoom off
        // first restores the tree layout; the just-split pane gets a
        // real slice of the body. Matches the reference editor.
        if tree.zoomed().is_some() {
            tree.toggle_zoom_active();
        }
        // Inherit the source shell's CWD. Three-tier lookup:
        //   1. OSC 7 cache (F4.7) — populated every prompt by the shell's
        //      OSC 7 hook. Lock-free read, no syscall. Empty only before
        //      the first prompt fires post-spawn.
        //   2. libproc / proc_pidinfo on the shell's pid — works for any
        //      local shell, including ones without an OSC 7 hook, at the
        //      cost of a syscall (~80–200 µs on macOS).
        //   3. The tab group's cwd — last-resort fallback for relay
        //      backends, dormant sub-panes, or shells that already exited.
        let inherited_cwd = tree
            .active_view()
            .and_then(|v| {
                let view = v.read(cx);
                view.cwd_hint().or_else(|| {
                    view.os_pid()
                        .and_then(crate::shell::cwd_resolver::cwd_of_pid)
                })
            })
            .unwrap_or(fallback_cwd);
        // workspace_id stays the project root even though the new sub-pane
        // spawns at the inherited (possibly cd'd) cwd.
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let Some((backend, session_id)) = spawn_local_pty(inherited_cwd, ids.env()) else {
            return;
        };
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        tree.split_active(axis, insert, view, observer);
        // Focus the just-spawned sub-pane so keyboard input lands there.
        if let Some(active_view) = tree.active_view() {
            active_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_split_sub_pane_right(
        &mut self,
        _: &SplitSubPaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_active_sub_pane(Axis::Horizontal, SplitInsert::After, window, cx);
    }

    pub(crate) fn on_split_sub_pane_down(
        &mut self,
        _: &SplitSubPaneDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_active_sub_pane(Axis::Vertical, SplitInsert::After, window, cx);
    }

    pub(crate) fn on_focus_next_sub_pane(
        &mut self,
        _: &FocusNextSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_sub_pane_focus(true, window, cx);
    }

    pub(crate) fn on_focus_prev_sub_pane(
        &mut self,
        _: &FocusPrevSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_sub_pane_focus(false, window, cx);
    }

    fn cycle_sub_pane_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        tree.cycle_focus(forward);
        if let Some(view) = tree.active_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Apply new weights to a Split node inside the active tab's
    /// sub-pane tree. Used by the divider resize handle. Triggers a
    /// re-render only when the call actually mutates the tree — the
    /// drag handler fires at ~60 fps and unconditional notifies would
    /// thrash.
    pub fn set_active_sub_pane_weights(
        &mut self,
        path: &[usize],
        weights: Vec<f32>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return false;
        };
        let prev = tree.split_weights(path);
        let changed = tree.set_split_weights(path, weights);
        if !changed {
            return false;
        }
        let next = tree.split_weights(path);
        if prev == next {
            return false;
        }
        cx.notify();
        true
    }

    /// Shared bounds-cache handle for the render pass to record sub-pane
    /// split-row geometry into (one `Rc` clone per render).
    pub fn sub_divider_bounds_cache(&self) -> DividerBoundsCache {
        self.sub_divider_bounds.clone()
    }

    /// The sub-pane divider currently being resized, if any.
    pub fn active_sub_divider(&self) -> Option<&ActiveDivider> {
        self.active_sub_divider.as_ref()
    }

    /// Arm a sub-pane divider for mouse-capture resize.
    pub fn arm_sub_divider(&mut self, active: ActiveDivider, cx: &mut Context<Self>) {
        self.last_sub_divider_path = Some(active.split_path.clone());
        self.active_sub_divider = Some(active);
        cx.notify();
    }

    /// Reset the sub-pane split the user just double-clicked. Resolves the
    /// target from the armed divider, or the most-recently-armed path when
    /// the first click's arm was already disarmed (so the reset still lands
    /// when the capture overlay is under the second click).
    pub fn reset_sub_divider_double_click(&mut self, cx: &mut Context<Self>) {
        let path = self
            .active_sub_divider
            .as_ref()
            .map(|a| a.split_path.clone())
            .or_else(|| self.last_sub_divider_path.clone());
        if let Some(path) = path {
            self.reset_sub_split_to_equal(&path, cx);
        }
        self.disarm_sub_divider(cx);
    }

    /// Apply a resize for the armed sub-pane divider from the cursor
    /// position. No-op when nothing is armed.
    pub fn resize_active_sub_divider(
        &mut self,
        pos: Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_sub_divider.clone() else {
            return false;
        };
        let Some(frac) = crate::shell::divider::fraction_along(&active, pos) else {
            return false;
        };
        let new_weights = render::redistribute_sub_pane_weights(
            &active.initial_weights,
            active.divider_idx,
            frac,
        );
        self.set_active_sub_pane_weights(&active.split_path, new_weights, cx)
    }

    /// Disarm the active sub-pane divider (mouse released).
    pub fn disarm_sub_divider(&mut self, cx: &mut Context<Self>) {
        if self.active_sub_divider.take().is_some() {
            cx.notify();
        }
    }

    /// Reset the active tab's sub-pane split at `path` to equal weights
    /// (double-click a sub-pane divider).
    pub fn reset_sub_split_to_equal(&mut self, path: &[usize], cx: &mut Context<Self>) {
        let Some(active_tab) = self.tabs.get(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &active_tab.content else {
            return;
        };
        let Some(weights) = tree.split_weights(path) else {
            return;
        };
        let equal = vec![1.0; weights.len()];
        self.set_active_sub_pane_weights(path, equal, cx);
    }

    pub(crate) fn on_toggle_zoom_sub_pane(
        &mut self,
        _: &ToggleZoomSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Re-resolve focused sub-pane first (same reasoning as the
        // split / close paths — mouse clicks don't auto-update active).
        let focused_idx = tree
            .iter_live()
            .find(|(_, v)| v.read(cx).focused())
            .map(|(i, _)| i);
        if let Some(idx) = focused_idx {
            tree.set_active(idx);
        }
        tree.toggle_zoom_active();
        if let Some(view) = tree.active_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }
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
