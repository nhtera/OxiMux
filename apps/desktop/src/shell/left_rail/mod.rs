//! LeftRail — full workspace + nav rail.
//!
//! Composition (top → bottom):
//!
//! 1. Nav section: Tasks / Automations / Agents / Search rows. Tasks and
//!    Automations open pane tabs; Agents swaps the rail body; Search is
//!    still a shell.
//! 2. WORKSPACES section header with filter / sort / + controls
//! 3. Workspace list — per-project groups rendering OxiMux `Workspace`
//!    rows with status dots derived from the latest agent session.
//! 4. Spacer
//! 5. Bottom toolbar: "Add Project" + settings cog
//!
//! Width is `density.w_left_rail` (250px in cockpit density). Full-collapse
//! toggling is handled at `WorkspaceRoot` via the `left_rail_open` flag.
//!
//! Data flow: data is pushed DOWN by `WorkspaceRoot::refresh_left_rail`
//! before each render. LeftRail itself never reads `WorkspaceRoot` —
//! that would re-enter the entity slot during rendering and panic
//! ("cannot read while it is already being updated"). The only thing
//! kept on `weak_root` is dispatch upward via callbacks (e.g.
//! `open_row_menu`), which fire on user events after render completes.

pub mod dashboard_status_menu;
pub mod nav_section;
pub mod options_menu;
pub mod project_drag;
pub mod project_group;
pub mod project_menu;
pub mod rail_agent_row;
pub mod resize;
pub mod row_menu;
pub mod toolbar;
pub mod workspace_agent_rows;
pub mod workspace_card;
pub mod workspace_list_render;
pub mod workspace_row;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{
    AppContext, Bounds, Context, DragMoveEvent, Entity, Focusable, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
    div, point, px, svg,
};
use gpui_component::input::{InputEvent, InputState};
use oximux_core::{AgentStatus, Project, SidebandDetail, Workspace};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::SettingsRepo;

use crate::shell::left_rail::workspace_row::DiffCounts;

use crate::left_rail_layout;

use crate::actions::{OpenAddProjectDialog, OpenProjectPicker, OpenWorkspaceCreate};
use crate::shell::agent_presentation::AmbientAgent;
use crate::shell::agents_dashboard::model::{attention_rank, needs_attention};
use crate::shell::agents_dashboard::filter::StatusFilter;
use crate::shell::agents_dashboard::render_agents_dashboard;
use crate::shell::left_rail::nav_section::{NavItem, render_nav_section};
use crate::shell::left_rail::project_group::{
    build_project_group_plan, render_project_group, render_workspace_block,
};
use crate::shell::left_rail::toolbar::render_toolbar;
use crate::shell::left_rail::workspace_list_render::{
    WorkspaceGroupMode, WorkspaceSortMode, sort_workspaces,
};
use crate::workspace_root::WorkspaceRoot;

const HEADER_ICON_SIZE: f32 = 14.0;

/// How long a Smart-sorted group holds its displayed row order after a
/// score-affecting change, so rows don't reshuffle under the cursor.
const SMART_SETTLE: Duration = Duration::from_secs(3);

/// Distance (px) from a list bound within which a drag triggers auto-scroll.
const AUTOSCROLL_BAND: f32 = 24.0;
/// Constant scroll step applied per auto-scroll tick (no acceleration — KISS).
const AUTOSCROLL_STEP: f32 = 14.0;
/// Auto-scroll tick interval while the cursor sits in an edge band.
const AUTOSCROLL_TICK: Duration = Duration::from_millis(16);

/// Snapshot of the latest agent-session status for a single workspace.
/// `None` means no sessions have ever been started for that workspace.
pub type LatestStatusMap = HashMap<String, Option<AgentStatus>>;

pub use rail_agent_row::{RailAgentRow, RailAgentTarget, WorkspaceAgentList};

/// One project group's held Smart-sort order plus when it was locked. Within
/// `SMART_SETTLE` of the lock time, the group renders this order instead of a
/// freshly-computed one, so agent-status changes don't reshuffle visible rows.
struct SettleEntry {
    /// Workspace ids in the order last shown (real rows; the synthesized
    /// primary is re-prepended by the sort each render).
    order: Vec<String>,
    /// Sorted ids of the rows pinned at lock time. A pin/unpin changes this
    /// set without changing membership, so it is compared separately — a pin
    /// must re-rank immediately rather than waiting out the settle window
    /// (an attention-only status change leaves it unchanged, so that still
    /// debounces).
    pinned: Vec<String>,
    /// When this order was locked in.
    at: Instant,
}

/// `true` when two id lists contain the same set of ids (ignoring order). A
/// changed membership (workspace created/removed) re-locks a fresh order so a
/// new row appears immediately rather than waiting out the settle window.
fn same_membership(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().collect::<HashSet<_>>() == b.iter().collect::<HashSet<_>>()
}

/// Reorder `list` so its rows follow `order` (by id); any row not named in
/// `order` keeps its relative position at the tail. Used to apply a held
/// settle order on top of a freshly-sorted list.
fn reorder_to_id_sequence(list: Vec<Workspace>, order: &[String]) -> Vec<Workspace> {
    let mut by_id: HashMap<String, Workspace> = list.into_iter().map(|w| (w.id.clone(), w)).collect();
    let mut out: Vec<Workspace> = Vec::with_capacity(by_id.len());
    for id in order {
        if let Some(w) = by_id.remove(id) {
            out.push(w);
        }
    }
    // Any rows not in `order` (shouldn't happen while membership matches) keep
    // a stable tail position.
    for w in by_id.into_values() {
        out.push(w);
    }
    out
}

