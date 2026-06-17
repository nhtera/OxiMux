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
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};

/// macOS Application Support sub-directory anchor. Must mirror the same
/// constant in `crates/app/src/main.rs` — kept duplicated rather than
/// re-exported because `main.rs` is a binary and `workspace_root.rs` is
/// the library, and the two share no module today.
pub(crate) const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";

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

/// Cadence of the usage-meter sample. The probe re-parses only logs whose
/// (mtime, len) changed since the previous sample, so a steady-state tick
/// costs one directory scan + at most one active-log re-parse.
const USAGE_METER_TICK: Duration = Duration::from_secs(60);

use crate::notifier::{AgentNotifySettings, Notifier};
use crate::state::AppState;

use crate::actions::{
    ActivateGroupTab, ActivateWorkspaceFromJump, ApplyLayoutBottomTerminal, ApplyLayoutHorizontal,
    ApplyLayoutStacked, CloseGroup, CloseTab, DismissOverlay, MoveTabToNewWindow,
    OpenAddProjectDialog,
    OpenCommandPalette, OpenCommitContextMenuAt, OpenCommitDialog, OpenFileFromContextMenu,
    OpenFileTreeContextMenuAt, OpenGitRowContextMenuAt, OpenPaneActions, OpenPaneActionsAt,
    NewBrowserTab, NewTab, OpenProjectPicker, OpenQuickOpen, OpenSettings, OpenTabContextMenuAt,
    OpenWorkspaceCreate, OpenWorkspaceJump, RequestOpenAdapterPicker, Search, SelectExplorerTab,
    SelectFilesTab,
    SelectSearchTab,
    SelectSourceControlTab, SendTextToActiveAgent, SplitDown, SplitGroupAt, SplitHorizontal,
    SplitLeft, SplitRight, SplitUp, SplitVertical, ToggleFloatingTerminal, ToggleLeftSidebar,
    ToggleRightSidebar,
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
        project_menu::ProjectRowMenu,
        row_menu::WorkspaceRowMenu,
        workspace_row::{DiffCounts, sum_numstat},
    },
    main_area,
    openable_text_file::is_openable_text_file,
    pane_actions::{PaneActionsAnchor, PaneActionsMenu},
    project_panes::ProjectPanes,
    project_picker::{OnPick, ProjectPickerEvent, ProjectPickerModal},
    settings_modal::{SettingsModal, SettingsModalEvent},
    right_sidebar::{
        RightSidebar, activity_bar::render_tab_buttons, layout::DEFAULT_PANEL_WIDTH, tab::RightTab,
    },
    status_bar,
    tab_context_menu::TabContextMenu,
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
    /// Quiet transient toast stack (bottom-right). Surfaces fleeting
    /// cross-surface events (agent done, commit failed, PR opened, clipboard)
    /// that the status bar's persistent state doesn't cover.
    pub(crate) toast_layer: Entity<ToastLayer>,
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
    /// Same focus-restore guard for the command palette and project picker
    /// (both grab focus on open).
    pub(crate) _palette_sub: Option<Subscription>,
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
    /// Live tool-activity line per workspace id ("Bash: cargo test…"),
    /// refreshed by `_agent_activity_task` for Running primary-CLI
    /// sessions and pushed to the rail via `refresh_left_rail`.
    pub(crate) agent_activity: HashMap<String, String>,
    /// Latest usage-meter state. `None` only before the first sample lands
    /// (then it is always `Available` or `Unavailable`).
    pub(crate) usage_state: Option<oximux_agents::session_log::usage::UsageState>,
    /// Whether the in-window usage popover is open (non-macOS fallback render).
    pub(crate) usage_popover_open: bool,
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
        repo: Option<Repository>,
        app_state: AppState,
        window_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = Theme::charcoal();
        let density = Density::cockpit();
        let typography = Typography::cockpit();

        // Construct the CLI agent runtime + adapter registry once per
        // workspace. The registry is built with all four built-in adapters
        // in dialog order; each adapter is also registered into the runtime
        // so `start_session` can resolve them by `AgentAdapter` enum.
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
        for kind in [
            AgentAdapter::ClaudeCode,
            AgentAdapter::Codex,
            AgentAdapter::Aider,
            AgentAdapter::Custom,
        ] {
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
        let right_sidebar = repo.clone().map(|r| {
            let root_path = r.workdir().to_path_buf();
            let on_open = Self::build_on_open_file_callback(weak_self.clone());
            let on_open_diff = Self::build_on_open_diff_callback(weak_self.clone(), r.clone());
            let on_query = Self::build_on_query_active_path_callback(weak_self.clone());
            let worktree_settings_repo = Some(app_state.worktree_settings_repo.clone());
            // Phase 13: load persisted panel width clamped against the
            // live window so a too-large value from a wider monitor
            // can't overflow a smaller one on next boot.
            let window_width = f32::from(window.bounds().size.width);
            let initial_width = px(crate::scm_layout_settings::load_panel_width(
                &app_state.settings_repo,
                window_width,
            ));
            let layout_boot = crate::shell::right_sidebar::SidebarLayoutBoot {
                initial_width: Some(initial_width),
                settings_repo: Some(app_state.settings_repo.clone()),
            };
            cx.new(|cx| {
                RightSidebar::new(
                    Some(r),
                    root_path,
                    false, // default-collapsed on app boot
                    Some(on_open),
                    Some(on_open_diff),
                    Some(on_query),
                    worktree_settings_repo,
                    layout_boot,
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
            })
        });
        let left_rail = cx.new(|cx| LeftRail::new(weak_self.clone(), cx));
        // Load persisted rail layout (width + collapsed groups) so it
        // survives restart.
        left_rail.update(cx, |rail, _cx| {
            rail.init_layout(app_state.settings_repo.clone());
        });
        let palette = cx.new(|cx| PaletteModal::new(theme, density, typography.clone(), cx));
        let pane_actions = cx.new(|_| PaneActionsMenu::new(theme, density, typography.clone()));
        let tab_context_menu = cx.new(|_| TabContextMenu::new(theme, density, typography.clone()));
        let file_tree_context_menu =
            cx.new(|_| FileTreeContextMenu::new(theme, density, typography.clone()));
        let git_row_context_menu =
            cx.new(|_| GitRowContextMenu::new(theme, density, typography.clone()));
        let commit_context_menu =
            cx.new(|_| CommitContextMenu::new(theme, density, typography.clone()));
        let on_select: OnSelect = Box::new(move |selection, window, cx| {
            let weak = weak_self.clone();
            let _ = weak.update(cx, |this, cx| match selection {
                AdapterSelection::NewTerminal => this.spawn_local_terminal_tab(window, cx),
                // Reuse the ⌘⇧B root handler so the menu entry and the
                // keybinding open the browser tab through one path.
                AdapterSelection::NewBrowserTab => {
                    window.dispatch_action(Box::new(NewBrowserTab), cx);
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
                    // One-click launch: always the agent's default settings.
                    this.spawn_agent_tab(kind, id, cwd, None, None, window, cx)
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
                cx,
            )
        });
        let toast_layer = cx.new(|_| ToastLayer::new(theme, density, typography.clone()));
        // Register this window's layer as the active toast surface up front so
        // toasts work before the first window-activation event arrives.
        crate::shell::toast::set_active_toast_layer(cx, toast_layer.downgrade());
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
        // The command palette and project picker grab focus on open too; restore
        // workspace focus when they close so global shortcuts keep dispatching.
        let palette_sub = cx.subscribe_in(
            &palette,
            window,
            |root, _palette, _ev: &PaletteEvent, window, cx| {
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
            let _ = weak.update_in(cx, |this, window, cx| {
                this.dispatch_workspace_submit(submit, window, cx);
            });
            let _ = window; // referenced only inside the update_in callback
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
            pane_actions,
            tab_context_menu,
            file_tree_context_menu,
            git_row_context_menu,
            commit_context_menu,
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
            _palette_sub: Some(palette_sub),
            _project_picker_sub: Some(project_picker_sub),
            app_state,
            project_picker,
            settings_modal,
            toast_layer,
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
            add_project_dialog,
            focus_handle,
            window_id,
            diff_counts: HashMap::new(),
            force_delete_offer: None,
            rail_workspaces_by_project: HashMap::new(),
            rail_latest_status: HashMap::new(),
            rail_latest_adapter: HashMap::new(),
            agent_activity: HashMap::new(),
            usage_state: None,
            usage_popover_open: false,
            #[cfg(target_os = "macos")]
            usage_popover_window: None,
            rail_dirty: false,
            rail_refresh_inflight: false,
            diff_refresh_focused: true,
            diff_refresh_in_flight: false,
            _diff_refresh_task: diff_refresh_task,
            _layout_autosave_task: layout_autosave_task,
            _agent_activity_task: agent_activity_task,
            _usage_meter_task: usage_meter_task,
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

    /// Collect every worktree path across all recent projects (the project
    /// root plus each linked worktree). Same source the rail snapshot uses;
    /// deduped so a project root that already has a workspace row is counted
    /// once. Synchronous SQLite reads only — no git, safe to call per round.
    fn all_worktree_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for project in &self.app_state.recent_projects {
            paths.push(project.root_path.clone());
            if let Ok(list) = self.app_state.workspace_repo.list_for_project(&project.id) {
                for w in list {
                    if w.worktree_path != project.root_path {
                        paths.push(w.worktree_path);
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    /// Run one concurrent diff-count refresh round. Self-guards against
    /// overlap via `diff_refresh_in_flight`. Fans out all per-worktree
    /// `git diff --numstat` shellouts concurrently (serial per-worktree
    /// shellouts previously froze the rail), then writes the results back on
    /// the main thread and evicts paths that no longer exist so the cache
    /// cannot grow without bound.
    ///
    /// The numstat shellout spawns a child via `tokio::process`, which needs a
    /// live Tokio reactor. GPUI's background executor has none, so the fan-out
    /// runs on the app's Tokio runtime (entered on the main thread for the life
    /// of the app) and results are ferried back over a oneshot to a GPUI task.
    /// Called only from main-thread GPUI callbacks, where `Handle::try_current`
    /// resolves to that runtime.
    pub(crate) fn run_diff_refresh_round(&mut self, cx: &mut Context<Self>) {
        // Reconciliation net for the sidebar's DB caches: any workspace /
        // agent-session write that missed an explicit `mark_rail_dirty`
        // call is picked up within one focus-gated tick.
        self.mark_rail_dirty(cx);
        if self.diff_refresh_in_flight {
            return;
        }
        let paths = self.all_worktree_paths();
        if paths.is_empty() {
            return;
        }
        // Bail (leaving the flag clear) when no runtime is entered so a
        // headless/test context degrades to "no live counts" instead of
        // panicking inside the child-process spawn.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::workspace_root",
                    "no tokio runtime; worktree diff counts stay stale this round"
                );
                return;
            }
        };
        self.diff_refresh_in_flight = true;
        let (tx, rx) =
            tokio::sync::oneshot::channel::<Vec<(String, Option<DiffCounts>)>>();
        handle.spawn(async move {
            let futs = paths.into_iter().map(|path| async move {
                let counts = oximux_git::diff_numstat_head(std::path::Path::new(&path))
                    .await
                    .ok()
                    .map(|map| sum_numstat(&map));
                (path, counts)
            });
            let _ = tx.send(futures::future::join_all(futs).await);
        });
        cx.spawn(async move |weak, cx| {
            let Ok(results) = rx.await else {
                // Sender dropped (runtime torn down) — clear the flag so a
                // later round can retry rather than wedging in-flight.
                let _ = weak.update(cx, |this, _| {
                    this.diff_refresh_in_flight = false;
                });
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                // Snapshot of paths seen this round drives eviction so removed
                // worktrees age out of the cache.
                let current: std::collections::HashSet<String> =
                    results.iter().map(|(p, _)| p.clone()).collect();
                for (path, counts) in results {
                    // A failed fetch leaves the prior value intact rather than
                    // blanking the chip on a transient git error.
                    if let Some(counts) = counts {
                        this.diff_counts.insert(path, counts);
                    }
                }
                this.diff_counts.retain(|k, _| current.contains(k));
                this.diff_refresh_in_flight = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Load global + active-project custom commands and push them into the
    /// command palette. Safe to call with no active project (loads global
    /// only, project file simply won't exist). Called on startup and
    /// whenever `ReloadCustomCommands` fires.
    pub(crate) fn reload_custom_commands(&self, cx: &mut Context<Self>) {
        // `load_for_project` gracefully no-ops a missing project-level
        // `.oximux/commands.toml`, so passing a non-existent root is fine.
        let project_root = self
            .active_project
            .as_ref()
            .map(|p| std::path::PathBuf::from(&p.root_path))
            .unwrap_or_else(|| std::path::PathBuf::from("/dev/null"));
        let commands = crate::custom_commands_loader::load_for_project(&project_root);
        self.palette
            .update(cx, |p, cx| p.set_custom_commands(commands, cx));
    }

    /// Open a fresh local-PTY tab in the active project's active pane group.
    fn spawn_local_terminal_tab(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| p.open_terminal_tab_in_active_group(window, cx));
    }

    // `toggle_floating_terminal` and the rest of the floating-terminal host
    // logic (restore, new-tab spawn, expand-to-pane, rename) live in
    // `crate::shell::floating_terminal_host` — same split-impl pattern as
    // `workspace_ops`.


    /// Resolves the currently-visible `ProjectPanes` entity by reading
    /// `active_project.id` against the per-project map. `None` when no
    /// project is active (welcome state) or when the project has no entity
    /// yet (mid-`set_active_project`).
    pub(crate) fn active_project_panes(&self) -> Option<Entity<ProjectPanes>> {
        let id = self.active_project.as_ref().map(|p| p.id.as_str())?;
        self.project_panes_by_project.get(id).cloned()
    }

    /// (Re)wire every source-control-panel event subscription against the
    /// CURRENT `right_sidebar` entities. Called from `new` AND after every
    /// `set_active_project` sidebar rebuild: that rebuild mints fresh
    /// `git_panel` / `commit_graph` / `branch_commits` / `stash_panel`
    /// entities, so any subscription captured against the prior generation
    /// silently stops firing. (Single-file diff opens survive a rebuild
    /// because they route through a stable `weak_self` callback re-passed at
    /// every sidebar build, not through an entity subscription.) Overwriting
    /// each `_*_subscription` field drops the stale one.
    pub(crate) fn rewire_scm_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sc) = self
            .right_sidebar
            .as_ref()
            .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
        else {
            // Non-git sidebar — no SCM panels. Drop any subscriptions left
            // over from a prior git project.
            self._discard_subscription = None;
            self._push_stash_subscription = None;
            self._show_commit_subscription = None;
            self._show_branch_file_subscription = None;
            self._show_combined_diff_subscription = None;
            self._show_branch_diff_all_subscription = None;
            return;
        };
        // Clone the child entities + repo up front so the immutable read
        // borrow ends before `cx.subscribe_in` borrows `cx` mutably.
        let (git_panel, stash_panel, commit_graph, branch_commits, repo) = {
            let sc = sc.read(cx);
            (
                sc.git_panel.clone(),
                sc.stash_panel.clone(),
                sc.commit_graph.clone(),
                sc.branch_commits.clone(),
                sc.repo.clone(),
            )
        };

        self._discard_subscription = Some(cx.subscribe_in(
            &git_panel,
            window,
            |root, panel, _ev: &DiscardRequested, window, cx| {
                root.mount_discard_dialog(panel.clone(), window, cx);
            },
        ));

        self._push_stash_subscription = Some(cx.subscribe_in(
            &stash_panel,
            window,
            |root, panel, _ev: &PushStashRequested, window, cx| {
                root.mount_push_stash_dialog(panel.clone(), window, cx);
            },
        ));

        let commit_repo = repo.clone();
        self._show_commit_subscription = Some(cx.subscribe_in(
            &commit_graph,
            window,
            move |root, _graph, ev: &ShowCommitRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let sha = ev.sha.clone();
                let short_oid = ev.short_oid.clone();
                let subject = ev.subject.clone();
                let repo = commit_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_commit_tab(repo, sha, short_oid, subject, window, cx);
                });
            },
        ));

        let branch_file_repo = repo.clone();
        self._show_branch_file_subscription = Some(cx.subscribe_in(
            &branch_commits,
            window,
            move |root, _panel, ev: &ShowBranchFileRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let base = ev.base.clone();
                let head = ev.head.clone();
                let path = ev.path.clone();
                let repo = branch_file_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_branch_diff_tab(repo, base, head, path, window, cx);
                });
            },
        ));

        let combined_repo = repo.clone();
        self._show_combined_diff_subscription = Some(cx.subscribe_in(
            &git_panel,
            window,
            move |root, _panel, ev: &ShowCombinedDiffRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let scope = ev.scope.clone();
                let repo = combined_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_combined_diff_tab(repo, scope, window, cx);
                });
            },
        ));

        let branch_all_repo = repo.clone();
        self._show_branch_diff_all_subscription = Some(cx.subscribe_in(
            &branch_commits,
            window,
            move |root, _panel, ev: &ShowBranchDiffAllRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let scope = oximux_core::CombinedDiffScope::Branch {
                    base: ev.base.clone(),
                    head: ev.head.clone(),
                };
                let repo = branch_all_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_combined_diff_tab(repo, scope, window, cx);
                });
            },
        ));
    }

    /// Route the Explorer context-menu Rename action into the FileExplorer's
    /// inline-rename flow. The row turns into an editable
    /// Input in-place (no modal). FileExplorer owns the actual fs op +
    /// post-rename refresh; this handler just kicks the state transition.
    pub(crate) fn start_inline_file_rename(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Refuse paths without a parent (i.e. filesystem root) — rename
        // isn't meaningful there and `std::fs::rename` would fail anyway.
        if path.parent().is_none() {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                path = %path.display(),
                "rename refused: path has no parent directory"
            );
            return;
        }
        // Close the context menu so its backdrop doesn't sit on top of
        // the inline input the explorer is about to mount.
        self.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
        let Some(rs) = self.right_sidebar.as_ref() else {
            return;
        };
        let fe = rs.read(cx).file_explorer.clone();
        fe.update(cx, |fe, cx| fe.start_rename(path, window, cx));
    }

    /// Open `path` as a new editor tab in the active project's active
    /// pane group. If the file is already open in any tab of that group,
    /// activate it instead of opening a duplicate.
    ///
    /// Pre-filters binary / system files so the editor never opens an
    /// empty buffer on a UTF-8 decode failure. No-op when there's no
    /// active project.
    pub fn open_file_in_active_pane(
        &self,
        path: std::path::PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !is_openable_text_file(&path) {
            tracing::info!(
                file = %path.display(),
                "open-file: refusing non-text file (binary, system metadata, or unreadable)"
            );
            return;
        }
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            if preview {
                p.open_preview_editor_tab(path, window, cx);
            } else {
                p.open_or_activate_editor_tab(path, window, cx);
            }
        });
    }

    /// Open `path` as a read-only diff tab in the active project's active
    /// pane group. `staged=true` shows the staged-vs-HEAD diff; `false`
    /// shows worktree-vs-index. Idempotent — clicking the same SCM row
    /// re-focuses the existing diff tab rather than opening a duplicate.
    pub fn open_diff_in_active_pane(
        &self,
        repo: oximux_git::Repository,
        path: std::path::PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            p.open_or_activate_diff_tab(repo, path, staged, untracked, window, cx);
        });
    }

    /// Seed the Search panel's include-glob field and switch the right
    /// sidebar to the Search tab. Drives the file-tree "Find in Folder"
    /// context-menu item. No-ops when no right sidebar is mounted (the
    /// active project isn't a git repo OR no project is active).
    pub(crate) fn seed_search_include_and_switch(
        &self,
        include_glob: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rs) = self.right_sidebar.clone() else {
            return;
        };
        rs.update(cx, |sidebar, cx| {
            // Seed first, then flip the tab so the rendered panel
            // already shows the glob when it appears.
            let include_input = sidebar.search_panel.read(cx).include_input_ref().clone();
            include_input.update(cx, |state, cx| {
                state.set_value(include_glob.as_str(), window, cx);
            });
            sidebar.select_tab(crate::shell::right_sidebar::tab::RightTab::Search, cx);
        });
    }

    /// Reveal `path` in the file-tree sidebar: open the sidebar if collapsed,
    /// switch to the Explorer tab, then expand the path's ancestors and scroll
    /// to its row. Drives the editor breadcrumb's "Reveal in Explorer View"
    /// action. No-ops when no right sidebar is mounted.
    pub(crate) fn reveal_path_in_explorer(
        &self,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(rs) = self.right_sidebar.clone() else {
            return;
        };
        rs.update(cx, |sidebar, cx| {
            if !sidebar.open {
                sidebar.toggle(cx);
            }
            sidebar.select_tab(crate::shell::right_sidebar::tab::RightTab::Explorer, cx);
            sidebar
                .file_explorer
                .update(cx, |fe, cx| fe.reveal_path(path, cx));
        });
    }

    /// Force the file explorer to re-read its cached directories from disk.
    /// Called after a mutation (duplicate / delete) so the new tree state
    /// shows up without waiting for the filesystem watcher.
    fn refresh_file_explorer(&self, cx: &mut Context<Self>) {
        if let Some(rs) = self.right_sidebar.as_ref() {
            let fe = rs.read(cx).file_explorer.clone();
            fe.update(cx, |fe, cx| fe.manual_refresh(cx));
        }
    }

    /// Duplicate a file/folder next to itself, then refresh the tree and
    /// reveal the new entry. Errors surface as a toast — duplication failures
    /// (permissions, disk full) aren't recoverable from the UI.
    pub(crate) fn duplicate_file_entry(
        &mut self,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |weak, cx| {
            // Copy off the main thread — duplicating a large folder must not
            // freeze the UI. Re-enter the main thread only for the refresh.
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::shell::file_explorer::file_mutations::duplicate_path(&path)
                })
                .await;
            weak.update(cx, |this, cx| match result {
                Ok(new_path) => {
                    this.refresh_file_explorer(cx);
                    this.reveal_path_in_explorer(new_path, cx);
                }
                Err(err) => {
                    this.push_toast(ToastKind::Error, format!("Duplicate failed: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Mount a plain confirm dialog for a file-tree Delete. On confirm the
    /// target is moved to the macOS Trash (reversible) and the tree refreshes;
    /// an open editor tab for the path is left to the external-mutation sweep,
    /// which flags it as deleted. Reuses the shared `confirm_dialog` slot +
    /// observer, same as the SCM discard flow.
    pub(crate) fn mount_file_delete_confirm(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let kind = if path.is_dir() { "folder" } else { "file" };
        let body = format!("Move the {kind} “{name}” to the Trash? You can restore it from the Trash.");

        let target = path.clone();
        let weak = cx.entity().downgrade();
        let on_confirm: ConfirmCallback = Rc::new(move |_window, cx| {
            match crate::shell::file_explorer::file_mutations::move_to_trash(&target) {
                Ok(()) => {
                    if let Some(root) = weak.upgrade() {
                        root.update(cx, |root, cx| root.refresh_file_explorer(cx));
                    }
                }
                Err(err) => crate::shell::toast::toast_op_error(cx, "Delete", &err),
            }
        });

        let prompt = ConfirmPrompt {
            title: "Move to Trash".into(),
            body: body.into(),
            expected: "".into(),
            on_confirm,
            confirm_label: Some("Move to Trash".into()),
            on_cancel: None,
            secondary: None,
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));
        // Cancel any in-flight observer (e.g. an SCM discard dialog) before
        // installing this one, matching the explicit-clear pattern used at the
        // other `confirm_dialog` mount sites.
        self._discard_dialog_observer = None;
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));
        self.confirm_dialog = Some(dialog);
        cx.notify();
    }

    /// Mount a `ConfirmDialog` for the SCM panel's pending discard
    /// request. Builds the prompt copy from the panel's snapshot,
    /// wires `on_confirm` to `confirmed_discard_path` and `on_cancel`
    /// to `clear_pending_discard`, then installs an observer that
    /// drops the dialog from the slot once the user confirms or
    /// cancels.
    fn mount_discard_dialog(
        &mut self,
        panel: Entity<GitPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = panel.read(cx).pending_discard().cloned() else {
            return;
        };

        // Dispatch on `scope`: Single → the existing single-path flow
        // (which Slice C teaches about pure-untracked dispatch);
        // Area → the per-section sequence with its unstage-first-for-
        // staged + git-clean-for-untracked branches.
        //
        // `request.paths` is moved (not cloned): `request` is the
        // owned snapshot from `pending_discard().cloned()` and we
        // don't use the rest of it after this point. The inner
        // `paths.clone()` in the callback is the unavoidable one —
        // `ConfirmCallback` is `Rc<dyn Fn>` and may fire more than
        // once.
        let on_confirm: ConfirmCallback = {
            let panel = panel.clone();
            let scope = request.scope;
            let paths = request.paths;
            Rc::new(move |_window, cx| {
                panel.update(cx, |p, cx| match scope {
                    crate::shell::git_panel::DiscardScope::Single { .. } => {
                        if let Some(path) = paths.first().cloned() {
                            p.confirmed_discard_path(path, cx);
                        }
                    }
                    crate::shell::git_panel::DiscardScope::Area { area } => {
                        p.confirmed_discard_area(area, paths.clone(), cx);
                    }
                });
            })
        };
        let on_cancel: ConfirmCallback = {
            let panel = panel.clone();
            Rc::new(move |_window, cx| {
                panel.update(cx, |p, cx| p.clear_pending_discard(cx));
            })
        };

        let prompt = ConfirmPrompt {
            title: request.copy.title,
            body: request.copy.body,
            expected: request.expected,
            on_confirm,
            confirm_label: Some(request.copy.confirm_label),
            on_cancel: Some(on_cancel),
            secondary: None,
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));

        // Drop the dialog the moment the user resolves it. Replacing
        // `_discard_dialog_observer` cancels any previous observer
        // that's tied to a stale dialog.
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));

        self.confirm_dialog = Some(dialog);
        cx.notify();
    }

    /// Mount a `PushStashDialog` for the SCM panel's stash-push
    /// request. Wires `on_confirm` to call `StashPanel::push` with
    /// the user-supplied message + include-untracked toggle. Installs
    /// an observer that drops the dialog from the slot once the user
    /// confirms or cancels.
    ///
    /// First-open-wins: a double-click on the header `+` button (or
    /// any sequence that re-fires `PushStashRequested` while the
    /// dialog is already mounted) is ignored. Replacing the slot
    /// would silently drop a half-typed form, which is the bug Phase
    /// 01's discard-dialog reviewer caught for the destructive flow;
    /// applying the same guard here so the user's in-progress
    /// message survives a stray re-click.
    fn mount_push_stash_dialog(
        &mut self,
        panel: Entity<StashPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.push_stash_dialog.is_some() {
            return;
        }
        let on_confirm: PushCallback = {
            let panel = panel.clone();
            Rc::new(move |msg, include_untracked, _window, cx| {
                panel.update(cx, |p, cx| p.push(msg, include_untracked, cx));
            })
        };
        // Cancel path is a no-op on the panel side — the dialog flips
        // `cancelled`, the observer below drops the slot. Wired anyway
        // so future telemetry (e.g. counting abandoned pushes) has a
        // hook point.
        let on_cancel: CancelCallback = Rc::new(|_window, _cx| {});

        let prompt = PushStashPrompt {
            on_confirm,
            on_cancel: Some(on_cancel),
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| PushStashDialog::new(prompt, theme, density, typography, window, cx));

        // Drop the dialog the moment the user resolves it. Replacing
        // `_push_stash_dialog_observer` cancels any previous observer
        // tied to a stale dialog.
        self._push_stash_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.push_stash_dialog = None;
                    root._push_stash_dialog_observer = None;
                    cx.notify();
                }
            },
        ));

        self.push_stash_dialog = Some(dialog);
        cx.notify();
    }

    /// Build the on-click callback handed to the SCM panel for diff
    /// opens. Captures a weak self-handle so the callback survives
    /// project switches that rebuild `RightSidebar`. The `repo`
    /// argument is captured at build time — RightSidebar already owns
    /// it for the lifetime of the source-control surface.
    pub(crate) fn build_on_open_diff_callback(
        weak: WeakEntity<Self>,
        repo: oximux_git::Repository,
    ) -> crate::shell::file_tree_view::OnOpenDiff {
        Arc::new(move |path, staged, untracked, window, cx| {
            let repo = repo.clone();
            let _ = weak.update(cx, |this, cx| {
                this.open_diff_in_active_pane(repo, path, staged, untracked, window, cx);
            });
        })
    }

    /// Build the on-click callback handed to the Files-tab `FileTreeView`.
    /// The closure captures a weak self-handle so the callback survives
    /// project switches that rebuild `RightSidebar`. A dropped weak handle
    /// (window closed) silently no-ops the click.
    pub(crate) fn build_on_open_file_callback(
        weak: WeakEntity<Self>,
    ) -> crate::shell::file_tree_view::OnOpenFile {
        Arc::new(move |path, preview, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.open_file_in_active_pane(path, preview, window, cx);
            });
        })
    }

    /// Build the active-file query handed to the Files-tab `FileTreeView`.
    /// Resolves the focused leaf of the active project's active tab and
    /// returns the file path of its currently-active editor tab (`None`
    /// when the focused leaf is a terminal or when no project is active).
    /// Fires once per FileTreeView render; cheap enough to walk on every
    /// frame since the tab + pane lookups are HashMap reads.
    pub(crate) fn build_on_query_active_path_callback(
        weak: WeakEntity<Self>,
    ) -> crate::shell::file_tree_view::OnQueryActivePath {
        Arc::new(move |cx| {
            let root = weak.upgrade()?;
            let panes = root.read(cx).active_project_panes()?;
            panes.read(cx).active_editor_path(cx)
        })
    }

    /// Walk every open project's tabs and serialize plain-terminal
    /// scrollback to `pane_buffers`. Called from the app-quit hook so
    /// state restored on next launch reflects the user's final view.
    pub fn capture_all_pane_buffers(&self, cx: &gpui::App) {
        let repo = self.app_state.pane_buffer_repo.clone();
        let window_id = &self.window_id;
        for (project_id, panes) in &self.project_panes_by_project {
            panes.read(cx).capture_pane_buffers(
                &repo,
                project_id,
                window_id,
                crate::project_panes_factory::PANE_BUFFER_MAX_BYTES,
                cx,
            );
        }
    }

    /// Walk every open project's `ProjectPanes` and persist its full
    /// layout snapshot (groups, sub-pane trees, tab_order, active
    /// indices, editor paths, agent metadata). Without this hook the
    /// on-quit save chain would only persist pane scrollback + relay
    /// ids — every tab/group structural change made during the session
    /// would be lost. Pairs with `capture_all_pane_buffers` so a single
    /// quit fires both writes.
    pub fn capture_all_layouts(&self, cx: &gpui::App) {
        for panes in self.project_panes_by_project.values() {
            panes.read(cx).save_now(cx);
        }
    }

    /// Walk every open project's tabs and persist each plain-terminal
    /// leaf's relay PTY id (Phase 5 step 6). Called from the same
    /// hooks as `capture_all_pane_buffers` so the two tables stay in
    /// sync. No-op when there's no relay session (in-process backend).
    pub fn capture_all_pane_relay_ids(&self, cx: &gpui::App) {
        let snap = crate::shell::terminal_view::relay_state_snapshot();
        let Some(session_id) = snap.session_id else {
            return;
        };
        self.capture_all_pane_relay_ids_with_session(&session_id, cx);
    }

    /// Same capture, but with the relay session id already in hand —
    /// for callers that just took a relay snapshot (the post-paint
    /// reconcile), so the capture adds no extra daemon round-trip on
    /// the main thread.
    pub fn capture_all_pane_relay_ids_with_session(&self, session_id: &str, cx: &gpui::App) {
        let repo = self.app_state.pane_relay_id_repo.clone();
        let window_id = &self.window_id;
        for (project_id, panes) in &self.project_panes_by_project {
            panes
                .read(cx)
                .capture_pane_relay_ids(&repo, project_id, window_id, session_id, cx);
        }
    }

    /// The id of the project currently active in this window, if any. Read
    /// by the open-windows manifest writer so the next launch can reopen
    /// this window onto the same project.
    pub(crate) fn active_project_id(&self) -> Option<String> {
        self.active_project.as_ref().map(|p| p.id.clone())
    }

    /// Borrow this window's `SettingsRepo` (shared app-wide via the same DB).
    /// The lib-level session-capture helper uses it to persist the
    /// open-windows manifest without the binary crate reaching into
    /// `AppState`'s private fields.
    pub(crate) fn settings_repo(&self) -> &oximux_storage::SettingsRepo {
        &self.app_state.settings_repo
    }

    /// Spawn the chosen agent in a new tab inside the active pane group.
    /// Runs the start_session → backend_for → terminal_session_id →
    /// subscribe_status chain, then hands the assembled handles to
    /// `ProjectPanes::push_agent_tab`.
    ///
    /// If `update_in` errors (window/workspace dropped mid-spawn), cancels
    /// the half-mounted session so the PTY doesn't zombie.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_agent_tab(
        &self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        cwd: std::path::PathBuf,
        model: Option<String>,
        effort: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        // Apply per-agent launch defaults from `agent_launch.toml`: fill the
        // model when the caller didn't pin one, and append the configured
        // extra flags (e.g. a skip-permissions default). The global is unset
        // until the settings layer seeds it, in which case defaults are empty.
        let (model, extra_args) = {
            let defaults = cx.try_global::<oximux_settings::AgentLaunchSettings>();
            (
                model.or_else(|| defaults.and_then(|d| d.model_for(adapter_id))),
                defaults.map(|d| d.args_for(adapter_id)).unwrap_or_default(),
            )
        };
        let runtime = self.cli_runtime.clone();
        let cwd_for_tab = cwd.clone();

        cx.spawn_in(window, async move |root, cx| {
            // `adapter_id` arrives from the row the user clicked — the
            // picker holds the `RegistryEntry` slug at click time, so we
            // skip a redundant `detect_available` walk here (M1 fix from
            // review 260520-1830).
            let cfg = AgentSessionConfig {
                adapter,
                worktree_path: cwd,
                prompt: None,
                model: model.clone(),
                effort: effort.clone(),
                extra_args,
                env: Vec::new(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                custom_command: None,
            };
            let session_id = match runtime.start_session(cfg).await {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, adapter = adapter_id, "start_session failed");
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };
            let backend = match runtime.backend_for(session_id) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(?err, "backend_for after start_session");
                    let _ = runtime.cancel(session_id).await;
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };
            let term_id = match runtime.terminal_session_id(session_id) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, "terminal_session_id after start_session");
                    let _ = runtime.cancel(session_id).await;
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };
            let status_rx = match runtime.subscribe_status(session_id) {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(?err, "subscribe_status after start_session");
                    let _ = runtime.cancel(session_id).await;
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };

            // Mirror this session's status history into the agent_sessions
            // row so the rail/dashboard rows show Running / Done / Stopped.
            // Watch receivers are cheap clones; the watcher self-terminates
            // on the terminal status.
            let _ = root.update(cx, |this, cx| {
                crate::shell::agent_session_persistence::spawn_for_session(
                    this,
                    cwd_for_tab.to_string_lossy().into_owned(),
                    adapter_id,
                    model.clone(),
                    effort.clone(),
                    status_rx.clone(),
                    cx,
                );
            });

            let mount_result = panes.update_in(cx, |p, window, cx| {
                p.push_agent_tab(
                    adapter,
                    adapter_id,
                    cwd_for_tab,
                    model,
                    effort,
                    session_id,
                    status_rx,
                    backend,
                    term_id,
                    None,
                    window,
                    cx,
                );
            });
            if mount_result.is_err() {
                tracing::warn!(
                    ?session_id,
                    "spawn_agent_tab: workspace dropped mid-spawn; cancelling orphan"
                );
                let _ = runtime.cancel(session_id).await;
            }
        })
        .detach();
    }

    /// Accessor for the workspace's CLI agent runtime. Used by the (future)
    /// settings panel + tests; the main consumer is the per-project
    /// `ProjectPanes`, which receives its own `Arc` clone at construction.
    #[doc(hidden)]
    pub fn cli_runtime(&self) -> Arc<CliRuntime> {
        self.cli_runtime.clone()
    }

    /// Accessor for the adapter registry. Same rationale as `cli_runtime`.
    #[doc(hidden)]
    pub fn adapter_registry(&self) -> Arc<AdapterRegistry> {
        self.adapter_registry.clone()
    }

    /// Test-only inspector for the left-rail visibility flag.
    #[doc(hidden)]
    pub fn left_rail_open(&self) -> bool {
        self.left_rail_open
    }

    // -----------------------------------------------------------------------
    // Cross-window tear-off (Slice C)
    // -----------------------------------------------------------------------

    /// Handler for the "Move Tab to New Window" context-menu action.
    ///
    /// Ordering contract (relay client enforces single subscriber per PTY):
    ///   1. Collect the tab's relay external_id while the view is still alive.
    ///   2. Call `detach` on each terminal leaf so the relay session is released
    ///      WITHOUT killing the daemon PTY.
    ///   3. `take_tab` removes the tab from the source group. The now-detached
    ///      `TerminalView` drops harmlessly (its `Drop` → `close` is a no-op
    ///      after `detach`).
    ///   4. Push a `PendingTearOff` for the minted destination window id.
    ///   5. Spawn an async task that opens the destination window. The window
    ///      build closure calls `consume_pending_tearoff` and then
    ///      `mount_pending_tearoff` to attach the PTY and mount a fresh
    ///      `TerminalView` in the new window's context.
    ///
    /// Rollback: if `attach_pty_existing` fails in the destination window, the
    /// PTY is orphaned in the daemon (alive but with no subscriber). We log
    /// loudly and do NOT silently swallow it. A future enhancement could
    /// re-attach to the source window; for v1 the orphan is a daemon-level
    /// concern (the relay's idle-gc eventually reaps it).
    pub(crate) fn handle_move_tab_to_new_window(
        &mut self,
        group_id_raw: u64,
        tab_idx_raw: u32,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        let group_id = crate::shell::pane_tree::PaneGroupId(group_id_raw);
        let tab_idx = tab_idx_raw as usize;

        // 1. Borrow the group to collect external_id(s) and tab metadata.
        //    All reads happen while the tab is still alive.
        let (external_ids, label, color, custom_title) = {
            let panes_ref = panes.read(cx);
            let Some(group) = panes_ref.group(group_id) else {
                return;
            };
            let group_ref = group.read(cx);
            let Some(tab) = group_ref.tabs().get(tab_idx) else {
                return;
            };
            let label = tab
                .custom_title
                .clone()
                .unwrap_or_else(|| tab.label.clone());
            let color = tab.color;
            let custom_title = tab.custom_title.clone();
            // Collect external_ids from every live terminal leaf.
            let ids: Vec<String> = match &tab.content {
                crate::shell::pane_content::PaneContent::Terminal(tree) => tree
                    .iter_live()
                    .filter_map(|(_, view)| view.read(cx).external_id())
                    .collect(),
                _ => return, // not a terminal tab — bail silently
            };
            if ids.is_empty() {
                tracing::warn!(
                    group_id = group_id_raw,
                    tab_idx,
                    "move-tab: no relay external_id on this tab; tear-off skipped"
                );
                return;
            }
            (ids, label, color, custom_title)
        };

        // 2. Detach BEFORE take_tab so the subscription is released
        //    while the TerminalView is still alive.
        {
            let panes_ref = panes.read(cx);
            let Some(group) = panes_ref.group(group_id) else {
                return;
            };
            let group_ref = group.read(cx);
            if let Some(tab) = group_ref.tabs().get(tab_idx)
                && let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content
            {
                for (_, view) in tree.iter_live() {
                    view.read(cx).detach();
                }
            }
        }

        // 3. Remove the tab from the source group. The detached TerminalViews
        //    drop here; their Drop → close is now a no-op.
        panes.update(cx, |p, cx| {
            if let Some(group) = p.group(group_id) {
                group.update(cx, |g, cx| {
                    let _ = g.take_tab(tab_idx, cx);
                });
            }
        });

        // 4. Mint a destination persist id and push the pending entry.
        let dest_window_id = crate::window_registry::next_persist_id(cx);
        let leaves: Vec<crate::window_registry::PendingLeaf> = external_ids
            .into_iter()
            .map(|id| crate::window_registry::PendingLeaf { external_id: id })
            .collect();
        let app_state = self.app_state.clone();
        let project_id = self.active_project_id();
        let pending = crate::window_registry::PendingTearOff {
            dest_window_id: dest_window_id.clone(),
            leaves,
            label,
            color,
            custom_title,
        };
        crate::window_registry::push_pending_tearoff(pending);

        // 5. Open the destination window asynchronously so we're out of the
        //    current borrow stack. The window build closure (in window_factory)
        //    consumes the pending entry and calls `mount_pending_tearoff`.
        cx.spawn_in(window, async move |_root, cx| {
            let _ = cx.update(|_window, cx| {
                crate::window_factory::open_workspace_window_with(
                    cx,
                    None, // repo resolved from project_id in window_factory
                    app_state,
                    dest_window_id,
                    project_id,
                );
            });
        })
        .detach();
    }

    /// Called by the destination window's build closure (via
    /// `window_factory::open_workspace_window_with`) when a pending tear-off
    /// entry is found for this window's id.
    ///
    /// For each relay PTY leaf in the entry: attach the existing relay PTY,
    /// mount a fresh `TerminalView`, and push it as a tab into the active
    /// pane group. The tab inherits the label, color, and custom title from
    /// the source window.
    ///
    /// On failure (e.g. `attach_pty_existing` returns `None`): logs a loud
    /// warning. The PTY is orphaned in the relay daemon and will be reaped by
    /// the daemon's idle-gc. No silent swallowing.
    pub fn mount_pending_tearoff(
        &mut self,
        tearoff: crate::window_registry::PendingTearOff,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            tracing::warn!(
                dest_window_id = %tearoff.dest_window_id,
                "mount_pending_tearoff: no active project panes; PTY orphaned in relay"
            );
            return;
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        // The torn-off PTY survives in the daemon, so its shell keeps the
        // original OXIMUX_* env it was spawned with. The destination view
        // gets a fresh identity under THIS window's workspace for future
        // persistence/respawn (carrying the source ids across windows is a
        // follow-up).
        let workspace_id = panes.read(cx).cwd().to_string_lossy().into_owned();

        for leaf in &tearoff.leaves {
            let Some((backend, session_id)) =
                crate::shell::terminal_view::attach_pty_existing(&leaf.external_id)
            else {
                tracing::warn!(
                    external_id = %leaf.external_id,
                    dest_window_id = %tearoff.dest_window_id,
                    "mount_pending_tearoff: attach_pty_existing failed; PTY orphaned in relay"
                );
                continue;
            };

            // Mount a fresh TerminalView in this window's entity context.
            // Entity<TerminalView> cannot cross windows — a new one is
            // required in the destination window context.
            let ids = crate::shell::context_env::SurfaceIds::fresh(workspace_id.clone());
            let view = cx.new(|cx| {
                crate::shell::terminal_view::TerminalView::mount(
                    backend,
                    session_id,
                    ids,
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
            });

            let label_str = tearoff.label.to_string();
            let color_for_tab = tearoff.color;
            let custom_title_for_tab = tearoff.custom_title.clone();
            panes.update(cx, |p, cx| {
                if let Some(group) = p.active_group() {
                    group.update(cx, |g, cx| {
                        // Use the restore helper to append the tab (wires
                        // the observer that drives group re-renders on
                        // TerminalView notifications).
                        g.push_restored_terminal_tab(label_str.clone(), view.clone(), cx);
                        // Apply color + custom title onto the freshly-appended tab.
                        let last_idx = g.tabs().len().saturating_sub(1);
                        g.set_tab_color(last_idx, color_for_tab, cx);
                        if custom_title_for_tab.is_some() {
                            g.set_tab_title(last_idx, custom_title_for_tab.clone(), cx);
                        }
                    });
                }
            });
        }
    }

    /// Route a directional split to the active project's pane groups.
    /// New sibling spawns one Terminal tab + steals focus.
    fn split_active_pane_group(
        &self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            p.split_active_group(axis, insert, window, cx);
        });
    }

    /// Reshape the active project's pane layout to `preset`. No-op when
    /// no project is active.
    fn reshape_active_project_layout(
        &self,
        preset: crate::shell::pane_group::layout_presets::Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            p.apply_layout_preset(preset, window, cx);
        });
    }

    /// Close the focused pane group in the active project. Manager
    /// returns `LastGroup` when no siblings exist; we swallow that so
    /// the keybind / menu item is a no-op rather than an error popup.
    fn close_active_pane_group(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            let _ = p.close_active_group(window, cx);
        });
    }

    /// Surface a quiet transient toast (bottom-right). The one entry point for
    /// fleeting cross-surface events; routes to the owned `ToastLayer`.
    pub(crate) fn push_toast(&self, kind: ToastKind, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        self.toast_layer.update(cx, |layer, cx| layer.push(kind, text, cx));
    }
}

