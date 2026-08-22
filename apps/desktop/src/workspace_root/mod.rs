//! WorkspaceRoot — the top-level view mounted into the GPUI window.
//!
//! Composition (per-column workspace pattern): three columns side-by-side,
//! each topped by its own 40px header strip. There is NO full-width chrome
//! row — the workspace tab strip is confined to the center column's width
//! so it never extends across the side panels.
//!
//! ```text
//! ┌──────────────┬─────────────────────────────┬──────────────────┐
//! │ left header  │ center header               │ right header     │  ← 40px
//! │ (chrome)     │ (workspace tab strip)       │ (activity + ×)   │
//! ├──────────────┼─────────────────────────────┼──────────────────┤
//! │              │                             │                  │
//! │ left rail    │ active project's pane groups │ right sidebar    │  ← flex_1
//! │ (250px)      │ (flex_1)                    │ panel (360px)    │
//! │              │                             │                  │
//! ├──────────────┴─────────────────────────────┴──────────────────┤
//! │ StatusBar (24px)                                              │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! When a side panel is collapsed, its column disappears entirely and the
//! chrome bits (wordmark/toggles) move into the center header so they stay
//! reachable — mirrors the `titlebar-left` floating behavior found in
//! similar workspace shells.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, AppContext, Context, DragMoveEvent, Entity, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use oximux_agents::{AdapterRegistry, AgentRuntime, AgentSessionConfig, CliRuntime};
use oximux_core::{AgentAdapter, Project};
use oximux_settings::{Density, Theme, Typography};


/// Cadence of the rail's per-worktree diff-count refresh. Deliberately slower
/// than the SCM status poll: each round shells out `git diff --numstat` per
/// worktree, so a tight interval would spawn many short-lived git processes.
/// 2 s keeps the `+A −B` chips feeling live without churning the disk.
const DIFF_REFRESH_TICK: Duration = Duration::from_millis(2000);

/// Cadence of the layout/relay-id autosave. Bounds what an app crash can
/// lose of mid-session structural changes (new tabs, splits, closes) to
/// one tick — quit/switch captures remain the authoritative full saves.
/// Idle ticks cost nothing: the save sink dedupes byte-identical layout
/// JSON, and the relay-id capture uses the cached handshake session id
/// (no daemon round-trip).
const LAYOUT_AUTOSAVE_TICK: Duration = Duration::from_secs(15);

/// Cadence of the agent-activity tail. Each tick does one bounded (64 KiB)
/// tail read per Running primary-CLI session on the background executor;
/// 2 s matches the phase target of "activity tracks tool changes within
/// ~2 s" without measurable IO churn.
const AGENT_ACTIVITY_TICK: Duration = Duration::from_secs(2);

/// Cadence of the listening-ports scan. Set by how long it is tolerable to
/// wait after `npm run dev` prints its URL — a couple of seconds reads as
/// "immediately", ten does not — rather than by cost: the socket read is one
/// kernel call on Windows and one small `/proc` read on Linux. Like the diff
/// refresh, it pauses while the window is unfocused and is kicked once on
/// focus regain, so a server started while you were away is listed by the
/// time you have looked back.
const PORT_SCAN_TICK: Duration = Duration::from_secs(3);

/// Cadence of the usage-meter sample. The probe re-parses only logs whose
/// (mtime, len) changed since the previous sample, so a steady-state tick
/// costs one directory scan + at most one active-log re-parse.
const USAGE_METER_TICK: Duration = Duration::from_secs(60);

use crate::notifier::{AgentNotifySettings, Notifier};
use crate::state::AppState;

use crate::actions::{
    ActivateGroupTab, ActivateWorkspaceFromJump, ApplyLayoutBottomTerminal, ApplyLayoutHorizontal,
    ApplyLayoutStacked, CloseGroup, CloseTab, DismissOverlay, MoveTabToNewWindow,
    OpenAddProjectDialog, OpenComposerBar,
    OpenCommandPalette, OpenCommitContextMenuAt, OpenCommitDialog, OpenFileFromContextMenu,
    CreateWorktreeWorkspaceForActiveChat,
    OpenFileTreeContextMenuAt, OpenGitRowContextMenuAt, OpenPaneActions, OpenPaneActionsAt,
    NewBrowserTab, NewTab, OpenChatSession, OpenProjectPicker, OpenQuickOpen, OpenSessionHistory,
    OpenSettings, RestartToUpdate, ShowWelcomeWizard,
    OpenTabContextMenuAt, OpenTerminalContextMenuAt, ResumeAgentSession,
    OpenWorkspaceCreate, OpenWorkspaceJump, RequestOpenAdapterPicker, Search, SelectExplorerTab,
    SelectFilesTab, SelectHistoryTab,
    SelectSearchTab,
    SelectSourceControlTab, SendTextToActiveAgent, SplitDown, SplitGroupAt, SplitHorizontal,
    SplitLeft, SplitRight, SplitUp, SplitVertical, ToggleChatTerminalView, ToggleDictation,
    ToggleFloatingTerminal, ToggleLeftSidebar, ToggleRightSidebar, UiZoomIn, UiZoomOut,
    UiZoomReset,
};
use crate::shell::pane_tree::{Axis, SplitInsert};
use crate::shell::{
    adapter_picker::{AdapterPicker, AdapterSelection, OnSelect},
    add_project_dialog::AddProjectDialog,
    command_palette::{PaletteEvent, PaletteModal, entry::PaletteMode},
    confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt},
    file_tree_context_menu::FileTreeContextMenu,
    git_panel::{
        DiscardRequested, GitPanel, ShowCombinedDiffRequested,
        row_context_menu::{GitRowContextMenu, GitRowContextTarget},
    },
    source_control::{
        branch_commits::{ShowBranchDiffAllRequested, ShowBranchFileRequested},
        commit_context_menu::CommitContextMenu,
        graph::ShowCommitRequested,
    },
    stash_panel::{
        PushStashRequested, StashPanel,
        push_dialog::{CancelCallback, PushCallback, PushStashDialog, PushStashPrompt},
    },
    left_rail::{
        LeftRail,
        dashboard_status_menu::DashboardStatusFilterMenu,
        options_menu::WorkspaceOptionsMenu,
        project_menu::ProjectRowMenu,
        row_menu::WorkspaceRowMenu,
        workspace_row::{DiffCounts, looks_like_renormalization, sum_numstat},
    },
    main_area,
    openable_text_file::is_openable_text_file,
    pane_actions::{PaneActionsAnchor, PaneActionsMenu},
    project_panes::ProjectPanes,
    project_picker::{OnPick, ProjectPickerEvent, ProjectPickerModal},
    session_history::{SessionHistoryEvent, SessionHistoryModal},
    settings_modal::{SettingsModal, SettingsModalEvent},
    right_sidebar::{
        RightSidebar, activity_bar::render_tab_buttons, layout::DEFAULT_PANEL_WIDTH, tab::RightTab,
    },
    status_bar,
    tab_context_menu::TabContextMenu,
    terminal_context_menu::TerminalContextMenu,
    toast::{ToastKind, ToastLayer},
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS},
    top_bar,
    workspace_dialog::{OnSubmit as OnWorkspaceSubmit, WorkspaceDialog},
    workspace_ops::{WorkspaceNavRef, build_add_project_dialog},
};
use std::rc::Rc;

/// Approximate horizontal inset (CSS px) of the `+` button from its
/// column's left edge. Used by the adapter picker's anchor calculation;
/// the strip can scroll when overflowing so this is intentionally a
/// stable approximation rather than a live layout query.
const ADAPTER_PICKER_LEFT_INSET: f32 = 8.0;

/// Map a settings-layer launch transport to the agents-layer runtime transport
/// (two deliberately separate enums — see the P0 seam design).
fn to_agents_transport(t: oximux_settings::Transport) -> oximux_agents::thread::Transport {
    match t {
        oximux_settings::Transport::StreamJson => oximux_agents::thread::Transport::StreamJson,
        oximux_settings::Transport::AppServer => oximux_agents::thread::Transport::AppServer,
        oximux_settings::Transport::Acp => oximux_agents::thread::Transport::Acp,
        oximux_settings::Transport::Rpc => oximux_agents::thread::Transport::Rpc,
    }
}