pub struct LeftRail {
    /// Which nav page is open, or `None` for the home view (workspace list,
    /// no nav row highlighted). Clicking the active nav toggles back to home.
    active_nav: Option<NavItem>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Sidebar data snapshot. `WorkspaceRoot::refresh_left_rail` writes
    /// these before each render; `Render` reads them. Never reach out to
    /// `weak_root` from inside `Render` — re-entrant read of a being-
    /// updated entity panics.
    projects: Vec<Project>,
    active_project_id: Option<String>,
    /// Selected workspace id — drives the active-row highlight. Pushed
    /// down by `refresh_left_rail` alongside the rest of the snapshot.
    active_workspace_id: Option<String>,
    workspaces_by_project: HashMap<String, Vec<Workspace>>,
    latest_status: LatestStatusMap,
    /// Agents inferred live from plain-terminal OSC titles, keyed by worktree
    /// path (status + display name). A hand-launched agent (typed
    /// `claude`/`codex`/…) with no tracked session shows up here; the card
    /// resolves the status against `latest_status` and shows the name.
    ambient_status: HashMap<String, AmbientAgent>,
    /// Latest tracked-session adapter id per workspace id (e.g. "claude-code").
    /// Resolves the agent display name on the card for a SPAWNED agent, the
    /// same way `ambient_status.label` does for a hand-launched one.
    latest_adapter: HashMap<String, String>,
    /// Worktree paths that currently have an open agent tab. A workspace
    /// in this set reads as "live" (green idle dot) even before its
    /// session reports a concrete status.
    live_worktrees: HashSet<String>,
    /// Cached per-worktree diff counts (keyed by worktree path). Populated
    /// by `WorkspaceRoot`'s concurrent diff-fetch background tasks and pushed
    /// down here via `set_sidebar_data`. `None` for a worktree means the
    /// count is not yet available; the card omits the chip rather than blocking.
    diff_counts: HashMap<String, DiffCounts>,
    /// Live tool-activity lines per workspace id ("Bash: cargo test…"),
    /// tailed from agent session logs by `WorkspaceRoot`'s background tick
    /// and pushed down with the rest of the snapshot. Rendered on Running
    /// dashboard rows only.
    agent_activity: HashMap<String, String>,
    /// Live structured sideband detail per workspace id — the tool the agent
    /// is currently invoking, fed event-driven from each session's status
    /// watch channel. Takes precedence over `agent_activity` on Running
    /// dashboard rows; cleared off Running so it never lingers.
    agent_sideband: HashMap<String, SidebandDetail>,
    /// Latest agent-session activity time per workspace id, as the raw
    /// RFC-3339 string (`ended_at` for a finished session, else `started_at`).
    /// Drives the dashboard's in-tier recency sort. Sourced from SQLite in
    /// `gather_rail_db_data` and pushed down with the rest of the snapshot.
    last_active: HashMap<String, String>,
    /// Per-workspace agent lists (live runtime sessions merged with DB
    /// history), keyed by workspace key. Built in `refresh_left_rail` and
    /// pushed down with the snapshot; the expandable multi-agent disclosure
    /// reads it at render. Empty until a workspace has agents.
    workspace_agents: WorkspaceAgentList,
    /// Workspace keys whose multi-agent disclosure is expanded. Toggled by
    /// clicking the "N agents" summary line; in-memory only (not persisted),
    /// and survives rail rebuilds since it lives on the entity.
    expanded_workspaces: HashSet<String>,
    /// The agent whose tab is the active pane, so its disclosure sub-row stays
    /// lit (the reference cockpit's focused-pane row). `None` when the active
    /// tab is not an agent surface. Pushed down with the snapshot.
    focused_agent: Option<RailAgentTarget>,
    /// Live rail width. Driven by the right-edge resize handle; read by
    /// `WorkspaceRoot` for pane-area reflow (`left_chrome`).
    width: Pixels,
    /// True while a resize drag is in flight. Set on every drag tick
    /// (`resize::apply_drag_move`), cleared on the first render after
    /// the drag ends. Drives the handle's highlight bar — hover styles
    /// are suppressed during drags, so the lit state must come from
    /// rail state instead.
    resizing: bool,
    /// Settings store for persisting `width` on each drag tick. `None`
    /// in unit tests that build the rail without a DB.
    settings_repo: Option<SettingsRepo>,
    /// Project ids whose group is collapsed (workspace rows hidden).
    /// Persisted to settings so the collapsed view survives restart.
    collapsed: HashSet<String>,
    /// How workspace rows are ordered within each project group. Persisted
    /// so the choice survives restart.
    sort_mode: WorkspaceSortMode,
    /// `true` renders single-line compact workspace cards; `false` (default)
    /// renders the two-line detailed cards. Persisted across restart.
    compact_cards: bool,
    /// Whether workspace rows are grouped under project headers (`Project`,
    /// default) or shown as one flat list (`None`). Persisted across restart.
    group_mode: WorkspaceGroupMode,
    /// Scroll position for the agents dashboard list. Stored on `LeftRail` so
    /// it survives re-renders while the Agents nav is active.
    agents_scroll: ScrollHandle,
    /// Agents-page status filter — the header chip, cycling on click.
    dashboard_status_filter: StatusFilter,
    /// Agents-page text filter, mirroring the filter input's current value.
    dashboard_filter: String,
    /// Lazily-built filter text input for the Agents page (needs a window).
    dashboard_filter_input: Option<Entity<InputState>>,
    /// Held so the filter input's `Change → repaint` subscription stays alive.
    _dashboard_filter_sub: Option<Subscription>,
    /// Scroll position for the home workspace list (children = project
    /// groups). Drives the scroll-to-current-workspace affordance.
    list_scroll: ScrollHandle,
    /// Bumped by `scroll_to_active`; the active card keys its locate-glow
    /// animation on this so each click replays the glow exactly once.
    /// 0 = never triggered (no animation mounts).
    locate_glow_seq: u64,
    /// Agent sessions that entered an attention/terminal state while the
    /// Agents page was NOT open. Shown as a badge on the Agents nav row;
    /// zeroed when the page is opened. Lives here (not on the dashboard
    /// view) because the rail entity survives project switches.
    agents_unread: u32,
    /// Per-project held Smart-sort order during the settle window (item:
    /// Smart rows don't reshuffle under the cursor right after a status
    /// change). Keyed by project id. Not persisted.
    smart_settle: HashMap<String, SettleEntry>,
    /// `true` while a settle-expiry re-render timer is pending, so the refresh
    /// path arms at most one.
    settle_timer_armed: bool,
    /// Cached Smart-sort settle overrides (project id → held workspace-id
    /// order), recomputed off the render path so `render` only reads it.
    /// Refreshed on data change, sort-mode change, and settle-timer expiry.
    settle_override_cache: HashMap<String, Vec<String>>,
    /// Active drag auto-scroll step: `+` scrolls toward the top of the list,
    /// `-` toward the bottom, `0` = no auto-scroll. Set from the cursor's
    /// position relative to the list bounds during a drag.
    autoscroll: f32,
    /// `true` while the auto-scroll tick loop is running, so it is armed once.
    autoscroll_armed: bool,
    /// The workspace currently being renamed inline (double-click on its
    /// title), or `None`. Carries the full row so the commit reuses the
    /// shared rename path. While set, that row renders an editable field
    /// instead of its title and suppresses its activate/drag handlers.
    renaming_workspace: Option<Workspace>,
    /// Shared text-input field backing the inline rename. Created lazily on
    /// first rename (needs a `Window`, which `new` does not have).
    rename_input: Option<Entity<InputState>>,
    /// Subscription that commits the inline rename when the field loses focus.
    _rename_sub: Option<Subscription>,
}

impl LeftRail {
    /// Resolve theme/density/typography from the current appearance.
    /// WorkspaceRoot resolves the same way in its own `new`, so the rail and
    /// root always agree.
    pub fn new(weak_root: WeakEntity<WorkspaceRoot>, cx: &mut Context<Self>) -> Self {
        let appearance = oximux_settings::appearance::active(cx);
        let density = Density::for_appearance(appearance);
        let theme = Theme::for_appearance(appearance);
        let typography = oximux_settings::appearance::typography(cx);
        Self {
            active_nav: None,
            weak_root,
            theme,
            density,
            typography,
            projects: Vec::new(),
            active_project_id: None,
            active_workspace_id: None,
            workspaces_by_project: HashMap::new(),
            latest_status: HashMap::new(),
            ambient_status: HashMap::new(),
            latest_adapter: HashMap::new(),
            live_worktrees: HashSet::new(),
            diff_counts: HashMap::new(),
            agent_activity: HashMap::new(),
            agent_sideband: HashMap::new(),
            last_active: HashMap::new(),
            workspace_agents: HashMap::new(),
            expanded_workspaces: HashSet::new(),
            focused_agent: None,
            width: px(density.w_left_rail),
            resizing: false,
            settings_repo: None,
            collapsed: HashSet::new(),
            sort_mode: WorkspaceSortMode::default(),
            compact_cards: false,
            group_mode: WorkspaceGroupMode::default(),
            agents_scroll: ScrollHandle::new(),
            dashboard_status_filter: StatusFilter::default(),
            dashboard_filter: String::new(),
            dashboard_filter_input: None,
            _dashboard_filter_sub: None,
            list_scroll: ScrollHandle::new(),
            locate_glow_seq: 0,
            agents_unread: 0,
            smart_settle: HashMap::new(),
            settle_timer_armed: false,
            settle_override_cache: HashMap::new(),
            autoscroll: 0.0,
            autoscroll_armed: false,
            renaming_workspace: None,
            rename_input: None,
            _rename_sub: None,
        }
    }