// `build_project_panes` + restore helpers live in
// `crate::project_panes_factory` so this file stays under the 800-LOC cap.
// `impl Focusable for WorkspaceRoot` lives in `shell::workspace_ops` for
// the same reason.

impl Render for WorkspaceRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Push sidebar data down before LeftRail::render runs in the tree.
        self.refresh_left_rail(cx);

        // Tell the panes whether a modal/overlay is covering them this frame,
        // so any embedded browser webview (a native view above the GPU canvas)
        // hides instead of floating over the modal. Set before the panes
        // render below.
        let panes_covered = self.palette.read(cx).is_open()
            || self.project_picker.read(cx).is_open()
            || self.settings_modal.read(cx).is_open()
            || self.workspace_dialog.read(cx).is_open()
            || self.add_project_dialog.read(cx).is_open()
            || self.adapter_picker.read(cx).is_open()
            || self.pane_actions.read(cx).is_open()
            || self.tab_context_menu.read(cx).is_open()
            || self.file_tree_context_menu.read(cx).is_open()
            || self.git_row_context_menu.read(cx).is_open()
            || self.commit_context_menu.read(cx).is_open()
            || self.row_menu.read(cx).is_open()
            || self.project_menu.read(cx).is_open()
            || self.floating_terminal_visible
            || self.confirm_dialog.is_some()
            || self.rename_tab_dialog.is_some()
            || self.push_stash_dialog.is_some();
        cx.set_global(crate::shell::browser_view::WebviewSuppressed(panes_covered));
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        // Refresh toast tokens each render (same push-down doctrine as the
        // rail/pane surfaces); store-only, no notify.
        self.toast_layer.update(cx, |layer, _| {
            layer.set_tokens(theme, density, typography.clone());
        });

        // Status-bar pane count = visible pane-group leaves in the active
        // project (1 when no splits, N after Cmd+D).
        let active_panes = self.active_project_panes();
        let pane_count = active_panes
            .as_ref()
            .map(|p| p.read(cx).manager().in_order_groups().len())
            .unwrap_or(0);

        // Aggregate agent count across every group in the active project:
        // spawned `Agent` tabs PLUS plain terminals running a hand-launched
        // agent (detected from the terminal title). Users care about total
        // agents in flight, not just the foreground group's.
        let agent_count = active_panes
            .as_ref()
            .map(|p| {
                let panes = p.read(cx);
                panes.agent_count(cx) + panes.ambient_agent_count(cx)
            })
            .unwrap_or(0);

        // True TTY count = terminal + agent tabs across every group.
        let tty_count = active_panes
            .as_ref()
            .map(|p| p.read(cx).tty_count(cx))
            .unwrap_or(0);

        // Route poll state to the status bar via the RightSidebar getter.
        // Non-git projects keep their PollState pinned at Loading forever
        // (no poller exists), so gate on `has_repo` to keep the status
        // bar from showing a perpetual "loading git…" placeholder.
        let git_state = self.right_sidebar.as_ref().and_then(|s| {
            let sidebar = s.read(cx);
            if sidebar.has_repo() {
                Some(sidebar.latest_poll_state().clone())
            } else {
                None
            }
        });

        // Activity-bar tabs are composed here so top_bar / right_sidebar stay
        // decoupled. Tabs only render when the sidebar is open.
        let (right_open, right_tabs) = match self.right_sidebar.as_ref() {
            Some(sidebar_entity) => {
                let sidebar_ref = sidebar_entity.read(cx);
                let open = sidebar_ref.open;
                let tabs_element = if open {
                    let tabs = sidebar_ref.visible_tabs();
                    let active = sidebar_ref.active_tab;
                    Some(
                        render_tab_buttons(active, &tabs, sidebar_entity, theme).into_any_element(),
                    )
                } else {
                    None
                };
                (open, tabs_element)
            }
            None => (false, None),
        };

        // Push current chrome width into the active ProjectPanes so PTY
        // grids match the actual visible area. ProjectPanes forwards the
        // value into each group it owns.
        // Read the live rail width through the entity so pane grids
        // reflow on every resize-drag tick (set_width's cx.notify
        // triggers a render which re-runs this read).
        let left_chrome = if self.left_rail_open {
            f32::from(self.left_rail.read(cx).width())
        } else {
            0.0
        };
        // Phase 13: the sidebar width is now state, not the
        // DEFAULT_PANEL_WIDTH const. Read the live width through the
        // sidebar entity so PTY grids reflow correctly on every drag
        // tick (set_panel_width's cx.notify triggers a render, which
        // re-runs this read).
        let right_chrome = if right_open {
            self.right_sidebar
                .as_ref()
                .map(|s| f32::from(s.read(cx).panel_width()))
                .unwrap_or_else(|| f32::from(DEFAULT_PANEL_WIDTH))
        } else {
            0.0
        };
        if let Some(panes) = active_panes.as_ref() {
            let chrome = left_chrome + right_chrome;
            panes.update(cx, |p, cx| p.set_chrome_width(chrome, cx));
        }

        // Push the active workspace's tint down so its tab strip's active-tab
        // edge carries the identifier hue. Resolved from the (already-refreshed)
        // left-rail snapshot, so no separately-cached copy can desync.
        let ws_tint = self.left_rail.read(cx).active_workspace_tint();
        if let Some(panes) = active_panes.as_ref() {
            panes.update(cx, |p, _cx| p.set_workspace_tint(ws_tint));
        }

        // Top-row pane groups hoist their per-group strips INTO the
        // top-bar row (mirroring tree column widths). Lower vertical-
        // split rows render their strips inline above their bodies.
        let workspace_tab_strip: Option<AnyElement> = active_panes
            .as_ref()
            .and_then(|panes| panes.read(cx).topmost_tab_strip((*panes).clone(), cx));
        // Drag-claim band (below) is only needed when there are tab
        // chips to protect from AppKit title-bar drag hijack. When
        // there are no tabs (welcome state), skipping the band trims
        // ~22px of dead chrome and aligns the empty-state chrome with
        // the reference editor's compact single-row header.
        let has_tabs = workspace_tab_strip.is_some();

        // Center column body: active project's ProjectPanes (which renders
        // its group tree internally), or welcome placeholder when no
        // project is active.
        let center_body: AnyElement = match active_panes.clone() {
            Some(view) => view.into_any_element(),
            None => main_area::view(theme, density, typography).into_any_element(),
        };

        // Per-column header layout — single 30-px chrome band at top
        // with each column owning its own header segment side-by-side:
        //   - left_header  : traffic-light gutter + wordmark + left toggle
        //   - center_header: collapsed clusters when rails closed;
        //                    otherwise just a flex spacer (the tab
        //                    strip lives in its OWN row below — see
        //                    `strip_row` further down)
        //   - right_header : activity tabs + right toggle (only when
        //                    sidebar open)
        //
        // Because each header is per-column, the activity tabs naturally
        // dock at the LEFT edge of the right sidebar (NOT at the far
        // right of the window).
        //
        // The per-pane tab strip is hoisted into a SEPARATE row inside
        // the center column, BELOW center_header — keeps tab chips at
        // `y > 30`, safely outside AppKit's title-bar drag zone (top
        // ~28px). Required for chip drag-reorder to work — empirically
        // verified: putting chips at `y < 28` (e.g. inside
        // center_header at y=1..29) breaks GPUI drag delivery.

        // Activity tabs route through the OPEN right column's header
        // when the sidebar is open; otherwise they collapse into the
        // center header (so the user can still see them).
        let (center_right_tabs, right_column_tabs) = if right_open {
            (None, right_tabs)
        } else {
            (right_tabs, None)
        };

        let left_column = if self.left_rail_open {
            Some(
                div()
                    .flex()
                    .flex_col()
                    .h_full()
                    .flex_shrink_0()
                    .child(top_bar::left_header(theme, density, typography))
                    .child(self.left_rail.clone()),
            )
        } else {
            None
        };

        // Tab strip row height matches the chrome row (h_top_bar) for
        // visual symmetry — two equal-height rows stacked.
        // Chips inside still render at their natural 28px height
        // (`TAB_STRIP_HEIGHT_PX`); `items_center` on the outer row
        // gives them top/bottom breathing room inside the 36px band.
        let strip_row_height_px = density.h_top_bar;
        let center_column = {
            // VS Code–style command center in the center chrome zone (the tab
            // strip lives in its OWN row below — not here). Resting label is the
            // active project name; click opens Quick Open.
            let command_center = top_bar::command_center(
                self.active_project.as_ref().map(|p| p.name.clone()),
                theme,
                density,
                typography,
            )
            .into_any_element();
            let header = top_bar::center_header(
                self.left_rail_open,
                right_open,
                Some(command_center),
                center_right_tabs,
                has_tabs, // suppress header bottom border when the tab strip renders below
                theme,
                density,
                typography,
            );
            let strip_row = workspace_tab_strip.map(|strip| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .h(px(strip_row_height_px))
                    .bg(theme.bg_panel)
                    // No bottom border here: the hoisted tab strip already
                    // paints its own bottom border (the focused-group accent),
                    // so a border on this wrapper would double it into a
                    // parallel hairline a couple px below.
                    .child(strip)
            });
            let body = div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .child(center_body);
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .child(header)
                .when_some(strip_row, |s, r| s.child(r))
                .child(body)
        };

        let right_column = match (self.right_sidebar.clone(), right_open) {
            (Some(sidebar), true) => Some(
                div()
                    .flex()
                    .flex_col()
                    .h_full()
                    .flex_shrink_0()
                    .child(top_bar::right_header(right_column_tabs, theme, density))
                    .child(sidebar),
            ),
            _ => None,
        };

        let mut row = div().flex().flex_row().flex_1().min_h(px(0.)).w_full();
        if let Some(col) = left_column {
            row = row.child(col);
        }
        row = row.child(center_column);
        if let Some(col) = right_column {
            row = row.child(col);
        }

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            // Phase 13: route sidebar-resize drag ticks. The handle
            // itself lives inside RightSidebar (left edge of the
            // column); the move listener has to live on a parent
            // wide enough to keep the cursor inside its bounds for
            // the duration of the drag — `size_full` qualifies. The
            // handler reads the cursor's window-relative x and the
            // listener-div's LIVE bounds width (`ev.bounds.size`),
            // so an OS-window resize mid-drag immediately shifts the
            // clamp ceiling without staring at a stale snapshot. The
            // payload's `window_width` is a fallback for the rare
            // frame where bounds are still zero (pre-layout).
            .on_drag_move::<crate::shell::right_sidebar::resize::SidebarResizePayload>(
                cx.listener(
                    |this, ev: &DragMoveEvent<
                        crate::shell::right_sidebar::resize::SidebarResizePayload,
                    >, _window, cx| {
                        let Some(sidebar) = this.right_sidebar.clone() else {
                            return;
                        };
                        let live_width = f32::from(ev.bounds.size.width);
                        let window_width = if live_width > 0.0 {
                            live_width
                        } else {
                            ev.drag(cx).window_width
                        };
                        let cursor_x = f32::from(ev.event.position.x);
                        sidebar.update(cx, |s, cx| {
                            crate::shell::right_sidebar::resize::apply_drag_move(
                                s,
                                cursor_x,
                                window_width,
                                cx,
                            );
                        });
                    },
                ),
            )
            // Route left-rail resize drag ticks. The handle lives on the
            // rail's right edge; the move listener sits on this full-size
            // row so the cursor stays inside its bounds for the whole
            // drag. The rail's left edge is pinned at window x=0, so the
            // new width is simply the cursor's window x.
            .on_drag_move::<crate::shell::left_rail::resize::LeftRailResizePayload>(
                cx.listener(
                    |this,
                     ev: &DragMoveEvent<
                        crate::shell::left_rail::resize::LeftRailResizePayload,
                    >,
                     _window,
                     cx| {
                        let cursor_x = f32::from(ev.event.position.x);
                        this.left_rail.update(cx, |rail, cx| {
                            crate::shell::left_rail::resize::apply_drag_move(rail, cursor_x, cx);
                        });
                    },
                ),
            )
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _window, cx| {
                this.left_rail_open = !this.left_rail_open;
                cx.notify();
            }))
            .on_action(cx.listener(
                |this, action: &SendTextToActiveAgent, _window, cx| {
                    // Resolve the routing target on the spot: the active
                    // project's first agent session, preferring the
                    // currently-focused tab. No-op when nothing is open.
                    let Some(panes) = this.active_project_panes() else {
                        tracing::debug!("send-to-agent: no active project");
                        return;
                    };
                    let Some(session_id) = panes.read(cx).target_agent_session(cx) else {
                        tracing::debug!("send-to-agent: no agent session available");
                        return;
                    };
                    let runtime = this.cli_runtime.clone();
                    let text = action.text.clone();
                    // CLI runtime call is async (writes to the agent's PTY
                    // off-thread). Detach — UI doesn't block on the write
                    // and Drop-on-error just surfaces in the trace log.
                    cx.background_spawn(async move {
                        if let Err(err) = runtime.send_message(session_id, &text).await {
                            tracing::warn!(?session_id, %err, "send-to-agent failed");
                        }
                    })
                    .detach();
                },
            ))
            .on_action(cx.listener(|this, _: &OpenQuickOpen, window, cx| {
                // Mutex with every other full-window overlay (close-then-open).
                this.close_modal_overlays(cx);
                let root = this
                    .active_project
                    .as_ref()
                    .map(|p| std::path::PathBuf::from(&p.root_path));
                this.palette.update(cx, |p, cx| {
                    p.open(PaletteMode::QuickOpen, window, cx);
                    // Lazily build the file index for the active project; the
                    // call is a no-op when already loaded for this project.
                    if let Some(root) = root {
                        p.kick_file_index(root, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.close_modal_overlays(cx);
                this.palette
                    .update(cx, |p, cx| p.open(PaletteMode::Commands, window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenWorkspaceJump, window, cx| {
                this.close_modal_overlays(cx);
                // Snapshot all workspaces + attention state, push into the
                // palette, then open it in jump mode.
                let items = this.build_workspace_jump_items(cx);
                this.palette.update(cx, |p, cx| {
                    p.set_workspace_items(items, cx);
                    p.open(PaletteMode::WorkspaceJump, window, cx);
                });
            }))
            .on_action(cx.listener(
                |this, action: &ActivateWorkspaceFromJump, window, cx| {
                    this.activate_workspace_from_jump(
                        action.workspace_id.clone(),
                        action.project_id.clone(),
                        action.worktree_path.clone(),
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &oximux_editor::RevealInExplorer, _window, cx| {
                    this.reveal_path_in_explorer(std::path::PathBuf::from(&action.path), cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::actions::NavWorkspaceBack, window, cx| {
                    this.nav_workspace_back(window, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::actions::NavWorkspaceForward, window, cx| {
                    this.nav_workspace_forward(window, cx);
                },
            ))
            .on_action(cx.listener(|this, _: &crate::actions::ReloadCustomCommands, _window, cx| {
                this.reload_custom_commands(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenWorkspaceCreate, window, cx| {
                let projects = this.app_state.recent_projects.clone();
                let active = this.active_project.clone();
                this.close_modal_overlays(cx);
                this.workspace_dialog
                    .update(cx, |d, cx| d.open_create(projects, active, window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenAddProjectDialog, window, cx| {
                this.close_modal_overlays(cx);
                this.add_project_dialog
                    .update(cx, |d, cx| d.open(window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenProjectPicker, window, cx| {
                if this.project_picker.read(cx).is_open() {
                    this.project_picker.update(cx, |p, cx| p.close(cx));
                    return;
                }
                let projects = this.app_state.recent_projects.clone();
                this.close_modal_overlays(cx);
                this.project_picker
                    .update(cx, |p, cx| p.open(projects, window, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                // Toggle: a second Cmd+, (or cog click) closes it.
                if this.settings_modal.read(cx).is_open() {
                    this.settings_modal.update(cx, |m, cx| m.close(cx));
                    return;
                }
                this.close_modal_overlays(cx);
                this.settings_modal.update(cx, |m, cx| m.open(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleFloatingTerminal, window, cx| {
                this.toggle_floating_terminal(window, cx);
            }))
            .on_action(
                cx.listener(|this, action: &RequestOpenAdapterPicker, window, cx| {
                    // Anchor precedence:
                    //   `Some(px)` → mouse path; popover lands under cursor.
                    //   `None`     → keyboard path; use the post-rail fallback.
                    let fallback_anchor = if this.left_rail_open {
                        this.density.w_left_rail + ADAPTER_PICKER_LEFT_INSET
                    } else {
                        ADAPTER_PICKER_LEFT_INSET
                    };
                    let left_anchor = action.x.unwrap_or(fallback_anchor);
                    // Mutex: only one full-window popover can hold the click-outside path.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.git_row_context_menu.update(cx, |m, cx| m.close(cx));
                    this.adapter_picker
                        .update(cx, |p, cx| p.open(left_anchor, window, cx));
                }),
            )
            .on_action(cx.listener(|this, _: &OpenPaneActions, _window, cx| {
                // Right-edge anchor: matches the "..." button position relative to
                // the right column when sidebar is open / center toggle when closed.
                // Reads the live sidebar width (Phase 13) so the anchor tracks
                // a freshly-dragged sidebar without staring at the old default.
                let (r_open, r_width) = match this.right_sidebar.as_ref() {
                    Some(s) => {
                        let read = s.read(cx);
                        (read.open, f32::from(read.panel_width()))
                    }
                    None => (false, f32::from(DEFAULT_PANEL_WIDTH)),
                };
                let right_anchor = if r_open {
                    r_width
                } else {
                    top_bar::TOGGLE_BUTTON_WIDTH
                };
                let has_siblings = this
                    .active_project_panes()
                    .map(|p| p.read(cx).manager().in_order_groups().len() > 1)
                    .unwrap_or(false);
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                this.pane_actions.update(cx, |p, cx| {
                    p.open(
                        PaneActionsAnchor::TopRight {
                            right_px: right_anchor,
                        },
                        has_siblings,
                        cx,
                    )
                });
            }))
            .on_action(
                cx.listener(|this, action: &OpenPaneActionsAt, _window, cx| {
                    // Per-pane "..." click carries the cursor's absolute window
                    // coords. The menu shifts itself left by its own width so
                    // the card stays inside the right edge when chips sit near
                    // it (see pane_actions::PaneActionsAnchor::Chip).
                    let has_siblings = this
                        .active_project_panes()
                        .map(|p| p.read(cx).manager().in_order_groups().len() > 1)
                        .unwrap_or(false);
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.pane_actions.update(cx, |p, cx| {
                        p.open(
                            PaneActionsAnchor::Chip {
                                x_px: action.x,
                                y_px: action.y,
                            },
                            has_siblings,
                            cx,
                        )
                    });
                }),
            )
            // Four-direction split actions. SplitHorizontal / SplitVertical
            // are aliases preserved for the legacy Cmd+D / Cmd+Shift+D
            // bindings; they map to Right / Down respectively.
            .on_action(cx.listener(|this, action: &ActivateGroupTab, window, cx| {
                // Global workspace strip chip click. The chip's own
                // group already set its inner active tab; here we route
                // workspace focus to the chip's group so the body shows
                // its content.
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                let tab_idx = action.tab_idx as usize;
                panes.update(cx, |p, cx| {
                    p.set_active_group(group_id, window, cx);
                    if let Some(group) = p.group(group_id) {
                        group.update(cx, |g, cx| g.set_active(tab_idx, window, cx));
                    }
                });
            }))
            .on_action(
                cx.listener(|this, action: &OpenTabContextMenuAt, _window, cx| {
                    // Tab chip right-click. Carries enough state (group id,
                    // tab index, click coords) for the shared TabContextMenu
                    // to mutate the right group even if focus moves before
                    // the user picks an item.
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                    let panes_ref = panes.read(cx);
                    let Some(group) = panes_ref.group(group_id) else {
                        return;
                    };
                    let group_ref = group.read(cx);
                    let tab_count = group_ref.tabs().len();
                    let tab_idx = action.tab_idx as usize;
                    let is_pinned = group_ref.is_pinned(tab_idx);
                    // Derive kind-specific payload (editor path) so the menu
                    // can render Copy Path / Reveal in Finder rows without
                    // walking back into the entity at click time.
                    let tab_kind = match group_ref.tabs().get(tab_idx).map(|t| &t.kind) {
                        Some(crate::shell::pane_group::PaneGroupTabKind::Editor { path }) => {
                            let project_root = this
                                .active_project
                                .as_ref()
                                .map(|p| std::path::PathBuf::from(&p.root_path));
                            crate::shell::tab_context_menu::TabContextKind::Editor {
                                path: path.clone(),
                                project_root,
                            }
                        }
                        _ => crate::shell::tab_context_menu::TabContextKind::Terminal,
                    };
                    // Tear-off is available only for single-leaf relay-backed
                    // terminal tabs. Multi-leaf split terminals are excluded
                    // (v1 scope: each leaf would need independent detach+mount
                    // in the destination). Editor and diff tabs are excluded
                    // because their content is window-bound.
                    let can_tear_off = group_ref
                        .tabs()
                        .get(tab_idx)
                        .map(|tab| {
                            if let crate::shell::pane_content::PaneContent::Terminal(tree) =
                                &tab.content
                            {
                                // Relay-backed only — the in-process fallback
                                // backend has no external id.
                                let active_has_external_id = tree
                                    .active_view()
                                    .map(|v| v.read(cx).external_id().is_some())
                                    .unwrap_or(false);
                                tab_can_tear_off(tree, active_has_external_id)
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    let weak = group.downgrade();
                    let x = action.x;
                    let y = action.y;
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| {
                        m.open(
                            x,
                            y,
                            weak,
                            group_id,
                            tab_idx,
                            tab_count,
                            tab_kind,
                            is_pinned,
                            can_tear_off,
                            cx,
                        )
                    });
                }),
            )
            .on_action(
                cx.listener(|this, action: &OpenFileTreeContextMenuAt, _window, cx| {
                    // File-tree row right-click — opens the shared
                    // `FileTreeContextMenu` with the clicked path. Directory
                    // rows get a reduced item set (Reveal + Copy Path only).
                    let path = std::path::PathBuf::from(&action.path);
                    let project_root = this
                        .active_project
                        .as_ref()
                        .map(|p| std::path::PathBuf::from(&p.root_path));
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| {
                        m.open(action.x, action.y, path, project_root, action.is_dir, cx)
                    });
                }),
            )
            .on_action(cx.listener(
                |this, action: &crate::actions::OpenFileTreeBackgroundMenuAt, _window, cx| {
                    // Right-click in the empty area below the file tree —
                    // opens the smaller "New File / New Folder" menu rooted
                    // at the workspace root.
                    let root = std::path::PathBuf::from(&action.root);
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu
                        .update(cx, |m, cx| m.open_background(action.x, action.y, root, cx));
                },
            ))
            .on_action(cx.listener(
                |this, action: &OpenGitRowContextMenuAt, _window, cx| {
                    // Git-row right-click — opens the shared
                    // `GitRowContextMenu` with the right scope variant
                    // (Single / Multi / Folder) derived from the
                    // payload. Close peer overlays first so two
                    // context menus can never share screen real
                    // estate.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));

                    // Resolve the GitPanel weak handle + workdir from
                    // the active right-sidebar. Bail silently when the
                    // sidebar is unmounted (right-click can't have
                    // landed on a non-rendered surface, but defensive).
                    let Some(sc) = this
                        .right_sidebar
                        .as_ref()
                        .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
                    else {
                        return;
                    };
                    let (panel, workdir) = {
                        let sc_ref = sc.read(cx);
                        (
                            sc_ref.git_panel.downgrade(),
                            Some(sc_ref.repo.workdir().to_path_buf()),
                        )
                    };

                    let path = std::path::PathBuf::from(&action.path);
                    let target = if action.is_folder {
                        let leaves: Vec<std::path::PathBuf> = action
                            .folder_leaves
                            .iter()
                            .map(std::path::PathBuf::from)
                            .collect();
                        GitRowContextTarget::Folder {
                            leaves,
                            is_staged_section: action.is_staged,
                            is_untracked_section: action.is_untracked,
                        }
                    } else if !action.selection_paths.is_empty() {
                        // Multi-select right-click. Section flags
                        // ride from the right-clicked row — when the
                        // selection spans sections, the right-clicked
                        // row's section wins for dispatch (cleanest
                        // mental model: user right-clicked from
                        // Staged → action targets Staged).
                        let paths: Vec<std::path::PathBuf> = action
                            .selection_paths
                            .iter()
                            .map(std::path::PathBuf::from)
                            .collect();
                        GitRowContextTarget::Multi {
                            paths,
                            all_staged: action.is_staged,
                            all_untracked: action.is_untracked,
                        }
                    } else {
                        GitRowContextTarget::Single {
                            path,
                            is_staged: action.is_staged,
                        }
                    };

                    this.git_row_context_menu.update(cx, |m, cx| {
                        m.open(action.x, action.y, target, panel, workdir, cx);
                    });
                },
            ))
            .on_action(cx.listener(
                |this, action: &OpenCommitContextMenuAt, _window, cx| {
                    // Commit-graph row right-click — opens
                    // `CommitContextMenu` with the right-clicked
                    // commit's full + short OID. Close peer overlays
                    // first so the menu z-band stays single-occupancy.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker.update(cx, |p, cx| p.close(cx));
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                    this.git_row_context_menu.update(cx, |m, cx| m.close(cx));

                    // Resolve the active source-control panel's
                    // CommitArea weak handle. Bail silently if the
                    // right sidebar isn't mounted (defensive — the
                    // action can't have been dispatched without a
                    // commit-graph row painted, but the lookup chain
                    // is fail-safe).
                    let Some(commit_area_weak) = this
                        .right_sidebar
                        .as_ref()
                        .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
                        .map(|sc| sc.read(cx).commit_area.downgrade())
                    else {
                        return;
                    };

                    this.commit_context_menu.update(cx, |m, cx| {
                        m.open(
                            action.x,
                            action.y,
                            action.sha.clone(),
                            action.short_sha.clone(),
                            commit_area_weak,
                            cx,
                        );
                    });
                },
            ))
            .on_action(
                cx.listener(|this, action: &crate::actions::FindInFolder, window, cx| {
                    // Switch right sidebar to Search and seed its include
                    // glob with `<rel>/**`. Resolution: prefer relative-to-
                    // workspace-root; fall back to the file name.
                    let target = std::path::PathBuf::from(&action.path);
                    let glob = this
                        .active_project
                        .as_ref()
                        .map(|p| std::path::PathBuf::from(&p.root_path))
                        .and_then(|root| {
                            target
                                .strip_prefix(root.as_path())
                                .ok()
                                .map(|r| r.to_path_buf())
                        })
                        .map(|rel| {
                            let s = rel.to_string_lossy().into_owned();
                            if s.is_empty() {
                                String::from("**")
                            } else {
                                format!("{s}/**")
                            }
                        })
                        .unwrap_or_else(|| String::from("**"));
                    this.seed_search_include_and_switch(glob, window, cx);
                }),
            )
            .on_action(cx.listener(
                |_this, action: &crate::actions::OpenInVSCode, _window, _cx| {
                    // `code <path>` requires the VS Code shell integration
                    // (`Shell Command: Install 'code' command in PATH`).
                    // Errors land in tracing — no UI surface because the
                    // user already left the cockpit by choosing this action.
                    let path = std::path::PathBuf::from(&action.path);
                    if let Err(err) = std::process::Command::new("code").arg(&path).spawn() {
                        tracing::warn!(
                            ?err,
                            path = %path.display(),
                            "open in vs code failed (is `code` on PATH?)"
                        );
                    }
                },
            ))
            .on_action(cx.listener(
                |_this, action: &crate::actions::OpenInFinder, _window, _cx| {
                    // `open <dir>` opens Finder at the target. Distinct from
                    // `open -R` (reveal) which opens Finder with the path
                    // selected — used for the workspace-root overflow item.
                    let path = std::path::PathBuf::from(&action.path);
                    if let Err(err) = std::process::Command::new("open").arg(&path).spawn() {
                        tracing::warn!(?err, path = %path.display(), "open in finder failed");
                    }
                },
            ))
            .on_action(cx.listener(
                |_this, action: &crate::actions::FileTreeNewFile, _window, _cx| {
                    // Phase 02 stub: logs the dispatch. Phase 03 wires the
                    // inline-input row in the file tree.
                    tracing::info!(
                        target: "oximux_app::file_explorer",
                        parent = %action.parent,
                        "FileTreeNewFile dispatched (inline input lands in Phase 03)"
                    );
                },
            ))
            .on_action(cx.listener(
                |_this, action: &crate::actions::FileTreeNewFolder, _window, _cx| {
                    tracing::info!(
                        target: "oximux_app::file_explorer",
                        parent = %action.parent,
                        "FileTreeNewFolder dispatched (inline input lands in Phase 03)"
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeRename, window, cx| {
                    this.start_inline_file_rename(
                        std::path::PathBuf::from(&action.path),
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeDelete, window, cx| {
                    this.mount_file_delete_confirm(
                        std::path::PathBuf::from(&action.path),
                        window,
                        cx,
                    );
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::FileTreeDuplicate, _window, cx| {
                    this.duplicate_file_entry(std::path::PathBuf::from(&action.path), cx);
                },
            ))
            .on_action(
                cx.listener(|this, action: &OpenFileFromContextMenu, window, cx| {
                    // File-tree menu "Open" / "Open to the Side" row. The
                    // menu has already closed itself; this handler routes
                    // to ProjectPanes via the same code paths the drag-drop
                    // flow uses (open_file_in_group / split_and_open_file).
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let path = std::path::PathBuf::from(&action.path);
                    if !is_openable_text_file(&path) {
                        tracing::info!(
                            file = %path.display(),
                            "open-from-context-menu: refusing non-text file"
                        );
                        return;
                    }
                    let split_right = action.split_right;
                    panes.update(cx, |p, cx| {
                        let target = p.manager().active_group_id();
                        if split_right {
                            p.split_and_open_file(
                                target,
                                crate::shell::pane_group::tab_drag_zones::Zone::Right,
                                path,
                                window,
                                cx,
                            );
                        } else {
                            p.open_file_in_group(target, path, window, cx);
                        }
                    });
                }),
            )
            .on_action(cx.listener(
                |this, action: &crate::actions::RequestRenameTabAt, window, cx| {
                    // Tab right-click "Change Title…": open a RenameTabDialog
                    // bound to (group_id, tab_idx). Callback mutates the
                    // target group's custom_title via set_tab_title.
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                    let tab_idx = action.tab_idx as usize;
                    let panes_ref = panes.read(cx);
                    let Some(group) = panes_ref.group(group_id) else {
                        return;
                    };
                    let initial = group.read(cx).visible_title(tab_idx).unwrap_or_default();
                    let weak_root: gpui::WeakEntity<WorkspaceRoot> = cx.weak_entity();
                    let weak_group = group.downgrade();
                    let on_commit: crate::shell::rename_tab_dialog::RenameCallback =
                        std::rc::Rc::new(move |outcome, _window, cx| {
                            use crate::shell::rename_tab_dialog::RenameOutcome;
                            // Mutate first, then drop the dialog regardless of
                            // outcome so Cancel actually dismisses the modal.
                            if let Some(g) = weak_group.upgrade() {
                                match outcome {
                                    RenameOutcome::Save(value) => g.update(cx, |g, cx| {
                                        g.set_tab_title(tab_idx, Some(value), cx)
                                    }),
                                    RenameOutcome::Reset => {
                                        g.update(cx, |g, cx| g.set_tab_title(tab_idx, None, cx))
                                    }
                                    RenameOutcome::Cancel => {}
                                }
                            }
                            let _ = weak_root.update(cx, |this, cx| {
                                this.rename_tab_dialog = None;
                                cx.notify();
                            });
                        });
                    let theme = this.theme;
                    let density = this.density;
                    let typography = this.typography.clone();
                    this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                    let dialog = cx.new(|cx| {
                        crate::shell::rename_tab_dialog::RenameTabDialog::new(
                            "Change Tab Title".into(),
                            initial,
                            on_commit,
                            theme,
                            density,
                            typography,
                            window,
                            cx,
                        )
                    });
                    // Focus the dialog's input AFTER it's mounted so the user
                    // can type immediately. Focusing before assignment is a
                    // no-op (the element isn't in the tree yet).
                    dialog.read(cx).input_focus_handle(cx).focus(window, cx);
                    this.rename_tab_dialog = Some(dialog);
                    cx.notify();
                },
            ))
            .on_action(cx.listener(
                |this, action: &crate::actions::TogglePinTabAt, _window, cx| {
                    // Tab right-click "Pin Tab" / "Unpin Tab": flip the
                    // pinned flag and re-cluster the chip inside the
                    // group's tab_order. Reading the live `pinned` from
                    // the group at dispatch time keeps the menu's stale
                    // snapshot from producing a wrong toggle (e.g. two
                    // rapid clicks).
                    let Some(panes) = this.active_project_panes() else {
                        return;
                    };
                    let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                    let tab_idx = action.tab_idx as usize;
                    let panes_ref = panes.read(cx);
                    let Some(group) = panes_ref.group(group_id) else {
                        return;
                    };
                    let group = group.clone();
                    group.update(cx, |g, cx| g.toggle_pin(tab_idx, cx));
                },
            ))
            .on_action(
                cx.listener(|this, action: &MoveTabToNewWindow, window, cx| {
                    this.handle_move_tab_to_new_window(action.group_id, action.tab_idx, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &SplitHorizontal, window, cx| {
                this.split_active_pane_group(Axis::Horizontal, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitVertical, window, cx| {
                this.split_active_pane_group(Axis::Vertical, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split_active_pane_group(Axis::Horizontal, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split_active_pane_group(Axis::Vertical, SplitInsert::After, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitLeft, window, cx| {
                this.split_active_pane_group(Axis::Horizontal, SplitInsert::Before, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitUp, window, cx| {
                this.split_active_pane_group(Axis::Vertical, SplitInsert::Before, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApplyLayoutStacked, window, cx| {
                use crate::shell::pane_group::layout_presets::Preset;
                this.reshape_active_project_layout(Preset::Stacked, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApplyLayoutHorizontal, window, cx| {
                use crate::shell::pane_group::layout_presets::Preset;
                this.reshape_active_project_layout(Preset::Horizontal, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ApplyLayoutBottomTerminal, window, cx| {
                use crate::shell::pane_group::layout_presets::Preset;
                this.reshape_active_project_layout(Preset::BottomTerminal, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseGroup, window, cx| {
                this.close_active_pane_group(window, cx);
            }))
            // Fallback CloseTab handler. The primary listener lives on each
            // `PaneGroup`'s root div, but on the FIRST FRAME after a new tab
            // is created (e.g. opening a file from the explorer) the new
            // view's focus_handle isn't yet in the rendered dispatch tree —
            // `window.focus_node_id_in_rendered_frame` falls back to root,
            // skipping the PaneGroup's listener. Routing Cmd+W through the
            // active group from the root anchor closes the race. When the
            // dispatch tree IS in the right state the PaneGroup listener
            // catches CloseTab first and stops propagation, so this
            // fallback only fires when actually needed (no double-close).
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.on_close_tab(&CloseTab, window, cx));
                    }
                });
            }))
            // Root-level fallback for NewTab. Like CloseTab above, the primary
            // listener lives on the active PaneGroup, which is NOT an ancestor
            // of the focused node when a full-window overlay (command palette)
            // holds focus. Dispatching NewTab from the palette would otherwise
            // reach no handler. Routing through the active group from the root
            // anchor makes the palette "New Tab" entry work; when a pane is
            // focused the PaneGroup listener catches it first and stops
            // propagation, so this fallback only fires when needed.
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.on_new_tab(&NewTab, window, cx));
                    }
                });
            }))
            // Root-level handler for NewBrowserTab (⌘⇧B) — routes to the
            // active group like the NewTab fallback so the keybind works
            // regardless of which surface holds focus.
            .on_action(cx.listener(|this, _: &NewBrowserTab, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.on_new_browser_tab(&NewBrowserTab, window, cx));
                    }
                });
            }))
            // Root-level fallback for Search (scrollback search overlay). The
            // primary listener lives on the focused TerminalView, which is not
            // on the dispatch path when the command palette holds focus. Route
            // to the active group's active terminal so the palette "Search
            // Pane" entry opens the overlay; a focused terminal consumes the
            // action first when no overlay is up.
            .on_action(cx.listener(|this, action: &Search, window, cx| {
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                panes.update(cx, |p, cx| {
                    if let Some(group) = p.active_group() {
                        group.update(cx, |g, cx| g.open_search_active_terminal(action, window, cx));
                    }
                });
            }))
            .on_action(cx.listener(|this, action: &SplitGroupAt, window, cx| {
                // Tab right-click "Split X" → target a SPECIFIC group
                // (the right-clicked one), not the focused one. We
                // first activate that group so the split lands on it,
                // then perform the directional split. Matches the
                // per-tab Split menu behavior of the design reference.
                let Some(panes) = this.active_project_panes() else {
                    return;
                };
                let group_id = crate::shell::pane_tree::PaneGroupId(action.group_id);
                let axis = if action.axis == 0 {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                let insert = if action.insert_before {
                    SplitInsert::Before
                } else {
                    SplitInsert::After
                };
                panes.update(cx, |p, cx| {
                    p.set_active_group(group_id, window, cx);
                    p.split_active_group(axis, insert, window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &DismissOverlay, window, cx| {
                // An in-flight drag takes priority: Escape cancels it (clears
                // the active drag, no drop side-effect) and consumes the key.
                // Only when no drag is active does Escape fall through to
                // overlay dismissal, preserving its existing behaviour.
                if cx.stop_active_drag(window) {
                    return;
                }
                // Close every transient overlay so a single Escape dismisses
                // whichever popover is currently visible. Modal dialogs
                // (project picker / workspace create / palette) own their
                // own focus and currently ignore this.
                //
                // When NOTHING is open, propagate instead of consuming: a
                // matched key binding swallows the keystroke before key
                // listeners ever run, so a no-op here would eat every
                // Escape a focused terminal needs — for its PTY (TUIs, the
                // agent CLI's panels, vim) and for the search overlay.
                let any_open = this.pane_actions.read(cx).is_open()
                    || this.tab_context_menu.read(cx).is_open()
                    || this.file_tree_context_menu.read(cx).is_open()
                    || this.git_row_context_menu.read(cx).is_open()
                    || this.commit_context_menu.read(cx).is_open()
                    || this.adapter_picker.read(cx).is_open()
                    || this.row_menu.read(cx).is_open()
                    || this.project_menu.read(cx).is_open()
                    || this.usage_popover_open;
                if !any_open {
                    cx.propagate();
                    return;
                }
                this.pane_actions.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                this.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
                this.git_row_context_menu.update(cx, |m, cx| m.close(cx));
                this.commit_context_menu.update(cx, |m, cx| m.close(cx));
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.row_menu.update(cx, |m, cx| m.close(cx));
                this.project_menu.update(cx, |m, cx| m.close(cx));
                if this.usage_popover_open {
                    this.usage_popover_open = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleRightSidebar, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.toggle(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectFilesTab, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.select_tab(RightTab::Files, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectExplorerTab, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.select_tab(RightTab::Explorer, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SelectSearchTab, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.select_tab(RightTab::Search, cx));
                }
            }))
            .on_action(
                cx.listener(|this, _: &SelectSourceControlTab, _window, cx| {
                    if let Some(rs) = &this.right_sidebar {
                        rs.update(cx, |s, cx| s.select_tab(RightTab::SourceControl, cx));
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &OpenCommitDialog, window, cx| {
                // Cmd+K: jump to Source Control tab and focus the commit input.
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| {
                        s.select_tab(RightTab::SourceControl, cx);
                        if !s.open {
                            s.toggle(cx);
                        }
                        s.focus_commit_subject(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &crate::actions::RefreshSourceControl, _window, cx| {
                // Cmd+R: refresh the commit graph's first page against
                // the active worktree. Same call site as the header-strip
                // refresh button; routes through the workspace so the
                // chord works from any focused pane in the window. Silent
                // no-op when the right sidebar is closed or the SCM tab
                // isn't mounted — the alternative (popping the sidebar
                // open on every Cmd+R) would surprise users who bound the
                // chord to "refresh this window" muscle memory.
                let Some(rs) = &this.right_sidebar else {
                    return;
                };
                let Some(sc) = rs.read(cx).source_control.as_ref().cloned() else {
                    return;
                };
                sc.update(cx, |panel, cx| {
                    panel.commit_graph.update(cx, |g, cx| g.refresh(cx));
                });
            }))
            .child(row)
            .child({
                // Fetch the SCM panel's cached primary action so the status
                // bar renders the same resolved verb — no second resolver.
                let scm_panel = self.right_sidebar.as_ref().and_then(|rs| {
                    rs.read(cx).source_control.clone()
                });
                let primary = scm_panel
                    .as_ref()
                    .and_then(|sc| sc.read(cx).last_primary_action());
                // Clone for the closure; the outer `window` is plumbed via
                // `update` (not `update_in`) per the GPUI memory note.
                let scm_for_click = scm_panel.clone();
                let weak_for_usage = cx.entity().downgrade();
                status_bar::view(
                    theme,
                    density,
                    typography,
                    pane_count,
                    tty_count,
                    agent_count,
                    git_state.as_ref(),
                    primary,
                    self.usage_state.as_ref(),
                    move |window, cx| {
                        if let Some(sc) = scm_for_click.clone() {
                            sc.update(cx, |panel, cx| {
                                panel.trigger_primary_action(window, cx);
                            });
                        }
                    },
                    move |window, cx| {
                        WorkspaceRoot::toggle_usage_popover(&weak_for_usage, window, cx);
                    },
                )
            })
            // Usage-meter popover — anchored above the status bar's right
            // corner. The transparent full-window backdrop closes it on any
            // outside click; z-band above the floating terminal, below the
            // palette overlays that follow.
            .when(self.usage_popover_open, |parent| {
                let Some(state) = self.usage_state.as_ref() else {
                    return parent;
                };
                let weak_close = cx.entity().downgrade();
                let card = crate::shell::usage_meter::render_usage_popover(
                    state,
                    oximux_agents::session_log::now_unix_ms(),
                    theme,
                    density,
                    typography,
                );
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            move |_ev, _window, cx| {
                                let _ = weak_close.update(cx, |this, cx| {
                                    this.usage_popover_open = false;
                                    cx.notify();
                                });
                            },
                        )
                        .child(
                            div()
                                .absolute()
                                .right(px(8.0))
                                .bottom(px(density.h_status_bar + 6.0))
                                // Clicks on the card must not bubble to the
                                // backdrop's dismiss handler — the user may
                                // click while reading the numbers.
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    |_ev, _window, cx| cx.stop_propagation(),
                                )
                                .child(card),
                        ),
                )
            })
            // Floating ("PiP") terminal — sits above the workspace panels but
            // below every popover / modal that follows. Retained across hides
            // (the PTY persists); only rendered while `*_visible` is set.
            .children(
                self.floating_terminal_visible
                    .then(|| self.floating_terminal.clone())
                    .flatten(),
            )
            // Pane Actions dropdown — appended before the palette so the
            // palette (more rare, larger) wins z-order when both are open.
            .child(self.pane_actions.clone())
            // Tab right-click context menu — same z-band as pane_actions.
            // Mutually exclusive with it via close-on-open logic in the
            // OpenPaneActionsAt / OpenTabContextMenuAt action handlers.
            .child(self.tab_context_menu.clone())
            // File-tree right-click context menu — same z-band; mutually
            // exclusive via close-on-open in OpenFileTreeContextMenuAt.
            .child(self.file_tree_context_menu.clone())
            // Git-row right-click context menu — same z-band; mutually
            // exclusive via close-on-open in OpenGitRowContextMenuAt.
            .child(self.git_row_context_menu.clone())
            // Commit-graph row right-click menu — same z-band as the
            // peer context menus; mutually exclusive via close-on-open
            // in OpenCommitContextMenuAt.
            .child(self.commit_context_menu.clone())
            // Adapter picker — same z-band as pane_actions; only one of
            // them can be open at a time so order between them is moot.
            .child(self.adapter_picker.clone())
            // Project picker (Cmd+O). Below the palette so an
            // accidentally-opened palette during picker use wins z-order;
            // the action handlers also close conflicting overlays.
            .child(self.project_picker.clone())
            // Workspace dialog (Cmd+Shift+N create + sidebar rename).
            .child(self.workspace_dialog.clone())
            // Per-row action popover (sidebar Rename / Archive / Delete).
            .child(self.row_menu.clone())
            // Per-project-header action popover (Reveal / Copy / Remove).
            .child(self.project_menu.clone())
            .child(self.add_project_dialog.clone())
            // Type-to-confirm dialog for destructive workspace ops. Built
            // per-request; `None` when idle. Wrapped in a full-window
            // overlay here so the inner `ConfirmDialog` card stays pure.
            .when_some(self.confirm_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Rename-tab modal — same overlay pattern as confirm_dialog.
            .when_some(self.rename_tab_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Push-stash form modal — same overlay pattern.
            .when_some(self.push_stash_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Palette modal — appended above the rest of the chrome.
            .child(self.palette.clone())
            // Settings modal — appended last so it paints above all other
            // children (last child = topmost z-layer in GPUI).
            .child(self.settings_modal.clone())
            // Toasts paint above even the modals so a transient event (commit
            // failed, agent done) is never hidden behind an open dialog. The
            // layer is non-interactive and bottom-right, so it doesn't steal
            // clicks from whatever is beneath it.
            .child(self.toast_layer.clone())
            // gpui-component notification layer. `Root::render` does not mount
            // it automatically, so leaf views that call `push_notification`
            // (e.g. the editor breadcrumb's copy/reveal actions) need it here
            // or their toasts never paint.
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}

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