/// Resolve the chat backend a launch of `adapter_id` should open over: the
/// mapped transport plus, for an ACP adapter, its configured command + args
/// (shell-split). Non-ACP adapters carry an empty command — the factory ignores
/// the `acp_*` fields for them.
pub(crate) fn chat_backend_for(
    settings: &oximux_settings::AgentLaunchSettings,
    adapter_id: &str,
) -> oximux_agents::thread::ChatBackend {
    let transport = to_agents_transport(settings.transport_for(adapter_id));
    let is_acp = transport == oximux_agents::thread::Transport::Acp;
    oximux_agents::thread::ChatBackend {
        transport,
        acp_command: is_acp.then(|| settings.acp_command_for(adapter_id)).flatten(),
        acp_args: if is_acp { settings.acp_args_for(adapter_id) } else { Vec::new() },
    }
}

pub struct WorkspaceRoot {
    pub(crate) theme: Theme,
    pub(crate) density: Density,
    pub(crate) typography: Typography,
    /// One `ProjectPanes` entity per project, keyed by `Project.id`. Each
    /// `ProjectPanes` owns the project's pane-group layout tree. State
    /// persists across project switches (entity stays alive in the map);
    /// `active_project_panes()` resolves the current entity via
    /// `active_project.id`.
    pub(crate) project_panes_by_project: HashMap<String, Entity<ProjectPanes>>,
    /// `right_sidebar` is the ACTIVE sidebar (the one rendered + wired to SCM
    /// subscriptions). This map keeps the previously-built sidebar for every
    /// visited project so a switch-back reuses the live entity instead of
    /// tearing it down and rebuilding (which re-opened the repo, respawned the
    /// status poller, re-ran `git log` for the commit graph, and rescanned the
    /// file tree — every one a "Loading…" flash). Mirrors
    /// `project_panes_by_project`: lazy-built on first activation, reused after.
    /// Only the active project's poller ticks; inactive sidebars are paused via
    /// `set_polling_focused(false)` so N cached sidebars don't run N concurrent
    /// status polls.
    pub(crate) right_sidebar_by_project: HashMap<String, Entity<RightSidebar>>,
    pub(crate) right_sidebar: Option<Entity<RightSidebar>>,
    pub(crate) left_rail: Entity<LeftRail>,
    pub(crate) palette: Entity<PaletteModal>,
    pub(crate) session_history: Entity<SessionHistoryModal>,
    pub(crate) pane_actions: Entity<PaneActionsMenu>,
    pub(crate) tab_context_menu: Entity<TabContextMenu>,
    /// File-tree right-click menu (Open / Open to the Side / Copy Path /
    /// Copy Relative Path / Reveal in Finder). Same shared-entity pattern
    /// as `tab_context_menu` — one menu owned at the workspace level so
    /// click-outside dismiss + z-order behave consistently.
    pub(crate) file_tree_context_menu: Entity<FileTreeContextMenu>,
    /// Right-click context menu for git_panel file + folder rows
    /// (Open in editor / Copy paths / Reveal in Finder / Stage /
    /// Unstage / Discard…). Same shared-entity pattern as
    /// `file_tree_context_menu`; scope (Single / Multi / Folder) is
    /// chosen by the action dispatcher based on the click payload +
    /// the panel's selection set.
    pub(crate) git_row_context_menu: Entity<GitRowContextMenu>,
    /// Right-click context menu for commit-graph rows
    /// (Cherry-pick / Revert / Copy SHA / Copy short SHA). Same
    /// shared-entity z-band as `git_row_context_menu`; mutually
    /// exclusive via close-on-open in the `OpenCommitContextMenuAt`
    /// handler. Holds a weak handle to the active `CommitArea` so
    /// Cherry-pick / Revert dispatches route through the existing
    /// single-flight `in_flight` flag.
    pub(crate) commit_context_menu: Entity<CommitContextMenu>,
    /// Right-click context menu for the terminal GRID (Copy / Paste / Select
    /// All / Clear / link / send-to-agent / split / tab ops). Same shared-
    /// entity z-band + click-outside dismiss as the other context menus; holds
    /// a weak handle to the right-clicked `TerminalView` so grid ops act on it
    /// directly across splits.
    pub(crate) terminal_context_menu: Entity<TerminalContextMenu>,
    /// Inline adapter-picker popover anchored to the workspace-tabs `+` button.
    pub(crate) adapter_picker: Entity<AdapterPicker>,
    /// PTY backend + status streams for every agent session. Held behind Arc
    /// so tab close and spawn paths share a single runtime.
    pub(crate) cli_runtime: Arc<CliRuntime>,
    /// macOS notification sink (or `NullNotifier` on non-mac). Cached so
    /// per-project `ProjectPanes` entities built lazily via
    /// `set_active_project` share the same notifier the initial mount used.
    pub(crate) notifier: Arc<dyn Notifier>,
    /// Cached registry of built-in adapters; resolves `AgentAdapter` at spawn.
    pub(crate) adapter_registry: Arc<AdapterRegistry>,
    /// Left rail visibility flag (Cmd+B).
    left_rail_open: bool,
    /// Bubbles ProjectPanes change notifications up so the workspace rerenders.
    pub(crate) _project_panes_observer: Option<Subscription>,
    /// Pauses + kicks StatusPoller on window blur/focus.
    _window_activation_observer: Subscription,
    /// macOS notification click watcher → activates the matching tab.
    _click_router: Task<()>,
    /// Persisted-state snapshot + repo handles. Hydrated once at boot.
    pub(crate) app_state: AppState,
    /// Project picker modal (Cmd+O).
    pub(crate) project_picker: Entity<ProjectPickerModal>,
    /// Settings modal (Cmd+, or left-rail cog). Minimal panes wiring the
    /// terminal + AI settings that already round-trip to disk, plus
    /// read-only keybindings / appearance / about references.
    pub(crate) settings_modal: Entity<SettingsModal>,
    /// First-run onboarding wizard (default agent + chat view). Opens on a
    /// fresh install via the boot-gate mailbox; reopenable from the palette.
    pub(crate) onboarding: Entity<crate::shell::onboarding::OnboardingWizard>,
    /// Quiet transient toast stack (bottom-right). Surfaces fleeting
    /// cross-surface events (agent done, commit failed, PR opened, clipboard)
    /// that the status bar's persistent state doesn't cover.
    pub(crate) toast_layer: Entity<ToastLayer>,
    /// The global "Listening…" voice-dictation HUD. Renders a floating pill when
    /// ⌘E dictates into a terminal or editor pane (the chat composer has its own
    /// in-line recording bar). Registered with the dictation service so the event
    /// drain can reach it.
    pub(crate) dictation_hud: Entity<crate::shell::agent_chat::DictationHud>,
    /// In-window floating ("PiP") terminal. `None` until first toggled;
    /// retained across hides (PTY persists) until the card's close button
    /// drops it. `floating_terminal_visible` gates whether it renders.
    pub(crate) floating_terminal: Option<Entity<crate::shell::floating_terminal::FloatingTerminal>>,
    pub(crate) floating_terminal_visible: bool,
    /// Subscription to the floating terminal's Close event.
    pub(crate) _floating_terminal_sub: Option<Subscription>,
    /// Restores workspace focus when the settings modal closes, so global key
    /// bindings (Cmd+,, etc.) keep dispatching instead of dying on an orphaned
    /// focus handle.
    pub(crate) _settings_modal_sub: Option<Subscription>,
    /// Focus-restore guard for the onboarding wizard's close.
    pub(crate) _onboarding_sub: Option<Subscription>,
    /// Same focus-restore guard for the command palette and project picker
    /// (both grab focus on open).
    pub(crate) _palette_sub: Option<Subscription>,
    pub(crate) _session_history_sub: Option<Subscription>,
    pub(crate) _project_picker_sub: Option<Subscription>,
    /// Workspace create / rename dialog (Cmd+Shift+N + sidebar rename).
    pub(crate) workspace_dialog: Entity<WorkspaceDialog>,
    /// Active type-to-confirm dialog (per-request; `None` when idle).
    pub(crate) confirm_dialog: Option<Entity<ConfirmDialog>>,
    /// Active rename-tab modal (per-request; `None` when idle).
    pub(crate) rename_tab_dialog: Option<Entity<crate::shell::rename_tab_dialog::RenameTabDialog>>,
    /// Currently active project — `None` until the user opens one.
    pub(crate) active_project: Option<Project>,
    /// Currently selected workspace id in the left rail — drives the
    /// active-row highlight. `None` until the user clicks a workspace.
    /// Window-local UI selection, not persisted.
    pub(crate) active_workspace_id: Option<String>,
    /// Browser-style back/forward history of workspace activations for this
    /// window (Cmd+Alt+←/→). Entries are `(project_id, workspace_id)` refs
    /// re-resolved on navigation so a deleted workspace fails gracefully.
    pub(crate) nav_history: Vec<WorkspaceNavRef>,
    /// Index into `nav_history` of the current position. Back decrements,
    /// forward increments; a fresh activation truncates everything after it.
    pub(crate) nav_cursor: usize,
    /// Set while a back/forward navigation is replaying so the resulting
    /// `activate_workspace` doesn't record a new history entry (which would
    /// corrupt the stack).
    pub(crate) nav_replaying: bool,
    /// Sidebar Rename/Archive/Delete popover (mounted at root for full-window backdrop).
    pub(crate) row_menu: Entity<WorkspaceRowMenu>,
    /// Sidebar per-project-header action popover (Reveal/Copy/Remove).
    /// Same full-window-backdrop mount contract as `row_menu`.
    pub(crate) project_menu: Entity<ProjectRowMenu>,
    /// Agents-page status-filter dropdown. Same full-window-backdrop mount
    /// contract as `row_menu`; applies its pick to `left_rail`.
    pub(crate) dashboard_status_menu: Entity<DashboardStatusFilterMenu>,
    /// Projects-header display-options dropdown (group-by / sort / card layout
    /// / collapse-all). Same backdrop mount contract; applies to `left_rail`.
    pub(crate) options_menu: Entity<WorkspaceOptionsMenu>,
    pub(crate) add_project_dialog: Entity<AddProjectDialog>,
    /// Render root tracks this so action dispatch reaches the workspace
    /// even when no pane is focused (sidebar toggle, command palette).
    pub(crate) focus_handle: FocusHandle,
    /// Per-window persistence key. Scopes pane scrollback, relay PTY ids,
    /// and tab layout blobs so two windows on the same project don't clobber
    /// each other. The first window uses "main" (matching the V005 migration
    /// default for legacy single-window rows); later windows use "w{n}".
    pub(crate) window_id: String,
    /// Long-lived subscription on `GitPanel::DiscardRequested`. Survives
    /// the entire workspace lifetime; dropping it would mean the discard
    /// modal silently stops appearing.
    pub(crate) _discard_subscription: Option<Subscription>,
    /// Per-mount observer on the active discard `ConfirmDialog`. Reset
    /// each time a new dialog is mounted (the previous observer is
    /// dropped along with its dialog). Watches `is_confirmed` /
    /// `is_cancelled` to free the `confirm_dialog` slot. Any other
    /// consumer of the shared `confirm_dialog` slot (e.g.
    /// `workspace_ops::delete_workspace_with_confirm`) MUST clear this
    /// field before reusing the slot, otherwise the stale observer
    /// will race the new dialog's teardown.
    pub(crate) _discard_dialog_observer: Option<Subscription>,
    /// Long-lived subscription on `StashPanel::PushStashRequested`. Same
    /// lifetime contract as `_discard_subscription` — dropping it
    /// silently disables the `+` push affordance in the stash section.
    pub(crate) _push_stash_subscription: Option<Subscription>,
    /// Active push-stash form modal (per-request; `None` when idle).
    /// Wired alongside `confirm_dialog` but kept in its own slot so
    /// the type-to-confirm flow stays separable from this creation
    /// form.
    pub(crate) push_stash_dialog: Option<Entity<PushStashDialog>>,
    /// Per-mount observer on the active `PushStashDialog`. Same lifecycle
    /// pattern as `_discard_dialog_observer` — reset each time a new
    /// dialog is mounted so the previous observer is dropped along with
    /// the previous dialog entity.
    pub(crate) _push_stash_dialog_observer: Option<Subscription>,
    /// Long-lived subscription on `CommitGraph::ShowCommitRequested`.
    /// Fired when the user clicks a commit row in the graph; the
    /// handler opens a commit-detail tab in the active pane group.
    /// Same lifetime contract as `_discard_subscription` —  dropping
    /// it silently disables the click-to-open affordance.
    pub(crate) _show_commit_subscription: Option<Subscription>,
    /// Long-lived subscription on `BranchCommitsPanel::ShowBranchFileRequested`.
    /// Fired when the user clicks a row in the "Committed on Branch"
    /// section; the handler opens a read-only range-diff tab in the active
    /// pane group. Same lifetime contract as `_show_commit_subscription`.
    pub(crate) _show_branch_file_subscription: Option<Subscription>,
    /// Long-lived subscription on `GitPanel::ShowCombinedDiffRequested`.
    /// Fired by a section "View all" CTA; opens a combined multi-file diff
    /// tab. Same lifetime contract as `_show_commit_subscription`.
    pub(crate) _show_combined_diff_subscription: Option<Subscription>,
    /// Long-lived subscription on `BranchCommitsPanel::ShowBranchDiffAllRequested`.
    /// Fired by the branch section's "View all" CTA; opens a combined
    /// read-only range diff. Same lifetime contract as above.
    pub(crate) _show_branch_diff_all_subscription: Option<Subscription>,
    /// Cached per-worktree diff line counts (keyed by worktree path).
    /// Populated by a periodic, focus-gated, concurrent background refresh
    /// (`run_diff_refresh_round`) and read in `refresh_left_rail`. Never
    /// written inside `Render` — results flow in from background completions
    /// via weak-entity update + cx.notify().
    pub(crate) diff_counts: HashMap<String, DiffCounts>,
    /// Cached sidebar DB data: each project's workspace rows (incl. the
    /// synthesized primary). Refreshed event-driven via `mark_rail_dirty`
    /// (workspace CRUD, project switch, the periodic diff tick as a
    /// reconciliation net) — `refresh_left_rail` only READS this, so
    /// render never touches SQLite.
    pub(crate) rail_workspaces_by_project: HashMap<String, Vec<oximux_core::Workspace>>,
    /// Cached latest agent-session status per workspace id — same
    /// lifecycle as [`Self::rail_workspaces_by_project`].
    pub(crate) rail_latest_status: crate::shell::left_rail::LatestStatusMap,
    /// Adapter slug of the latest agent session per workspace id — same
    /// lifecycle as `rail_latest_status`. Gates the activity tail (only
    /// the primary CLI journals session logs).
    pub(crate) rail_latest_adapter: HashMap<String, String>,
    /// Latest agent-session activity time per workspace id (raw RFC-3339:
    /// `ended_at` if finished, else `started_at`) — same lifecycle as
    /// `rail_latest_status`. Drives the dashboard's in-tier recency sort.
    pub(crate) rail_last_active: HashMap<String, String>,
    /// Every agent session per workspace id (`agent_sessions` rows, newest
    /// first) — same gather lifecycle as `rail_latest_status`. `refresh_left_rail`
    /// merges this DB history with the live `live_agents` map into the rail's
    /// per-workspace agent list. The single-row caches above stay derived from
    /// the newest session, so existing collapsed-dot behavior is unchanged.
    pub(crate) rail_workspace_sessions: HashMap<String, Vec<oximux_core::AgentSession>>,
    /// Live tool-activity line per workspace id ("Bash: cargo test…"),
    /// refreshed by `_agent_activity_task` for Running primary-CLI
    /// sessions and pushed to the rail via `refresh_left_rail`.
    pub(crate) agent_activity: HashMap<String, String>,
    /// Live structured sideband detail per workspace key (`workspaces.id` or
    /// `primary:<project_id>`) — the tool the agent is currently invoking,
    /// fed event-driven from each session's status watch channel by the
    /// persistence watcher (`note_agent_sideband`). Holds an entry only while
    /// the agent is `Running`; takes precedence over `agent_activity` on the
    /// dashboard's Running rows and is pushed to the rail via `refresh_left_rail`.
    pub(crate) agent_sideband: HashMap<String, oximux_core::SidebandDetail>,
    /// Live agent sessions keyed by `agent_sessions.id` UUID, fed directly
    /// from each tab's status watch channel by the persistence watcher. Unlike
    /// `agent_sideband` (one collapsed entry per workspace key), this holds
    /// EVERY open agent so the rail can list multiple agents per workspace.
    /// Entries live only while the session is non-terminal (its tab is open).
    pub(crate) live_agents: crate::shell::session_live_store::LiveAgentMap,
    /// Latest usage-meter state. `None` only before the first sample lands
    /// (then it is always `Available` or `Unavailable`).
    pub(crate) usage_state: Option<oximux_agents::session_log::usage::UsageState>,
    /// Whether the in-window usage popover is open (non-macOS fallback render).
    pub(crate) usage_popover_open: bool,
    /// Whether the "What's New" popover (staged-update release notes, opened
    /// from the title-bar Update pill) is showing.
    pub(crate) whats_new_open: bool,
    /// The open usage-popover panel window, if any (macOS floats it above the
    /// inline webview). `None` when closed.
    #[cfg(target_os = "macos")]
    pub(crate) usage_popover_window:
        Option<gpui::WindowHandle<crate::shell::usage_popover::UsagePopover>>,
    /// Workspace id whose normal delete failed at the worktree-removal
    /// step. The next delete request for the SAME workspace offers the
    /// Force Delete variant (force-remove + always drop the DB row,
    /// reporting leftovers). Cleared on success or when a different
    /// workspace's delete is requested.
    pub(crate) force_delete_offer: Option<String>,
    /// Set when rail DB data may be stale; consumed by the gather task.
    pub(crate) rail_dirty: bool,
    /// Guards against overlapping rail gathers (the gather re-runs itself
    /// while `rail_dirty` keeps getting re-set).
    pub(crate) rail_refresh_inflight: bool,
    /// Cached per-workspace agent lists. `refresh_left_rail` runs every frame
    /// (WorkspaceRoot re-renders on every agent-output tick via the panes
    /// observer), and rebuilding 150+ rows each time is the streaming jank.
    /// Recomputed only when `rail_agents_dirty` flips — a session or live-agent
    /// change — not on raw terminal output. The cheap per-frame ambient rows are
    /// appended to a clone of this after the rebuild gate.
    pub(crate) rail_agents_cache: crate::shell::left_rail::WorkspaceAgentList,
    /// Marks `rail_agents_cache` stale (session/live-agent change).
    pub(crate) rail_agents_dirty: bool,
    /// `true` while the window is active. The periodic diff refresh only runs
    /// when focused so an inactive window does not churn `git` in the
    /// background (mirrors the SCM status poller's pause-on-blur behavior).
    pub(crate) diff_refresh_focused: bool,
    /// Guards against overlapping refresh rounds — a slow round must finish
    /// before the next tick starts one, so concurrent shellouts cannot pile up.
    pub(crate) diff_refresh_in_flight: bool,
    /// Owns the periodic refresh loop. Dropping it cancels the loop when the
    /// window/root entity goes away.
    _diff_refresh_task: Task<()>,
    /// Periodic layout + relay-id autosave — bounds mid-session crash
    /// loss to one `LAYOUT_AUTOSAVE_TICK`. Held for its lifetime only.
    _layout_autosave_task: Task<()>,
    /// Periodic agent-activity tail (Running sessions only, focus-gated,
    /// background IO). Dropping cancels the loop.
    _agent_activity_task: Task<()>,
    /// Periodic usage-meter sample (60 s, background IO). Dropping cancels.
    _usage_meter_task: Task<()>,
    /// The window's single ports panel. One per *window*, not per project:
    /// a listening port is a fact about the machine, and the panel groups by
    /// project itself. Every `RightSidebar` this root builds is handed this
    /// same entity.
    pub(crate) ports_panel: Entity<crate::shell::ports_panel::PortsPanel>,
    /// Guards against overlapping port scans — the socket read runs on the
    /// background executor, and a slow one must not have a second stacked
    /// behind it.
    pub(crate) port_scan_in_flight: bool,
    /// Periodic listening-ports scan (focus-gated). Dropping cancels.
    _port_scan_task: Task<()>,
}