    /// The id of the workspace being renamed inline, if any. Read by the row
    /// renderer to swap that row's title for the edit field.
    fn renaming_workspace_id(&self) -> Option<&str> {
        self.renaming_workspace.as_ref().map(|w| w.id.as_str())
    }

    /// Begin an inline rename of `workspace`: lazily create + focus the shared
    /// edit field, seed it with the current name, and select it for replace.
    /// Primary (synthesized) rows are not renamable and are ignored.
    pub(crate) fn begin_rename_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace.id.starts_with("primary:") {
            return;
        }
        // Lazily build the field (and wire its blur→commit) the first time.
        if self.rename_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx));
            let sub = cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
                // Losing focus commits the edit — but only if a rename is still
                // in flight (Enter/Escape clear it first, so their blur is a
                // no-op and Escape stays a cancel).
                if matches!(event, InputEvent::Blur) {
                    this.commit_rename(cx);
                }
            });
            self.rename_input = Some(input);
            self._rename_sub = Some(sub);
        }
        if let Some(input) = self.rename_input.clone() {
            input.update(cx, |state, cx| {
                state.set_value(&workspace.name, window, cx);
            });
            let focus = input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        self.renaming_workspace = Some(workspace);
        cx.notify();
    }

    /// Commit the in-flight inline rename via the shared rename path, then
    /// dismiss the field. No-op when nothing is being renamed.
    pub(crate) fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.renaming_workspace.take() else {
            return;
        };
        let new_name = self
            .rename_input
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        // Skip the DB write + rail refresh when nothing actually changed (e.g.
        // the field lost focus without an edit).
        if new_name.trim() != workspace.name {
            let _ = self
                .weak_root
                .update(cx, |root, cx| root.rename_workspace_now(workspace, new_name, cx));
        }
        cx.notify();
    }

    /// Cancel the in-flight inline rename, discarding the edit. No-op when
    /// nothing is being renamed.
    pub(crate) fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming_workspace.take().is_some() {
            cx.notify();
        }
    }

    /// Set the Agents-page status filter to `choice` (picked from the header
    /// dropdown). Repaints only when it actually changes.
    pub(crate) fn set_dashboard_status_filter(
        &mut self,
        choice: StatusFilter,
        cx: &mut Context<Self>,
    ) {
        if self.dashboard_status_filter != choice {
            self.dashboard_status_filter = choice;
            cx.notify();
        }
    }

    /// Clear the Agents-page text filter (the Escape affordance on its input).
    pub(crate) fn clear_dashboard_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.dashboard_filter_input.clone() {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        if !self.dashboard_filter.is_empty() {
            self.dashboard_filter.clear();
            cx.notify();
        }
    }

    /// Lazily build the Agents-page filter input (it needs a window) and wire
    /// its `Change → mirror value + repaint` subscription. Returns a clone for
    /// the header render.
    fn ensure_dashboard_filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = &self.dashboard_filter_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter…"));
        let sub = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.dashboard_filter = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        self.dashboard_filter_input = Some(input.clone());
        self._dashboard_filter_sub = Some(sub);
        input
    }

    /// Clear all held Smart-sort orders so the next render re-locks fresh
    /// orders. Called when a structural change (e.g. a pin toggle) must apply
    /// immediately rather than waiting out the settle window.
    pub(crate) fn clear_sort_settle(&mut self) {
        self.smart_settle.clear();
        // Drop any held order immediately so the next render re-sorts fresh
        // rather than waiting for the next data push to refresh the cache.
        self.settle_override_cache.clear();
    }

    /// Recompute the per-project Smart-sort settle overrides and store them in
    /// `settle_override_cache` for the next render to read. For each project,
    /// compares the freshly-computed Smart order against the held one: within
    /// the settle window and with unchanged membership, the held order wins
    /// (and a single expiry timer is armed); otherwise a fresh order is locked.
    /// Caches an empty map outside Smart mode. Call this off the render path —
    /// on data change, sort-mode change, or settle-timer expiry — so render
    /// never mutates state or arms a timer.
    fn refresh_settle_cache(&mut self, cx: &mut Context<Self>) {
        let mut overrides = HashMap::new();
        // Settle applies to Smart only — Recent/Manual orders don't move when an
        // agent status changes. Leaving Smart drops stale state so a later
        // return to Smart re-locks fresh.
        if self.sort_mode != WorkspaceSortMode::Smart {
            self.smart_settle.clear();
            self.settle_override_cache = overrides;
            return;
        }
        // Snapshot the freshly-sorted id order + pinned set per project under
        // an immutable borrow, then reconcile against the cache (mutable) in a
        // second pass.
        let fresh: Vec<(String, Vec<String>, Vec<String>)> = self
            .projects
            .iter()
            .map(|p| {
                let ws = self
                    .workspaces_by_project
                    .get(&p.id)
                    .cloned()
                    .unwrap_or_default();
                let mut pinned_now: Vec<String> =
                    ws.iter().filter(|w| w.pinned).map(|w| w.id.clone()).collect();
                pinned_now.sort();
                let sorted = sort_workspaces(&ws, &p.root_path, WorkspaceSortMode::Smart, |w| {
                    let status = self.latest_status.get(&w.id).cloned().flatten();
                    attention_rank(
                        status.as_ref(),
                        self.live_worktrees.contains(&w.worktree_path),
                    )
                });
                (
                    p.id.clone(),
                    sorted.into_iter().map(|w| w.id).collect(),
                    pinned_now,
                )
            })
            .collect();

        let mut within_window = false;
        for (pid, ids_now, pinned_now) in fresh {
            match self.smart_settle.get(&pid) {
                // Hold the prior order only while the rows AND the pinned set
                // are unchanged — a pin/unpin re-ranks immediately.
                Some(entry)
                    if entry.at.elapsed() < SMART_SETTLE
                        && entry.pinned == pinned_now
                        && same_membership(&entry.order, &ids_now) =>
                {
                    overrides.insert(pid, entry.order.clone());
                    within_window = true;
                }
                _ => {
                    self.smart_settle.insert(
                        pid,
                        SettleEntry {
                            order: ids_now,
                            pinned: pinned_now,
                            at: Instant::now(),
                        },
                    );
                }
            }
        }
        if within_window {
            self.arm_settle_timer(cx);
        }
        self.settle_override_cache = overrides;
    }

    /// Arm a single timer that fires at the end of the settle window to drop
    /// expired held orders and repaint, so a Smart group re-sorts once the
    /// window passes even if no other event would have triggered a render.
    fn arm_settle_timer(&mut self, cx: &mut Context<Self>) {
        if self.settle_timer_armed {
            return;
        }
        self.settle_timer_armed = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SMART_SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                this.settle_timer_armed = false;
                // Recompute off the render path: expired holds fall away and a
                // fresh order locks, refreshing the cache render will read.
                this.refresh_settle_cache(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Update the drag auto-scroll direction from the cursor's window Y
    /// relative to the list viewport `bounds`: near the top edge scroll up,
    /// near the bottom edge scroll down, otherwise stop. Arms the tick loop
    /// when a direction is set.
    fn note_autoscroll_cursor(
        &mut self,
        cursor_y: f32,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let top = f32::from(bounds.top());
        let bottom = f32::from(bounds.bottom());
        let dir = if cursor_y < top + AUTOSCROLL_BAND {
            AUTOSCROLL_STEP
        } else if cursor_y > bottom - AUTOSCROLL_BAND {
            -AUTOSCROLL_STEP
        } else {
            0.0
        };
        self.autoscroll = dir;
        if dir != 0.0 {
            self.arm_autoscroll(cx);
        }
    }

    /// Run the auto-scroll tick loop while a drag holds the cursor in an edge
    /// band. Stops when the drag ends, the cursor leaves the band
    /// (`autoscroll` reset to 0 by `note_autoscroll_cursor`), or the list hits
    /// a scroll extent.
    fn arm_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll_armed {
            return;
        }
        self.autoscroll_armed = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_TICK).await;
                let keep = this
                    .update(cx, |this, cx| {
                        if !cx.has_active_drag() || this.autoscroll == 0.0 {
                            this.autoscroll = 0.0;
                            this.autoscroll_armed = false;
                            return false;
                        }
                        let off = this.list_scroll.offset();
                        // Valid scroll range is [-max_offset.y, 0]; clamp so a
                        // runaway tick can't push past either extent.
                        let max_y = f32::from(this.list_scroll.max_offset().y);
                        let cur_y = f32::from(off.y);
                        let new_y = (cur_y + this.autoscroll).clamp(-max_y, 0.0);
                        if (new_y - cur_y).abs() < 0.01 {
                            // At an extent — nothing more to reveal this way.
                            this.autoscroll = 0.0;
                            this.autoscroll_armed = false;
                            return false;
                        }
                        this.list_scroll.set_offset(point(off.x, px(new_y)));
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    /// Scroll the workspace list so the active project's group is in view
    /// and replay the locate glow on the active card. Reduced motion skips
    /// the glow — the scroll landing on the (already raised) active card
    /// is sufficient locate feedback.
    pub(crate) fn scroll_to_active(&mut self, cx: &mut Context<Self>) {
        let Some(active_id) = self.active_project_id.as_deref() else {
            return;
        };
        if let Some(ix) = self.projects.iter().position(|p| p.id == active_id) {
            self.list_scroll.scroll_to_item(ix);
        }
        if !crate::motion_settings::active(cx).reduced {
            self.locate_glow_seq += 1;
        }
        cx.notify();
    }

    /// Install the settings store + load persisted layout (width +
    /// collapsed groups). Called once by `WorkspaceRoot` after
    /// construction (kept out of `new` so unit tests can build a
    /// repo-less rail).
    pub(crate) fn init_layout(&mut self, settings_repo: SettingsRepo) {
        self.width = px(left_rail_layout::load_left_rail_width(&settings_repo));
        self.collapsed = left_rail_layout::load_collapsed_projects(&settings_repo)
            .into_iter()
            .collect();
        self.sort_mode = left_rail_layout::load_sort_mode(&settings_repo);
        self.compact_cards = left_rail_layout::load_compact_cards(&settings_repo);
        self.group_mode = left_rail_layout::load_group_mode(&settings_repo);
        self.settings_repo = Some(settings_repo);
    }

    /// Current workspace sort mode.
    pub fn sort_mode(&self) -> WorkspaceSortMode {
        self.sort_mode
    }

    /// Current project grouping mode.
    pub fn group_mode(&self) -> WorkspaceGroupMode {
        self.group_mode
    }

    /// Set the project grouping mode, persist it, and re-render.
    pub(crate) fn set_group_mode(&mut self, mode: WorkspaceGroupMode, cx: &mut Context<Self>) {
        self.group_mode = mode;
        if let Some(repo) = &self.settings_repo {
            left_rail_layout::save_group_mode(repo, mode);
        }
        cx.notify();
    }

    /// Set the workspace sort mode directly (from the display-options menu),
    /// persist it, and re-render.
    pub(crate) fn set_sort_mode(&mut self, mode: WorkspaceSortMode, cx: &mut Context<Self>) {
        self.sort_mode = mode;
        if let Some(repo) = &self.settings_repo {
            left_rail_layout::save_sort_mode(repo, mode);
        }
        self.refresh_settle_cache(cx);
        cx.notify();
    }

    /// Whether compact single-line cards are active.
    pub fn compact_cards(&self) -> bool {
        self.compact_cards
    }

    /// Collapse all groups, or expand all if every group is already
    /// collapsed. Persists the new set and re-renders.
    pub(crate) fn toggle_collapse_all(&mut self, cx: &mut Context<Self>) {
        let all_collapsed = !self.projects.is_empty()
            && self.projects.iter().all(|p| self.collapsed.contains(&p.id));
        if all_collapsed {
            self.collapsed.clear();
        } else {
            self.collapsed = self.projects.iter().map(|p| p.id.clone()).collect();
        }
        self.persist_collapsed();
        cx.notify();
    }

    /// Flip between compact (single-line) and detailed (two-line) workspace
    /// cards and persist the choice.
    pub(crate) fn toggle_compact_cards(&mut self, cx: &mut Context<Self>) {
        self.compact_cards = !self.compact_cards;
        if let Some(repo) = &self.settings_repo {
            left_rail_layout::save_compact_cards(repo, self.compact_cards);
        }
        self.refresh_settle_cache(cx);
        cx.notify();
    }

    /// Persist the current collapsed set (no-op without a settings repo).
    fn persist_collapsed(&self) {
        if let Some(repo) = &self.settings_repo {
            let ids: Vec<String> = self.collapsed.iter().cloned().collect();
            left_rail_layout::save_collapsed_projects(repo, &ids);
        }
    }

    /// Toggle the collapsed state of a project group, persist the new
    /// set, and re-render.
    ///
    /// Pins the toggled header in place across the relayout: collapsing or
    /// expanding a group changes total content height, which can otherwise
    /// snap the viewport (e.g. when scrolled near the end). Records the
    /// header's screen position before the toggle and, on the next frame
    /// (after the new layout), nudges the scroll offset so the header returns
    /// to where it was.
    pub(crate) fn toggle_collapsed(
        &mut self,
        project_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let anchor_top = self
            .projects
            .iter()
            .position(|p| p.id == project_id)
            .and_then(|ix| self.list_scroll.bounds_for_item(ix))
            .map(|b| f32::from(b.top()));

        if !self.collapsed.remove(&project_id) {
            self.collapsed.insert(project_id.clone());
        }
        self.persist_collapsed();
        cx.notify();

        let Some(target_top) = anchor_top else {
            return;
        };
        let entity = cx.entity();
        window.on_next_frame(move |_window, cx| {
            entity.update(cx, |this, cx| {
                let Some(ix) = this.projects.iter().position(|p| p.id == project_id) else {
                    return;
                };
                let Some(bounds) = this.list_scroll.bounds_for_item(ix) else {
                    return;
                };
                let delta = f32::from(bounds.top()) - target_top;
                // Sub-pixel drift isn't worth a correcting repaint.
                if delta.abs() < 0.5 {
                    return;
                }
                let off = this.list_scroll.offset();
                this.list_scroll
                    .set_offset(point(off.x, px(f32::from(off.y) - delta)));
                cx.notify();
            });
        });
    }

    /// Current rail width — read by `WorkspaceRoot` for pane reflow.
    pub(crate) fn width(&self) -> Pixels {
        self.width
    }

    /// Set the rail width from a drag tick: clamp into bounds, persist,
    /// and re-render. The persisted value lets the width survive restart.
    pub(crate) fn set_width(&mut self, candidate: Pixels, cx: &mut Context<Self>) {
        let clamped = left_rail_layout::clamp_left_rail_width(f32::from(candidate));
        self.width = px(clamped);
        if let Some(repo) = &self.settings_repo {
            left_rail_layout::save_left_rail_width(repo, clamped);
        }
        cx.notify();
    }

    /// Push the latest sidebar snapshot. Called by
    /// `WorkspaceRoot::refresh_left_rail` at the top of each render.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_sidebar_data(
        &mut self,
        projects: Vec<Project>,
        active_project_id: Option<String>,
        active_workspace_id: Option<String>,
        workspaces_by_project: HashMap<String, Vec<Workspace>>,
        latest_status: LatestStatusMap,
        live_worktrees: HashSet<String>,
        ambient_status: HashMap<String, AmbientAgent>,
        latest_adapter: HashMap<String, String>,
        diff_counts: HashMap<String, DiffCounts>,
        agent_activity: HashMap<String, String>,
        agent_sideband: HashMap<String, SidebandDetail>,
        last_active: HashMap<String, String>,
        workspace_agents: WorkspaceAgentList,
        focused_agent: Option<RailAgentTarget>,
        cx: &mut Context<Self>,
    ) {
        // Unread accounting BEFORE the snapshot swap: any workspace whose
        // status TRANSITIONED into an attention/terminal state while the
        // Agents page is closed bumps the nav badge. Comparing against the
        // previous snapshot (not absolute states) means a long-finished
        // session doesn't re-count on every refresh.
        if self.active_nav != Some(NavItem::Agents) {
            for (ws_id, status) in &latest_status {
                let was = self.latest_status.get(ws_id).cloned().flatten();
                if status.as_ref() != was.as_ref()
                    && status.as_ref().is_some_and(needs_attention)
                {
                    self.agents_unread = self.agents_unread.saturating_add(1);
                }
            }
        }
        // Dirty-check: `WorkspaceRoot::render` pushes this snapshot down every
        // frame (and re-renders on every pane-output tick while an agent
        // streams), so notifying unconditionally rebuilds the whole rail
        // constantly — the hover/scroll jank. Only re-render when the rail's
        // render inputs actually changed. Every genuine live change already
        // routes through `mark_rail_dirty` (status edges), `note_agent_sideband`
        // (tool steps), or the per-frame ambient/diff maps → one of these
        // compared fields changes, so the rail still updates; we only drop the
        // redundant repaints. `status_rx` is excluded (not `Eq`); its live status
        // surfaces via `latest_status`/`agent_sideband`/`ambient_status`, which
        // ARE compared (ambient rows also carry their prompt in `persisted_title`,
        // compared by `agents_display_equal`).
        let changed = self.projects != projects
            || self.active_project_id != active_project_id
            || self.active_workspace_id != active_workspace_id
            || self.workspaces_by_project != workspaces_by_project
            || self.latest_status != latest_status
            || self.ambient_status != ambient_status
            || self.latest_adapter != latest_adapter
            || self.live_worktrees != live_worktrees
            || self.diff_counts != diff_counts
            || self.agent_activity != agent_activity
            || self.agent_sideband != agent_sideband
            || self.last_active != last_active
            || self.focused_agent != focused_agent
            || !agents_display_equal(&self.workspace_agents, &workspace_agents);

        self.projects = projects;
        self.active_project_id = active_project_id;
        // A locate glow is scoped to the workspace it was triggered on —
        // reset on switch so the NEXT active card doesn't replay it
        // uninvited (a fresh tree position would re-run the animation).
        if self.active_workspace_id != active_workspace_id {
            self.locate_glow_seq = 0;
        }
        self.active_workspace_id = active_workspace_id;
        self.workspaces_by_project = workspaces_by_project;
        self.latest_status = latest_status;
        self.ambient_status = ambient_status;
        self.latest_adapter = latest_adapter;
        self.live_worktrees = live_worktrees;
        self.diff_counts = diff_counts;
        self.agent_activity = agent_activity;
        self.agent_sideband = agent_sideband;
        self.last_active = last_active;
        self.workspace_agents = workspace_agents;
        self.focused_agent = focused_agent;

        // Only repaint the rail when its inputs changed (see the dirty-check
        // above). A no-op frame keeps the last render — no churn on hover or
        // while an agent merely streams output.
        if changed {
            // Refresh the settle cache off the render path so render only reads
            // it. Gated on `changed` for the same reason as notify — settle
            // inputs (order, pinned, status) only move when the snapshot does;
            // the expiry timer handles the time-only case.
            self.refresh_settle_cache(cx);
            cx.notify();
        }
    }

    /// Flip the multi-agent disclosure for one workspace. The caller notifies;
    /// this only mutates the in-memory expand set.
    pub(crate) fn toggle_workspace_expanded(&mut self, workspace_key: &str) {
        if !self.expanded_workspaces.remove(workspace_key) {
            self.expanded_workspaces.insert(workspace_key.to_string());
        }
    }

    /// Test-only inspector for the currently-active nav item (`None` = home).
    #[doc(hidden)]
    pub fn active_nav(&self) -> Option<NavItem> {
        self.active_nav
    }

    /// Resolve the active workspace's tint swatch from the current snapshot.
    /// `WorkspaceRoot` reads this each render to accent the active tab strip,
    /// so the tint stays in sync without a separately-cached copy.
    pub(crate) fn active_workspace_tint(
        &self,
    ) -> Option<crate::shell::pane_group::TabColor> {
        let ws_id = self.active_workspace_id.as_ref()?;
        let proj_id = self.active_project_id.as_ref()?;
        self.workspaces_by_project
            .get(proj_id)?
            .iter()
            .find(|w| &w.id == ws_id)?
            .tint
            .as_deref()
            .and_then(crate::shell::pane_group::TabColor::from_slug)
    }

    /// Return to the home view (workspace list, no nav highlighted). Used after
    /// creating a workspace from the Tasks page so the new row is visible.
    pub(crate) fn go_home(&mut self, cx: &mut Context<Self>) {
        if self.active_nav.is_some() {
            self.active_nav = None;
            cx.notify();
        }
    }

    /// Toggle a nav page. Clicking the active page returns to the home view
    /// (workspace list, no nav row highlighted).
    ///
    /// Tasks and Automations are not rail bodies — they live in the pane
    /// group. Callers that have a `&mut Window` should use `select_nav_in` so
    /// those get the window context required to open the pane tab.
    pub fn select_nav(&mut self, item: NavItem, cx: &mut Context<Self>) {
        // Pane-hosted pages skip the rail-body toggle path entirely.
        if item.opens_in_pane() {
            return;
        }
        self.active_nav = if self.active_nav == Some(item) {
            None
        } else {
            Some(item)
        };
        if self.active_nav == Some(NavItem::Agents) {
            self.agents_unread = 0;
        }
        cx.notify();
    }

    /// Version of `select_nav` that carries `&mut Window`, required when
    /// opening a pane-hosted page (RT-1). Dispatches up through `weak_root` to
    /// open the singleton tab in the active project's pane group; everything
    /// else delegates to `select_nav`.
    pub fn select_nav_in(
        &mut self,
        item: NavItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match item {
            NavItem::Tasks => {
                let _ = self.weak_root.update(cx, |root, cx| {
                    root.open_tasks_tab(window, cx);
                });
            }
            NavItem::Automations => {
                let _ = self.weak_root.update(cx, |root, cx| {
                    root.open_automations_tab(window, cx);
                });
            }
            NavItem::Agents | NavItem::Search => self.select_nav(item, cx),
        }
    }
}

/// Compare two per-workspace agent lists by their RENDER-VISIBLE fields only,
/// skipping `status_rx` (a `watch::Receiver`, not `Eq`). The live status the
/// receiver carries surfaces through `latest_status`/`agent_sideband`, which the
/// caller compares separately; an ambient row's prompt rides `persisted_title`,
/// compared here — so this projection is enough to decide whether the disclosure
/// rows would look any different.
fn agents_display_equal(a: &WorkspaceAgentList, b: &WorkspaceAgentList) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(key, rows_a)| {
        b.get(key).is_some_and(|rows_b| {
            rows_a.len() == rows_b.len()
                && rows_a.iter().zip(rows_b).all(|(x, y)| {
                    x.db_id == y.db_id
                        && x.is_live == y.is_live
                        && x.db_status == y.db_status
                        && x.age_label == y.age_label
                        && x.label == y.label
                        && x.adapter_id == y.adapter_id
                        && x.persisted_title == y.persisted_title
                        && x.persisted_message == y.persisted_message
                })
        })
    })
}

impl Render for LeftRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // A resize drag is over once no drag is active — releasing the
        // button produces no further drag-move ticks, so the flag is
        // cleared here on the next render instead.
        if self.resizing && !cx.has_active_drag() {
            self.resizing = false;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let entity = cx.entity().clone();

        // The flex-1 body slot changes depending on the active nav page.
        // Agents → agents dashboard; Tasks → issue/PR browser; home (None) and
        // the not-yet-built shells → the workspace list.
        let content_body: gpui::AnyElement = if self.active_nav == Some(NavItem::Agents) {
            let filter_input = self.ensure_dashboard_filter_input(_window, cx);
            let status_filter = self.dashboard_status_filter;
            let filter_text = self.dashboard_filter.clone();
            // Long-form card ages are relative to this render's clock.
            let now = chrono::Utc::now().to_rfc3339();
            render_agents_dashboard(
                &self.workspace_agents,
                &self.projects,
                &self.workspaces_by_project,
                &now,
                self.focused_agent.as_ref(),
                status_filter,
                &filter_text,
                filter_input,
                entity.clone(),
                self.weak_root.clone(),
                &self.agents_scroll,
                theme,
                density,
                &typography,
            )
        } else {
            // Read the pre-computed Smart-sort settle overrides (refreshed off
            // the render path, never mutated here) so held orders override the
            // freshly-computed ones for this render.
            let settle_overrides = self.settle_override_cache.clone();
            let renaming_id = self.renaming_workspace_id().map(str::to_string);
            let rename_input = self.rename_input.clone();
            let workspace_list = render_workspace_list(
                self.projects.clone(),
                self.active_project_id.clone(),
                self.active_workspace_id.clone(),
                self.collapsed.clone(),
                self.sort_mode,
                self.group_mode,
                entity.clone(),
                self.workspaces_by_project.clone(),
                self.latest_status.clone(),
                self.live_worktrees.clone(),
                self.ambient_status.clone(),
                self.latest_adapter.clone(),
                self.diff_counts.clone(),
                self.workspace_agents.clone(),
                self.expanded_workspaces.clone(),
                self.focused_agent.clone(),
                self.weak_root.clone(),
                self.locate_glow_seq,
                self.list_scroll.clone(),
                settle_overrides,
                renaming_id,
                rename_input,
                self.compact_cards,
                theme,
                density,
                &typography,
            );
            div()
                .flex()
                .flex_col()
                .h_full()
                .w_full()
                .child(workspace_header(
                    &entity,
                    &self.weak_root,
                    self.active_project_id.is_some(),
                    theme,
                    density,
                    &typography,
                ))
                // Scrollable list viewport. `min_h(0)` lets the flex child
                // shrink below its content so overflow actually engages.
                // The scroll handle itself is tracked on the list COLUMN
                // (inside `render_workspace_list`, children = project
                // groups) so `scroll_to_item` indexes project groups.
                .child(div().flex_1().w_full().min_h(px(0.)).child(workspace_list))
                .into_any_element()
        };

        // Body fills the column minus the right-edge resize handle.
        // The rail surface is painted by the ROOT row (not here) so the
        // resize-handle column shares it — an unfilled handle column
        // reads as a dark gutter against the lifted rail.
        let body = div()
            .flex()
            .flex_col()
            .h_full()
            .flex_1()
            .min_w_0()
            .child(render_nav_section(
                self.active_nav,
                self.agents_unread,
                &entity,
                theme,
                density,
                &typography,
            ))
            .child(divider(theme))
            .child(div().flex_1().w_full().child(content_body))
            .child(render_toolbar(
                entity.downgrade(),
                theme,
                density,
                &typography,
            ));

        let weak_root_for_drop = self.weak_root.clone();
        div()
            .id("left-rail-root")
            .flex()
            .flex_row()
            .h_full()
            .w(self.width)
            .bg(theme.bg_rail)
            // OS-native folder drop target: tint while a directory drag
            // hovers the rail, register + activate the folder(s) on drop.
            // File (non-directory) drops are ignored.
            .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| {
                style.bg(Hsla {
                    a: 0.4,
                    ..theme.selection
                })
            })
            .on_drop::<gpui::ExternalPaths>(move |payload, window, cx| {
                let dirs: Vec<std::path::PathBuf> = payload
                    .paths()
                    .iter()
                    .filter(|p| p.is_dir())
                    .cloned()
                    .collect();
                if dirs.is_empty() {
                    return;
                }
                let _ = weak_root_for_drop.update(cx, |root, cx| {
                    for dir in dirs {
                        root.add_project_from_drop(dir, window, cx);
                    }
                });
            })
            .child(body)
            .child(resize::build_handle(self.resizing, theme))
    }
}