impl WorkspaceRoot {
    /// Toggle the usage-meter popover from the status-bar chip.
    ///
    /// On macOS the popover is a separate `WindowKind::PopUp` panel window
    /// (`shell::usage_popover`): an inline-browser webview is a native view
    /// layered above the GPU canvas, so an in-window GPUI card would render
    /// *behind* a visible page, and hiding the whole webview to surface it
    /// blanks the page. A popup panel composites above everything, leaving the
    /// page visible. A second chip click closes the open panel; a short
    /// debounce keeps the same click that dismisses it (by resigning the
    /// panel's key status) from immediately reopening it. Off macOS there is no
    /// such layering, so the in-window GPUI popover is toggled directly.
    #[cfg(target_os = "macos")]
    pub(crate) fn toggle_usage_popover(
        owner: &WeakEntity<Self>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        // Already open → this chip click resigns the panel's key status, so its
        // own observer dismisses it. Just don't open a second one.
        if owner
            .update(cx, |this, _| this.usage_popover_window.is_some())
            .unwrap_or(false)
        {
            return;
        }
        // Swallow the same click that just dismissed the panel (resign-key →
        // close), so it doesn't immediately reopen.
        let since_close = oximux_agents::session_log::now_unix_ms()
            - crate::shell::usage_popover::LAST_CLOSED_MS
                .load(std::sync::atomic::Ordering::SeqCst);
        if since_close < crate::shell::usage_popover::REOPEN_DEBOUNCE_MS {
            return;
        }
        // Snapshot the data + styling, then open the panel.
        let Ok(Some((state, theme, density, typography))) = owner.update(cx, |this, _| {
            this.usage_state
                .clone()
                .map(|s| (s, this.theme, this.density, this.typography.clone()))
        }) else {
            return;
        };
        if let Some(handle) = crate::shell::usage_popover::open(
            state,
            theme,
            density,
            typography,
            owner.clone(),
            window,
            cx,
        ) {
            let _ = owner.update(cx, |this, _| this.usage_popover_window = Some(handle));
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn toggle_usage_popover(
        owner: &WeakEntity<Self>,
        _window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let _ = owner.update(cx, |this, cx| {
            this.usage_popover_open = !this.usage_popover_open;
            cx.notify();
        });
    }

    /// Called by the popup panel when it self-dismisses (resign-key / Escape)
    /// so the chip toggle sees it as closed and can reopen on the next click.
    #[cfg(target_os = "macos")]
    pub(crate) fn note_usage_popover_closed(&mut self) {
        self.usage_popover_window = None;
    }

    pub fn new(
        app_state: AppState,
        window_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Seeded from the user's appearance rather than the shipped defaults:
        // every render pulls the current tokens anyway, but starting wrong
        // would paint one frame in the wrong palette and size on launch.
        let appearance = oximux_settings::appearance::active(cx);
        let theme = Theme::for_appearance(appearance);
        let density = Density::for_appearance(appearance);
        let typography = Typography::for_appearance(appearance);

        // Construct the CLI agent runtime + adapter registry once per
        // workspace. The registry is built with every built-in adapter in dialog
        // order; each is also registered into the runtime so `start_session` can
        // resolve them by `AgentAdapter` enum. The runtime's set is *derived*
        // from the registry rather than listed again here — a second list drifts
        // silently, and the failure is invisible until launch: the row renders,
        // the click routes, and `start_session` dies with "no adapter registered"
        // in a log nobody is reading.
        // Detection (`registry.detect_available()`) is intentionally lazy —
        // the Cmd+Shift+A action handler calls it at spawn time. Step 10's
        // popover will switch to fire-on-startup once the UX needs the
        // installed-list rendered before user interaction.
        // Route agent PTYs through the relay daemon (when up) so they
        // survive an app restart and the tab can re-attach to the live CLI,
        // matching plain-terminal restore. Falls back to in-process PTYs
        // when no relay is installed.
        let cli_runtime = Arc::new(CliRuntime::with_shared_backend(
            crate::shell::terminal_view::shared_backend(),
        ));
        let adapter_registry = Arc::new(AdapterRegistry::with_builtin_adapters());
        for kind in adapter_registry.builtin_kinds() {
            if let Some(adapter) = adapter_registry.adapter_for(kind) {
                cli_runtime.register_adapter(kind, adapter);
            }
        }

        // macOS notification click bridge. The notifier (a Send + Sync
        // `Notifier` impl) gets the sender; the router task below drains
        // the receiver on the GPUI side. Non-mac builds plug in a no-op
        // notifier and the channel pair is unused.
        let (click_tx, mut click_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::notifier::ClickTarget>();
        // Live notification prefs (per-kind enable, sound, focus gate),
        // hydrated from the flat settings store. Shared with the notifier;
        // the settings pane flips the atomics at runtime.
        let agent_notify_settings = Arc::new(AgentNotifySettings::from_getter(|k| {
            app_state.settings_repo.get(k).ok().flatten()
        }));
        // Seed the process-global sleep-assertion holder from the persisted
        // pref so a disabled toggle survives a relaunch (the settings pane
        // keeps it in sync afterwards).
        crate::agent_awake::global().set_enabled(agent_notify_settings.agent_awake_enabled());
        #[cfg(target_os = "macos")]
        let notifier: Arc<dyn Notifier> = Arc::new(crate::notifier::mac::MacNotifier::new(
            click_tx,
            agent_notify_settings.clone(),
        ));
        #[cfg(not(target_os = "macos"))]
        let notifier: Arc<dyn Notifier> = {
            // Channel exists to keep type-shape consistent across cfg;
            // sender is dropped, receiver yields None immediately.
            drop(click_tx);
            Arc::new(crate::notifier::null::NullNotifier)
        };

        // ProjectPanes entities live in a per-project HashMap, lazily built on
        // the first `set_active_project` call. Boot renders the welcome view
        // until the project-restore path (or user open) supplies one.
        let project_panes_by_project: HashMap<String, Entity<ProjectPanes>> = HashMap::new();
        let right_sidebar_by_project: HashMap<String, Entity<RightSidebar>> = HashMap::new();
        let project_panes_observer: Option<Subscription> = None;
        // Shared weak self-handle: LeftRail + picker callbacks route through it.
        // Built before the right-sidebar so the Files-tab `OnOpenFile` callback
        // can capture it and route clicks back to `open_file_in_active_pane`.
        let weak_self: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        // No sidebar at construction. Every sidebar is built by
        // `set_active_project`'s async arm (which opens the project's repo off
        // the UI thread) — the same path project switches and tear-off windows
        // already take. Boot deliberately passes no repo: opening one means
        // spawning `git`, and on the packaged Windows build the process's
        // FIRST child spawn can block for seconds inside CreateProcess's
        // console-host setup — a cost that must never sit on the first-paint
        // path (see main.rs, "boot: repo open is post-paint").
        let right_sidebar: Option<Entity<RightSidebar>> = None;
        let left_rail = cx.new(|cx| LeftRail::new(weak_self.clone(), cx));
        // Load persisted rail layout (width + collapsed groups) so it
        // survives restart.
        left_rail.update(cx, |rail, _cx| {
            rail.init_layout(app_state.settings_repo.clone());
        });
        let palette = cx.new(|cx| PaletteModal::new(theme, density, typography.clone(), cx));
        let session_history =
            cx.new(|cx| SessionHistoryModal::new(theme, density, typography.clone(), cx));
        let pane_actions = cx.new(|_| PaneActionsMenu::new(theme, density, typography.clone()));
        let tab_context_menu = cx.new(|_| TabContextMenu::new(theme, density, typography.clone()));
        let file_tree_context_menu =
            cx.new(|_| FileTreeContextMenu::new(theme, density, typography.clone()));
        let git_row_context_menu =
            cx.new(|_| GitRowContextMenu::new(theme, density, typography.clone()));
        let commit_context_menu =
            cx.new(|_| CommitContextMenu::new(theme, density, typography.clone()));
        let terminal_context_menu =
            cx.new(|_| TerminalContextMenu::new(theme, density, typography.clone()));
        let on_select: OnSelect = Box::new(move |selection, window, cx| {
            let weak = weak_self.clone();
            let _ = weak.update(cx, |this, cx| match selection {
                AdapterSelection::NewTerminal => this.spawn_local_terminal_tab(window, cx),
                // Reuse the ⌘⇧B root handler so the menu entry and the
                // keybinding open the browser tab through one path.
                AdapterSelection::NewBrowserTab => {
                    window.dispatch_action(Box::new(NewBrowserTab), cx);
                }
                AdapterSelection::NewAgentDraft => {
                    // Unified draft chat: open an unbound chat rooted at the active
                    // project (same rooting as a spawned agent, so its status dot
                    // binds to the workspace). The agent + model are picked in the
                    // composer; the transport binds on the first message.
                    let cwd = this
                        .active_project_panes()
                        .map(|panes| panes.read(cx).cwd().clone())
                        .or_else(|| {
                            this.active_project
                                .as_ref()
                                .map(|p| std::path::PathBuf::from(&p.root_path))
                        })
                        .unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        });
                    if let Some(panes) = this.active_project_panes() {
                        panes.update(cx, |p, cx| {
                            p.open_agent_chat_tab_unbound_in_active_group(cwd, window, cx);
                        });
                    }
                }
                AdapterSelection::Adapter { kind, id } => {
                    // Root the agent at the active project (its panes' cwd),
                    // so the worktree the agent runs in matches a sidebar
                    // workspace row — that drives the live (green) status
                    // dot. The process cwd is irrelevant to the user's
                    // project and would leave every agent orphaned.
                    let cwd = this
                        .active_project_panes()
                        .map(|panes| panes.read(cx).cwd().clone())
                        .or_else(|| {
                            this.active_project
                                .as_ref()
                                .map(|p| std::path::PathBuf::from(&p.root_path))
                        })
                        .unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        });
                    // Route to the structured chat view when the resolved open
                    // mode for THIS agent is Chat and the adapter declares chat
                    // support (Claude stream-json, Codex app-server, or an ACP
                    // adapter/preset with a command). `open_mode_for` layers a
                    // per-agent override + a preset's Chat default over the global,
                    // so an ACP preset (Cursor/Amp) opens as chat even when the
                    // global default is Terminal. `chat_capable` is the whole gate —
                    // for ACP it already requires a command. Every terminal-only
                    // adapter (and Terminal mode) takes the classic path unchanged.
                    let launch = cx.try_global::<oximux_settings::AgentLaunchSettings>();
                    let open_chat = launch.map(|s| s.opens_as_chat(id)).unwrap_or(false);
                    if open_chat {
                        // `open_chat` is only true when `launch` is `Some`.
                        let backend = launch
                            .map(|s| chat_backend_for(s, id))
                            .unwrap_or_default();
                        if let Some(panes) = this.active_project_panes() {
                            panes.update(cx, |p, cx| {
                                p.open_agent_chat_tab_in_active_group(cwd, None, backend, None, window, cx);
                            });
                        }
                    } else {
                        // One-click launch: always the agent's default settings.
                        this.spawn_agent_tab(
                            kind,
                            id,
                            cwd,
                            None,
                            None,
                            None,
                            oximux_core::SessionResumption::None,
                            None,
                            window,
                            cx,
                        )
                    }
                }
                AdapterSelection::AcpPreset { id } => {
                    // A built-in ACP preset (Cursor/Amp) is chat-only: open it
                    // straight as a structured chat tab over the generic ACP
                    // backend, rooted at the active project's cwd (same rooting as
                    // a spawned agent, so its status dot binds to the workspace).
                    // Guard on `chat_capable` (belt-and-suspenders with the picker
                    // filter): a preset id the user overrode with a non-ACP entry
                    // would otherwise resolve to stream-json and misroute to Claude,
                    // so bail rather than launch the wrong agent.
                    let backend = match cx.try_global::<oximux_settings::AgentLaunchSettings>() {
                        Some(s) if s.chat_capable(id) => chat_backend_for(s, id),
                        _ => return,
                    };
                    let cwd = this
                        .active_project_panes()
                        .map(|panes| panes.read(cx).cwd().clone())
                        .or_else(|| {
                            this.active_project
                                .as_ref()
                                .map(|p| std::path::PathBuf::from(&p.root_path))
                        })
                        .unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        });
                    if let Some(panes) = this.active_project_panes() {
                        panes.update(cx, |p, cx| {
                            p.open_agent_chat_tab_in_active_group(cwd, None, backend, None, window, cx);
                        });
                    }
                }
            });
        });
        let adapter_picker = cx.new(|_| {
            AdapterPicker::new(
                theme,
                density,
                typography.clone(),
                adapter_registry.clone(),
                on_select,
            )
        });

        // Project picker: weak self-reference in the on_pick closure so
        // the picker can route the user's choice back to
        // `set_active_project` without holding a strong cycle.
        let weak_for_picker: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let on_pick: OnPick = Box::new(move |project, window, cx| {
            let weak = weak_for_picker.clone();
            let _ = weak.update(cx, |this, cx| {
                this.refresh_recent_projects();
                this.set_active_project(project, window, cx);
            });
        });
        let project_repo = app_state.project_repo.clone();
        let project_picker = cx.new(|cx| {
            ProjectPickerModal::new(
                theme,
                density,
                typography.clone(),
                project_repo,
                on_pick,
                cx,
            )
        });
        let settings_modal = cx.new(|cx| {
            SettingsModal::new(
                theme,
                density,
                typography.clone(),
                agent_notify_settings.clone(),
                app_state.settings_repo.clone(),
                notifier.clone(),
                app_state.schedule_store(),
                cx,
            )
        });
        // First-run onboarding wizard. Constructed closed; opened below when
        // the boot gate armed the one-shot mailbox (fresh install only), or
        // later via the "Show Welcome Wizard" palette action.
        let onboarding = cx.new(|cx| {
            crate::shell::onboarding::OnboardingWizard::new(
                theme,
                density,
                typography.clone(),
                app_state.settings_repo.clone(),
                adapter_registry.clone(),
                cx,
            )
        });
        if crate::shell::onboarding::take_pending() {
            // Deferred: `new()` grabs root focus further down (the chrome-action
            // fallback), which would clobber a focus taken here in the same
            // frame and leave the wizard keyboard-dead on the one path that
            // matters (fresh install). Defer runs after construction, so the
            // wizard's grab is the last one standing.
            let onboarding = onboarding.clone();
            window.defer(cx, move |window, cx| {
                onboarding.update(cx, |wizard, cx| wizard.open(window, cx));
            });
        }
        let toast_layer = cx.new(|_| ToastLayer::new(theme, density, typography.clone()));
        // Register this window's layer as the active toast surface up front so
        // toasts work before the first window-activation event arrives.
        crate::shell::toast::set_active_toast_layer(cx, toast_layer.downgrade());
        // This window's voice-dictation HUD (floating "Listening…" pill for
        // terminal/editor panes). Each window owns its own; the session's
        // `DictationTarget::Hud` carries this handle, so no global registration
        // is needed (and none would be correct with multiple windows).
        let dictation_hud = cx.new(|_| {
            crate::shell::agent_chat::DictationHud::new(theme, density, typography.clone())
        });
        // Keybinding-override problems found at boot predate any window;
        // surface them now that a toast layer exists (first window drains).
        for warning in crate::keybindings_settings::take_boot_warnings() {
            toast_layer.update(cx, |layer, cx| {
                layer.push(crate::shell::toast::ToastKind::Error, warning, cx);
            });
        }
        // When the settings modal closes (×, Esc, click-outside, or toggle),
        // return keyboard focus to the workspace root. The modal grabs focus
        // for its search field on open; without this the handle stays focused
        // after it hides and global shortcuts (Cmd+,) silently stop firing.
        let settings_modal_sub = cx.subscribe_in(
            &settings_modal,
            window,
            |root, _modal, _ev: &SettingsModalEvent, window, cx| {
                root.focus_handle.focus(window, cx);
            },
        );
        // Same contract for the onboarding wizard: it takes focus on open, so
        // its close (Finish or Skip) must hand focus back or global chords die.
        let onboarding_sub = cx.subscribe_in(
            &onboarding,
            window,
            |root, _wizard, _ev: &crate::shell::onboarding::OnboardingEvent, window, cx| {
                root.focus_handle.focus(window, cx);
            },
        );
        // The command palette and project picker grab focus on open too; restore
        // workspace focus when they close so global shortcuts keep dispatching.
        let palette_sub = cx.subscribe_in(
            &palette,
            window,
            |root, _palette, _ev: &PaletteEvent, window, cx| {
                root.focus_handle.focus(window, cx);
            },
        );
        let session_history_sub = cx.subscribe_in(
            &session_history,
            window,
            |root, _modal, _ev: &SessionHistoryEvent, window, cx| {
                root.focus_handle.focus(window, cx);
            },
        );
        let project_picker_sub = cx.subscribe_in(
            &project_picker,
            window,
            |root, _picker, _ev: &ProjectPickerEvent, window, cx| {
                root.focus_handle.focus(window, cx);
            },
        );

        // Workspace dialog: same weak-ref pattern. Submit payload carries
        // the mode (Create vs Rename) — `WorkspaceRoot` dispatches to
        // `create_workspace_async` or `rename_workspace_now`.
        let weak_for_workspace: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let on_workspace_submit: OnWorkspaceSubmit = Box::new(move |submit, window, cx| {
            let weak = weak_for_workspace.clone();
            // `update`, NOT `update_in`, and forward the OUTER window — same
            // contract as `on_pick`/`on_select` above. This callback runs
            // during mouse/action dispatch, where the window is already taken
            // out of `cx.windows`; `update_in`'s nested window lookup fails
            // there and returns an Err that this call site would discard,
            // silently dropping the submit.
            let _ = weak.update(cx, |this, cx| {
                this.dispatch_workspace_submit(submit, window, cx);
            });
        });
        let workspace_dialog = cx.new(|cx| {
            WorkspaceDialog::new(
                theme,
                density,
                typography.clone(),
                on_workspace_submit,
                window,
                cx,
            )
        });
        let weak_for_menu: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let row_menu =
            cx.new(|_| WorkspaceRowMenu::new(theme, density, typography.clone(), weak_for_menu));
        let weak_for_project_menu: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let project_menu = cx.new(|_| {
            ProjectRowMenu::new(theme, density, typography.clone(), weak_for_project_menu)
        });
        let weak_left_rail = left_rail.downgrade();
        let dashboard_status_menu = cx.new(|_| {
            DashboardStatusFilterMenu::new(theme, density, typography.clone(), weak_left_rail)
        });
        let weak_left_rail_for_options = left_rail.downgrade();
        let options_menu = cx.new(|_| {
            WorkspaceOptionsMenu::new(theme, density, typography.clone(), weak_left_rail_for_options)
        });
        let pr = app_state.project_repo.clone();
        let add_project_dialog =
            build_add_project_dialog(theme, density, typography.clone(), pr, cx);

        // Pause status polling when the window blurs; force an immediate
        // refresh on focus regain via StatusPoller::kick(). The rail's
        // diff-count refresh follows the same focus gating: paused while
        // inactive, kicked once immediately on focus regain so chips are
        // fresh the moment the user returns.
        let window_activation_observer =
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |sidebar, _cx| sidebar.set_polling_focused(active));
                }
                this.diff_refresh_focused = active;
                if active {
                    // Route toasts to whichever window the user is looking at.
                    crate::shell::toast::set_active_toast_layer(
                        cx,
                        this.toast_layer.downgrade(),
                    );
                    this.run_diff_refresh_round(cx);
                    // Same reason the diff round is kicked here: the ports
                    // scan pauses while the window is behind another, so a
                    // server started in the meantime would otherwise be
                    // missing from the panel for up to one tick after the
                    // user has already looked at it.
                    this.run_port_scan(cx);
                    // The user is back — clear the accumulated attention dock badge.
                    this.notifier.clear_attention();
                }
            });

        // Periodic diff-count refresh loop. Ticks every `DIFF_REFRESH_TICK`
        // and, while the window is focused, kicks a concurrent per-worktree
        // refresh round (self-guarded against overlap). Breaks when the root
        // entity is gone.
        let diff_refresh_task = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(DIFF_REFRESH_TICK).await;
                let still_alive = weak
                    .update(cx, |this, cx| {
                        if this.diff_refresh_focused {
                            this.run_diff_refresh_round(cx);
                        }
                    })
                    .is_ok();
                if !still_alive {
                    break;
                }
            }
        });

        // Periodic agent-activity tail: for Running primary-CLI sessions,
        // tail the newest session log for the current tool call and push
        // the label map to the dashboard rows. Focus-gated like the diff
        // refresh; all file IO on the background executor.
        let agent_activity_task = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(AGENT_ACTIVITY_TICK).await;
                // Snapshot targets on the main thread: Running workspaces
                // whose latest session belongs to the primary CLI.
                let Ok(targets) = weak.update(cx, |this, _| {
                    if !this.diff_refresh_focused {
                        return Vec::new();
                    }
                    let mut targets: Vec<(String, String)> = Vec::new();
                    for (ws_id, status) in &this.rail_latest_status {
                        if !matches!(status, Some(oximux_core::AgentStatus::Running)) {
                            continue;
                        }
                        if this.rail_latest_adapter.get(ws_id).map(String::as_str)
                            != Some("claude-code")
                        {
                            continue;
                        }
                        let path = this
                            .rail_workspaces_by_project
                            .values()
                            .flatten()
                            .find(|w| &w.id == ws_id)
                            .map(|w| w.worktree_path.clone());
                        if let Some(path) = path {
                            targets.push((ws_id.clone(), path));
                        }
                    }
                    targets
                }) else {
                    break;
                };
                let new_map = cx
                    .background_executor()
                    .spawn(async move { gather_agent_activity(targets) })
                    .await;
                let alive = weak
                    .update(cx, |this, cx| {
                        if this.agent_activity != new_map {
                            this.agent_activity = new_map;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });

        // The window's ports panel, built once and shared by every sidebar
        // this root goes on to build.
        let weak_for_ports: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let ports_panel = cx.new(|cx| {
            crate::shell::ports_panel::PortsPanel::new(
                weak_for_ports,
                Some(app_state.settings_repo.clone()),
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });

        // Periodic listening-ports scan. Focus-gated like the diff refresh:
        // nobody is reading the panel while the window is behind another, and
        // the activation observer kicks a scan the moment they are.
        let port_scan_task = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(PORT_SCAN_TICK).await;
                let still_alive = weak
                    .update(cx, |this, cx| {
                        if this.diff_refresh_focused {
                            this.run_port_scan(cx);
                        }
                    })
                    .is_ok();
                if !still_alive {
                    break;
                }
            }
        });

        // Periodic usage-meter sample. Sample immediately (so the meter is
        // present right after boot), then every `USAGE_METER_TICK`. Each
        // sample is a blocking Keychain + network read, so it runs on the
        // background executor.
        use oximux_agents::session_log::usage_probe::UsageProbe as _;
        let usage_probe: std::sync::Arc<
            oximux_agents::session_log::usage_probe::SessionLogUsageProbe,
        > = std::sync::Arc::new(
            oximux_agents::session_log::usage_probe::SessionLogUsageProbe::new(
                dirs::home_dir().unwrap_or_default(),
            ),
        );
        let usage_meter_task = cx.spawn(async move |weak, cx| {
            loop {
                let probe = usage_probe.clone();
                let state = cx
                    .background_executor()
                    .spawn(async move { probe.sample() })
                    .await;
                let alive = weak
                    .update(cx, |this, cx| {
                        if this.usage_state.as_ref() != Some(&state) {
                            this.usage_state = Some(state);
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(USAGE_METER_TICK).await;
            }
        });

        // Periodic layout + relay-id autosave. Quit/switch captures miss
        // anything created mid-session — a tab or split made after the
        // last capture is gone entirely if the app crashes. This bounds
        // that loss to one `LAYOUT_AUTOSAVE_TICK`. Skipped during quit so
        // it can't race the quit-save over a half-torn-down tree.
        let layout_autosave_task = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(LAYOUT_AUTOSAVE_TICK).await;
                if crate::shell::terminal_view::APP_QUITTING
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    break;
                }
                let still_alive = weak
                    .update(cx, |this, cx| {
                        this.capture_all_layouts(cx);
                        if let Some(session_id) =
                            crate::shell::terminal_view::relay_session_id_cached()
                        {
                            this.capture_all_pane_relay_ids_with_session(&session_id, cx);
                        }
                    })
                    .is_ok();
                if !still_alive {
                    break;
                }
            }
        });

        // Click router: drains tab-ids posted by the macOS click watcher.
        // For each id, navigate to the owning project + workspace + tab
        // (cross-project included). Raises the window only when the tab
        // still exists — popping the window with no destination on a
        // stale click (agent closed since the banner fired) would be
        // disruptive UX. Closure ends when the mpsc receiver returns None
        // (all senders dropped, e.g. at app shutdown) or when the entity
        // is gone.
        let click_router = cx.spawn_in(window, async move |weak, cx| {
            while let Some(target) = click_rx.recv().await {
                if weak
                    .update_in(cx, |root, window, cx| match target {
                        crate::notifier::ClickTarget::AgentTab(tab_id) => {
                            root.navigate_to_agent_tab(tab_id, window, cx);
                        }
                        crate::notifier::ClickTarget::TerminalSession(raw) => {
                            root.navigate_to_terminal_session(
                                oximux_pty::TerminalSessionId(raw),
                                window,
                                cx,
                            );
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        // Focus the workspace-root handle immediately so the root div's
        // `on_action(...)` listeners (ToggleRightSidebar, ToggleLeftSidebar,
        // and the rest of the chrome) have a valid dispatch path from the
        // first frame. Without this, actions dispatched by mouse clicks on
        // toolbar buttons (which themselves carry no focus) before the
        // user has clicked into any pane silently drop on the floor —
        // matches the "can't expand right sidebar until I click a tab"
        // bug. `defer_focus_active` later moves focus to the active tab;
        // until that fires, this fallback keeps actions routable.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        // SCM-panel event subscriptions (discard, push-stash, commit-click,
        // branch-file, combined "View all", branch-diff-all) are wired by
        // `rewire_scm_subscriptions` below — and re-wired after every
        // `set_active_project` sidebar rebuild, since that mints fresh panel
        // entities the original subscriptions would otherwise orphan.

        let mut this = Self {
            theme,
            density,
            typography,
            project_panes_by_project,
            right_sidebar_by_project,
            notifier: notifier.clone(),
            right_sidebar,
            left_rail,
            palette,
            session_history,
            pane_actions,
            tab_context_menu,
            file_tree_context_menu,
            git_row_context_menu,
            commit_context_menu,
            terminal_context_menu,
            adapter_picker,
            cli_runtime,
            adapter_registry,
            left_rail_open: true,
            _project_panes_observer: project_panes_observer,
            _window_activation_observer: window_activation_observer,
            _click_router: click_router,
            // SCM subscriptions are wired by `rewire_scm_subscriptions` just
            // below, then re-wired after each sidebar rebuild.
            _discard_subscription: None,
            _discard_dialog_observer: None,
            _push_stash_subscription: None,
            _show_branch_file_subscription: None,
            _show_combined_diff_subscription: None,
            _show_branch_diff_all_subscription: None,
            push_stash_dialog: None,
            _push_stash_dialog_observer: None,
            _show_commit_subscription: None,
            floating_terminal: None,
            floating_terminal_visible: false,
            _floating_terminal_sub: None,
            _settings_modal_sub: Some(settings_modal_sub),
            _onboarding_sub: Some(onboarding_sub),
            _palette_sub: Some(palette_sub),
            _session_history_sub: Some(session_history_sub),
            _project_picker_sub: Some(project_picker_sub),
            app_state,
            project_picker,
            settings_modal,
            onboarding,
            toast_layer,
            dictation_hud,
            workspace_dialog,
            confirm_dialog: None,
            rename_tab_dialog: None,
            active_project: None,
            active_workspace_id: None,
            nav_history: Vec::new(),
            nav_cursor: 0,
            nav_replaying: false,
            row_menu,
            project_menu,
            dashboard_status_menu,
            options_menu,
            add_project_dialog,
            focus_handle,
            window_id,
            diff_counts: HashMap::new(),
            force_delete_offer: None,
            rail_workspaces_by_project: HashMap::new(),
            rail_latest_status: HashMap::new(),
            rail_latest_adapter: HashMap::new(),
            rail_last_active: HashMap::new(),
            rail_workspace_sessions: HashMap::new(),
            agent_activity: HashMap::new(),
            agent_sideband: HashMap::new(),
            live_agents: HashMap::new(),
            usage_state: None,
            usage_popover_open: false,
            whats_new_open: false,
            #[cfg(target_os = "macos")]
            usage_popover_window: None,
            rail_dirty: false,
            rail_refresh_inflight: false,
            rail_agents_cache: HashMap::new(),
            rail_agents_dirty: true,
            diff_refresh_focused: true,
            diff_refresh_in_flight: false,
            _diff_refresh_task: diff_refresh_task,
            _layout_autosave_task: layout_autosave_task,
            _agent_activity_task: agent_activity_task,
            _usage_meter_task: usage_meter_task,
            ports_panel,
            port_scan_in_flight: false,
            _port_scan_task: port_scan_task,
        };
        // Seed the sidebar's DB-backed caches (workspace rows + agent
        // statuses) — the first gather lands async, typically before the
        // first meaningful paint.
        this.mark_rail_dirty(cx);
        this.rewire_scm_subscriptions(window, cx);
        // Load global custom commands on startup. No active project yet so
        // only the global `commands.toml` is checked; project commands are
        // loaded (and re-merged) on the first `set_active_project` call.
        this.reload_custom_commands(cx);
        this
    }
}

// `build_project_panes` + restore helpers live in
// `crate::project_panes_factory` so this file stays under the 800-LOC cap.
// `impl Focusable for WorkspaceRoot` lives in `shell::workspace_ops` for
// the same reason.


/// Whether a terminal tab is eligible for cross-window tear-off.
///
/// Tear-off moves a single relay-backed daemon PTY to a new window. It is
/// blocked when the tab is anything other than one relay-backed terminal:
///   - multi-leaf split (`live_count != 1`) — each leaf would need an
///     independent detach + remount in the destination (out of scope here),
///   - multi-tab leaf (`active_leaf().len() != 1`) — the collect path follows
///     only each leaf's ACTIVE view, so moving a multi-tab leaf would orphan
///     the background tabs' PTYs in the daemon, and
///   - no relay session (`active_has_external_id == false`) — the in-process
///     fallback backend has no external id to reattach in the new window.
///
/// `active_has_external_id` is read from the active view by the caller (which
/// holds the `cx`). Split out so the eligibility contract is unit-testable
/// without driving the full context-menu open path.
pub(crate) fn tab_can_tear_off(
    tree: &crate::shell::pane_group::sub_pane::TerminalSplitTree,
    active_has_external_id: bool,
) -> bool {
    tree.live_count() == 1
        && tree.active_leaf().map(|l| l.len()).unwrap_or(0) == 1
        && active_has_external_id
}

/// One activity-tail round: for each `(workspace_id, worktree_path)`
/// target, find the newest primary-CLI session log for that cwd and pull
/// the current tool call out of its tail. Blocking file IO — background
/// executor only. A target without a fresh log simply contributes no
/// entry, so finished/stale rows clear naturally.
fn gather_agent_activity(targets: Vec<(String, String)>) -> HashMap<String, String> {
    use oximux_agents::session_log::{self, activity};

    let mut out = HashMap::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let claude_dir = home.join(".claude");
    let now_ms = session_log::now_unix_ms();
    for (workspace_id, worktree_path) in targets {
        let dir = session_log::project_log_dir(&claude_dir, std::path::Path::new(&worktree_path));
        let Some((log, mtime_ms)) = session_log::newest_session_log(&dir) else {
            continue;
        };
        // A log idle past the freshness window can't hold current activity;
        // skip the read entirely.
        if now_ms.saturating_sub(mtime_ms) > activity::FRESH_WITHIN_MS {
            continue;
        }
        if let Some(act) = activity::read_current_activity(&log, now_ms) {
            out.insert(workspace_id, act.label());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::chat_backend_for;
    use oximux_agents::thread::Transport as AgentTransport;
    use oximux_settings::{AgentLaunchSettings, Transport as SettingsTransport};

    #[test]
    fn chat_backend_for_carries_acp_command_only_for_acp_adapters() {
        let mut s = AgentLaunchSettings::default();
        // A user-configured ACP adapter: transport + command + args flow through.
        {
            let e = s.entry_mut("gemini");
            e.transport = SettingsTransport::Acp;
            e.acp_command = "gemini".into();
            e.acp_args = "--experimental-acp".into();
        }
        let acp = chat_backend_for(&s, "gemini");
        assert_eq!(acp.transport, AgentTransport::Acp);
        assert_eq!(acp.acp_command.as_deref(), Some("gemini"));
        assert_eq!(acp.acp_args, vec!["--experimental-acp".to_string()]);

        // Built-in Codex (app-server) and Claude (stream-json) carry no acp_*.
        let codex = chat_backend_for(&s, "codex");
        assert_eq!(codex.transport, AgentTransport::AppServer);
        assert_eq!(codex.acp_command, None);
        assert!(codex.acp_args.is_empty());

        let claude = chat_backend_for(&s, "claude-code");
        assert_eq!(claude.transport, AgentTransport::StreamJson);
        assert_eq!(claude.acp_command, None);
        assert!(claude.acp_args.is_empty());
    }

    #[test]
    fn chat_backend_for_resolves_a_builtin_preset_without_config() {
        // A Cursor preset launch resolves its ACP command/args with zero TOML —
        // the one-click AcpPreset routing path relies on this.
        let s = AgentLaunchSettings::default();
        let cursor = chat_backend_for(&s, "cursor");
        assert_eq!(cursor.transport, AgentTransport::Acp);
        assert_eq!(cursor.acp_command.as_deref(), Some("cursor-agent"));
        assert_eq!(cursor.acp_args, vec!["acp".to_string()]);
    }
}

mod ops;
mod render;