#[allow(clippy::too_many_arguments)]
fn render_workspace_list(
    projects: Vec<Project>,
    active_project_id: Option<String>,
    active_workspace_id: Option<String>,
    collapsed: HashSet<String>,
    sort_mode: WorkspaceSortMode,
    group_mode: WorkspaceGroupMode,
    rail: gpui::Entity<LeftRail>,
    workspaces_by_project: HashMap<String, Vec<Workspace>>,
    latest_status: LatestStatusMap,
    live_worktrees: HashSet<String>,
    ambient_status: HashMap<String, AmbientAgent>,
    latest_adapter: HashMap<String, String>,
    diff_counts: HashMap<String, DiffCounts>,
    workspace_agents: WorkspaceAgentList,
    expanded_workspaces: HashSet<String>,
    focused_agent: Option<RailAgentTarget>,
    weak_root: WeakEntity<WorkspaceRoot>,
    locate_glow_seq: u64,
    list_scroll: ScrollHandle,
    settle_overrides: HashMap<String, Vec<String>>,
    renaming_id: Option<String>,
    rename_input: Option<Entity<InputState>>,
    compact: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> gpui::AnyElement {
    if projects.is_empty() {
        return open_project_cta(theme, density, typography).into_any_element();
    }

    // Drag auto-scroll: while a workspace- or project-reorder drag holds the
    // cursor near a list edge, nudge the scroll offset so off-screen rows are
    // reachable. The move listeners read the cursor against this container's
    // bounds; the rail's tick loop does the actual scrolling.
    let rail_for_ws_scroll = rail.clone();
    let rail_for_proj_scroll = rail.clone();

    // The column is the scroll container: its direct children are the
    // project groups, so `ScrollHandle::scroll_to_item(project_index)`
    // brings the active project's group into view.
    let mut col = div()
        .id("left-rail-workspace-list")
        .flex()
        .flex_col()
        .w_full()
        .h_full()
        .overflow_y_scroll()
        .track_scroll(&list_scroll)
        .on_drag_move::<crate::shell::left_rail::project_drag::WorkspaceDragPayload>(
            move |ev: &DragMoveEvent<
                crate::shell::left_rail::project_drag::WorkspaceDragPayload,
            >,
                  _window,
                  cx| {
                let cursor_y = f32::from(ev.event.position.y);
                let bounds = ev.bounds;
                rail_for_ws_scroll
                    .update(cx, |r, cx| r.note_autoscroll_cursor(cursor_y, bounds, cx));
            },
        )
        .on_drag_move::<crate::shell::left_rail::project_drag::ProjectDragPayload>(
            move |ev: &DragMoveEvent<
                crate::shell::left_rail::project_drag::ProjectDragPayload,
            >,
                  _window,
                  cx| {
                let cursor_y = f32::from(ev.event.position.y);
                let bounds = ev.bounds;
                rail_for_proj_scroll
                    .update(cx, |r, cx| r.note_autoscroll_cursor(cursor_y, bounds, cx));
            },
        );

    // Flat (ungrouped) list: every workspace across all projects in one
    // globally-sorted column with no project headers. Drag-reorder is disabled
    // here because ordering across project boundaries is undefined.
    if group_mode == WorkspaceGroupMode::Flat {
        let status_for_flat = latest_status.clone();
        let latest_status_for =
            move |workspace_id: &str| status_for_flat.get(workspace_id).cloned().flatten();
        let adapter_for_flat = latest_adapter.clone();
        let latest_adapter_for = move |workspace_id: &str| -> Option<&'static str> {
            adapter_for_flat
                .get(workspace_id)
                .map(|id| crate::shell::agent_presentation::adapter_display_name(id))
        };
        let weak_root_for_menu = weak_root.clone();
        let on_row_menu = move |workspace: Workspace,
                                x: f32,
                                y: f32,
                                _window: &mut gpui::Window,
                                cx: &mut gpui::App| {
            let _ =
                weak_root_for_menu.update(cx, |root, cx| root.open_row_menu(workspace, x, y, cx));
        };

        // Pair each workspace with its owning project, then order the whole
        // list by the active sort mode.
        let mut flat: Vec<(Project, Workspace)> = Vec::new();
        for project in projects.iter() {
            if let Some(ws) = workspaces_by_project.get(&project.id) {
                for w in ws {
                    flat.push((project.clone(), w.clone()));
                }
            }
        }
        match sort_mode {
            WorkspaceSortMode::Name => {
                flat.sort_by_key(|a| a.1.name.to_lowercase());
            }
            WorkspaceSortMode::Project => flat.sort_by(|a, b| {
                a.0.name
                    .to_lowercase()
                    .cmp(&b.0.name.to_lowercase())
                    .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
            }),
            WorkspaceSortMode::Recent => {
                flat.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));
            }
            WorkspaceSortMode::Manual => {
                flat.sort_by(|a, b| a.1.sort_order.total_cmp(&b.1.sort_order));
            }
            WorkspaceSortMode::Smart => flat.sort_by_key(|(_, w)| {
                let status = latest_status.get(&w.id).cloned().flatten();
                attention_rank(status.as_ref(), live_worktrees.contains(&w.worktree_path))
            }),
        }

        // Pinned rows float to the top regardless of mode. The sort is stable,
        // so the active mode's order is preserved within the pinned and
        // unpinned partitions — mirroring the grouped path's pinned-above
        // contract (`false` < `true`, so `!pinned` lifts pinned rows first).
        flat.sort_by_key(|(_, w)| !w.pinned);

        for (row_index, (project, workspace)) in flat.into_iter().enumerate() {
            col = col.child(render_workspace_block(
                workspace,
                row_index,
                &project,
                sort_mode,
                &latest_status_for,
                &latest_adapter_for,
                active_workspace_id.as_deref(),
                &live_worktrees,
                &ambient_status,
                &diff_counts,
                &workspace_agents,
                &expanded_workspaces,
                focused_agent.as_ref(),
                &rail,
                &weak_root,
                &on_row_menu,
                false,
                locate_glow_seq,
                renaming_id.as_deref(),
                &rename_input,
                compact,
                theme,
                density,
                typography,
            ));
        }
        return col.into_any_element();
    }

    for (project_index, project) in projects.into_iter().enumerate() {
        let workspaces = workspaces_by_project
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        // Order rows per the active sort mode. The attention tier reuses the
        // same ranking the agents dashboard uses, so both surfaces agree on
        // what "needs attention" means.
        let workspaces = sort_workspaces(&workspaces, &project.root_path, sort_mode, |ws| {
            let status = latest_status.get(&ws.id).cloned().flatten();
            attention_rank(status.as_ref(), live_worktrees.contains(&ws.worktree_path))
        });
        // Apply a held Smart-sort order, if the settle window is keeping this
        // group's rows from reshuffling under the cursor.
        let workspaces = match settle_overrides.get(&project.id) {
            Some(order) => reorder_to_id_sequence(workspaces, order),
            None => workspaces,
        };
        let is_active = active_project_id.as_deref() == Some(project.id.as_str());
        let is_collapsed = collapsed.contains(&project.id);
        let plan = build_project_group_plan(&project, &workspaces, is_active, is_collapsed);

        let status_for_group = latest_status.clone();
        let latest_status_for =
            move |workspace_id: &str| status_for_group.get(workspace_id).cloned().flatten();

        // Resolve the tracked-session adapter display name for a workspace,
        // so a spawned agent shows its name on the card the same way a
        // hand-launched (ambient) one does.
        let adapter_for_group = latest_adapter.clone();
        let latest_adapter_for = move |workspace_id: &str| -> Option<&'static str> {
            adapter_for_group
                .get(workspace_id)
                .map(|id| crate::shell::agent_presentation::adapter_display_name(id))
        };

        let weak_root_for_menu = weak_root.clone();
        let on_row_menu = move |workspace: Workspace,
                                x: f32,
                                y: f32,
                                _window: &mut gpui::Window,
                                cx: &mut gpui::App| {
            let _ =
                weak_root_for_menu.update(cx, |root, cx| root.open_row_menu(workspace, x, y, cx));
        };

        let weak_root_for_project_menu = weak_root.clone();
        let on_project_menu = move |project: Project,
                                    x: f32,
                                    y: f32,
                                    _window: &mut gpui::Window,
                                    cx: &mut gpui::App| {
            let _ = weak_root_for_project_menu
                .update(cx, |root, cx| root.open_project_menu(project, x, y, cx));
        };

        col = col.child(render_project_group(
            plan,
            project,
            project_index,
            sort_mode,
            workspaces,
            latest_status_for,
            latest_adapter_for,
            active_workspace_id.as_deref(),
            &live_worktrees,
            &ambient_status,
            &diff_counts,
            &workspace_agents,
            &expanded_workspaces,
            focused_agent.as_ref(),
            rail.clone(),
            weak_root.clone(),
            on_row_menu,
            on_project_menu,
            locate_glow_seq,
            renaming_id.as_deref(),
            rename_input.clone(),
            compact,
            theme,
            density,
            typography,
        ));
    }
    col.into_any_element()
}

/// Empty-state row: clickable "Open a project (⌘O)" that dispatches the
/// project-picker action.
fn open_project_cta(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .id("left-rail-open-project-cta")
        .flex()
        .items_center()
        .justify_center()
        .h(px(60.))
        .px(px(density.pad_panel))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .hover(|s| s.text_color(theme.fg_base))
        .child(open_project_hint())
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(OpenProjectPicker), cx);
        })
}

/// "Open a project (⌘O)" with the chord resolved from the keymap registry
/// (drops the parens when the action is unbound).
fn open_project_hint() -> String {
    match crate::keymap_registry::display_chord_for("open_project_picker") {
        Some(chord) => format!("Open a project ({chord})"),
        None => "Open a project".to_string(),
    }
}

fn divider(theme: Theme) -> impl IntoElement {
    div().w_full().h(px(1.)).bg(theme.border_inactive)
}

fn workspace_header(
    rail: &gpui::Entity<LeftRail>,
    weak_root: &WeakEntity<WorkspaceRoot>,
    has_active_project: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row + 4.))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(
            // Section title.
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .child("Projects"),
        )
        .child(options_icon(rail.clone(), weak_root.clone(), theme))
        .child(add_project_icon(theme))
        .child(new_workspace_icon(has_active_project, theme))
}

/// Display-options trigger: opens the dropdown holding group-by, sort, card
/// layout, and collapse-all. Reads the rail's current display state on click so
/// the menu opens with the live selections checked.
fn options_icon(
    rail: gpui::Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
) -> impl IntoElement {
    div()
        .id("workspaces-header-options")
        .cursor_pointer()
        .text_color(theme.fg_muted)
        .hover(|s| s.text_color(theme.fg_base))
        .tooltip(|window, cx| {
            gpui_component::tooltip::Tooltip::new("Display options").build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _window, cx| {
            let x = f32::from(ev.position.x);
            let y = f32::from(ev.position.y);
            let sort = rail.read(cx).sort_mode();
            let group = rail.read(cx).group_mode();
            let compact = rail.read(cx).compact_cards();
            let _ = weak_root.update(cx, |root, cx| {
                root.open_options_menu(sort, group, compact, x, y, cx);
            });
        })
        .child(
            svg()
                .path("icons/settings-2.svg")
                .size(px(HEADER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
}

/// Always-visible add-project affordance in the Projects header. Opens the
/// same add-project dialog as the bottom toolbar — surfaced here so the action
/// is discoverable without hunting the toolbar.
fn add_project_icon(theme: Theme) -> impl IntoElement {
    div()
        .id("workspaces-header-add-project")
        .cursor_pointer()
        .text_color(theme.fg_muted)
        .hover(|s| s.text_color(theme.fg_base))
        .tooltip(|window, cx| {
            gpui_component::tooltip::Tooltip::new("Add project").build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(OpenAddProjectDialog), cx);
        })
        .child(
            svg()
                .path("icons/folder-plus.svg")
                .size(px(HEADER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
}

/// Always-visible new-workspace affordance in the Projects header. Creates a
/// worktree under the active project (the same action the per-project hover
/// `+` dispatches). Rendered dimmed and inert when no project is active, since
/// there is nothing to create the worktree under.
fn new_workspace_icon(has_active_project: bool, theme: Theme) -> impl IntoElement {
    let icon_color = if has_active_project {
        theme.fg_muted
    } else {
        theme.fg_subtle
    };
    let tip = if has_active_project {
        "New workspace"
    } else {
        "New workspace — open a project first"
    };
    let mut el = div()
        .id("workspaces-header-new-workspace")
        .text_color(icon_color)
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tip).build(window, cx)
        });
    if has_active_project {
        el = el
            .cursor_pointer()
            .hover(|s| s.text_color(theme.fg_base))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
                window.dispatch_action(Box::new(OpenWorkspaceCreate), cx);
            });
    }
    el.child(
        svg()
            .path("icons/plus.svg")
            .size(px(HEADER_ICON_SIZE))
            .text_color(icon_color),
    )
}

#[cfg(test)]
mod tests {
    use super::{reorder_to_id_sequence, same_membership};
    use oximux_core::Workspace;

    fn ws(id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: "p".to_string(),
            name: id.to_string(),
            slug: id.to_string(),
            branch: format!("oximux/{id}"),
            worktree_path: format!("/tmp/{id}"),
            status: "active".to_string(),
            created_at: "2026-06-16T00:00:00+00:00".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    #[test]
    fn same_membership_ignores_order() {
        let a = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let b = vec!["z".to_string(), "x".to_string(), "y".to_string()];
        assert!(same_membership(&a, &b));
    }

    #[test]
    fn same_membership_detects_added_or_removed_row() {
        let a = vec!["x".to_string(), "y".to_string()];
        let added = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let removed = vec!["x".to_string()];
        assert!(!same_membership(&a, &added));
        assert!(!same_membership(&a, &removed));
    }

    #[test]
    fn agents_display_equal_skips_status_rx_catches_visible_fields() {
        use crate::shell::left_rail::{RailAgentRow, RailAgentTarget};
        use oximux_core::{AgentSnapshot, AgentStatus};
        use std::collections::HashMap;
        use tokio::sync::watch;

        // Each row gets its OWN status receiver instance — the compare must
        // ignore those and look only at the visible fields.
        fn row(age: &str, title: Option<&str>) -> RailAgentRow {
            let (_tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Running));
            RailAgentRow {
                db_id: "a".into(),
                target: RailAgentTarget::AgentSession { db_id: "a".into() },
                workspace_key: "ws".into(),
                adapter_id: "claude-code".into(),
                label: "Claude Code".into(),
                is_live: true,
                status_rx: Some(rx),
                db_status: AgentStatus::Idle,
                started_at: Some("t".into()),
                ended_at: None,
                age_label: age.into(),
                persisted_title: title.map(str::to_string),
                persisted_message: None,
                ambient_detail: None,
            }
        }
        let list = |rows: Vec<RailAgentRow>| {
            let mut m: super::WorkspaceAgentList = HashMap::new();
            m.insert("ws".to_string(), rows);
            m
        };

        // Same visible fields, different receiver instances → equal.
        assert!(super::agents_display_equal(
            &list(vec![row("now", None)]),
            &list(vec![row("now", None)])
        ));
        // A changed age label → not equal (the row would render differently).
        assert!(!super::agents_display_equal(
            &list(vec![row("now", None)]),
            &list(vec![row("3d", None)])
        ));
        // A changed prompt title (e.g. an ambient agent's new prompt) → not equal.
        assert!(!super::agents_display_equal(
            &list(vec![row("now", Some("old"))]),
            &list(vec![row("now", Some("new"))])
        ));
        // Different agent count → not equal.
        assert!(!super::agents_display_equal(
            &list(vec![row("now", None)]),
            &list(vec![row("now", None), row("now", None)])
        ));
    }

    #[test]
    fn reorder_applies_held_sequence() {
        // A held settle order pins the displayed positions even though the
        // input list is in a different (freshly-sorted) order.
        let fresh = vec![ws("b"), ws("c"), ws("a")];
        let held = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = reorder_to_id_sequence(fresh, &held);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn reorder_keeps_unlisted_rows_at_tail() {
        // A row not named in the held order (shouldn't happen while membership
        // matches) still renders rather than vanishing.
        let fresh = vec![ws("a"), ws("b"), ws("new")];
        let held = vec!["b".to_string(), "a".to_string()];
        let out = reorder_to_id_sequence(fresh, &held);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(&ids[..2], ["b", "a"]);
        assert!(ids.contains(&"new"));
        assert_eq!(ids.len(), 3);
    }
}
