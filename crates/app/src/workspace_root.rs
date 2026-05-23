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

use gpui::{
    AnyElement, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window, div,
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

use crate::notifier::{Notifier, TabId};
use crate::state::AppState;

use crate::actions::{
    ActivateGroupTab, CloseGroup, DismissOverlay, OpenAddProjectDialog, OpenCommandPalette,
    OpenCommitDialog, OpenPaneActions, OpenPaneActionsAt, OpenProjectPicker, OpenQuickOpen,
    OpenTabContextMenuAt, OpenWorkspaceCreate, RequestOpenAdapterPicker, SelectExplorerTab,
    SelectFilesTab, SelectSearchTab, SelectSourceControlTab, SplitDown, SplitGroupAt,
    SplitHorizontal, SplitLeft, SplitRight, SplitUp, SplitVertical, ToggleLeftSidebar,
    ToggleRightSidebar,
};
use crate::shell::pane_tree::{Axis, SplitInsert};
use crate::shell::{
    adapter_picker::{AdapterPicker, AdapterSelection, OnSelect},
    add_project_dialog::AddProjectDialog,
    command_palette::{PaletteModal, entry::PaletteMode},
    confirm_dialog::ConfirmDialog,
    left_rail::{LeftRail, row_menu::WorkspaceRowMenu},
    main_area,
    openable_text_file::is_openable_text_file,
    pane_actions::{PaneActionsAnchor, PaneActionsMenu},
    tab_context_menu::TabContextMenu,
    project_picker::{OnPick, ProjectPickerModal},
    right_sidebar::{
        RightSidebar, activity_bar::render_tab_buttons, layout::DEFAULT_PANEL_WIDTH, tab::RightTab,
    },
    status_bar,
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS},
    top_bar,
    project_panes::ProjectPanes,
    workspace_dialog::{OnSubmit as OnWorkspaceSubmit, WorkspaceDialog},
    workspace_ops::build_add_project_dialog,
};

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
    pub(crate) right_sidebar: Option<Entity<RightSidebar>>,
    pub(crate) left_rail: Entity<LeftRail>,
    pub(crate) palette: Entity<PaletteModal>,
    pub(crate) pane_actions: Entity<PaneActionsMenu>,
    pub(crate) tab_context_menu: Entity<TabContextMenu>,
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
    /// Workspace create / rename dialog (Cmd+Shift+N + sidebar rename).
    pub(crate) workspace_dialog: Entity<WorkspaceDialog>,
    /// Active type-to-confirm dialog (per-request; `None` when idle).
    pub(crate) confirm_dialog: Option<Entity<ConfirmDialog>>,
    /// Active rename-tab modal (per-request; `None` when idle).
    pub(crate) rename_tab_dialog: Option<Entity<crate::shell::rename_tab_dialog::RenameTabDialog>>,
    /// Currently active project — `None` until the user opens one.
    pub(crate) active_project: Option<Project>,
    /// Sidebar Rename/Archive/Delete popover (mounted at root for full-window backdrop).
    pub(crate) row_menu: Entity<WorkspaceRowMenu>,
    pub(crate) add_project_dialog: Entity<AddProjectDialog>,
    /// Render root tracks this so action dispatch reaches the workspace
    /// even when no pane is focused (sidebar toggle, command palette).
    pub(crate) focus_handle: FocusHandle,
}

impl WorkspaceRoot {
    pub fn new(
        repo: Option<Repository>,
        app_state: AppState,
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
        let cli_runtime = Arc::new(CliRuntime::new());
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
        let (click_tx, mut click_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        #[cfg(target_os = "macos")]
        let notifier: Arc<dyn Notifier> =
            Arc::new(crate::notifier::mac::MacNotifier::new(click_tx));
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
        let project_panes_observer: Option<Subscription> = None;
        // Shared weak self-handle: LeftRail + picker callbacks route through it.
        // Built before the right-sidebar so the Files-tab `OnOpenFile` callback
        // can capture it and route clicks back to `open_file_in_active_pane`.
        let weak_self: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let right_sidebar = repo.clone().map(|r| {
            let root_path = r.workdir().to_path_buf();
            let on_open = Self::build_on_open_file_callback(weak_self.clone());
            let on_query = Self::build_on_query_active_path_callback(weak_self.clone());
            cx.new(|cx| {
                RightSidebar::new(
                    Some(r),
                    root_path,
                    false, // default-collapsed on app boot
                    Some(on_open),
                    Some(on_query),
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
            })
        });
        let left_rail = cx.new(|cx| LeftRail::new(weak_self.clone(), cx));
        let palette = cx.new(|_| PaletteModal::new(theme, density, typography.clone()));
        let pane_actions = cx.new(|_| PaneActionsMenu::new(theme, density, typography.clone()));
        let tab_context_menu =
            cx.new(|_| TabContextMenu::new(theme, density, typography.clone()));
        let on_select: OnSelect = Box::new(move |selection, window, cx| {
            let weak = weak_self.clone();
            let _ = weak.update(cx, |this, cx| match selection {
                AdapterSelection::NewTerminal => this.spawn_local_terminal_tab(window, cx),
                AdapterSelection::Adapter { kind, id } => {
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    this.spawn_agent_tab(kind, id, cwd, window, cx)
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
        let pr = app_state.project_repo.clone();
        let add_project_dialog =
            build_add_project_dialog(theme, density, typography.clone(), pr, cx);

        // Pause status polling when the window blurs; force an immediate
        // refresh on focus regain via StatusPoller::kick().
        let window_activation_observer =
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |sidebar, _cx| sidebar.set_polling_focused(active));
                }
            });

        // Click router: drains tab-ids posted by the macOS click watcher.
        // For each id, raise the window and activate the matching tab.
        // Closure ends when the mpsc receiver returns None (all senders
        // dropped, e.g. at app shutdown) or when the entity is gone.
        let click_router = cx.spawn_in(window, async move |weak, cx| {
            while let Some(tab_id_raw) = click_rx.recv().await {
                let tab_id = TabId(tab_id_raw);
                // Raise the window only when the tab still exists. A
                // notification for a since-closed agent (M1 review-260521):
                // popping the window with no destination would be a
                // disruptive UX on a stale click.
                if weak
                    .update_in(cx, |root, window, cx| {
                        let activated = root.active_project_panes().is_some_and(|panes_entity| {
                            panes_entity.update(cx, |panes, cx| {
                                panes.set_active_by_tab_id(tab_id, window, cx)
                            })
                        });
                        if activated {
                            window.activate_window();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        Self {
            theme,
            density,
            typography,
            project_panes_by_project,
            notifier: notifier.clone(),
            right_sidebar,
            left_rail,
            palette,
            pane_actions,
            tab_context_menu,
            adapter_picker,
            cli_runtime,
            adapter_registry,
            left_rail_open: true,
            _project_panes_observer: project_panes_observer,
            _window_activation_observer: window_activation_observer,
            _click_router: click_router,
            app_state,
            project_picker,
            workspace_dialog,
            confirm_dialog: None,
            rename_tab_dialog: None,
            active_project: None,
            row_menu,
            add_project_dialog,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Open a fresh local-PTY tab in the active project's active pane group.
    fn spawn_local_terminal_tab(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| p.open_terminal_tab_in_active_group(window, cx));
    }

    /// Resolves the currently-visible `ProjectPanes` entity by reading
    /// `active_project.id` against the per-project map. `None` when no
    /// project is active (welcome state) or when the project has no entity
    /// yet (mid-`set_active_project`).
    pub(crate) fn active_project_panes(&self) -> Option<Entity<ProjectPanes>> {
        let id = self.active_project.as_ref().map(|p| p.id.as_str())?;
        self.project_panes_by_project.get(id).cloned()
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
            p.open_or_activate_editor_tab(path, window, cx);
        });
    }

    /// Build the on-click callback handed to the Files-tab `FileTreeView`.
    /// The closure captures a weak self-handle so the callback survives
    /// project switches that rebuild `RightSidebar`. A dropped weak handle
    /// (window closed) silently no-ops the click.
    pub(crate) fn build_on_open_file_callback(
        weak: WeakEntity<Self>,
    ) -> crate::shell::file_tree_view::OnOpenFile {
        Arc::new(move |path, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.open_file_in_active_pane(path, window, cx);
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
        for (project_id, panes) in &self.project_panes_by_project {
            panes.read(cx).capture_pane_buffers(
                &repo,
                project_id,
                crate::project_panes_factory::PANE_BUFFER_MAX_BYTES,
                cx,
            );
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
        let repo = self.app_state.pane_relay_id_repo.clone();
        for (project_id, panes) in &self.project_panes_by_project {
            panes
                .read(cx)
                .capture_pane_relay_ids(&repo, project_id, &session_id, cx);
        }
    }

    /// Spawn the chosen agent in a new tab inside the active pane group.
    /// Runs the start_session → backend_for → terminal_session_id →
    /// subscribe_status chain, then hands the assembled handles to
    /// `ProjectPanes::push_agent_tab`.
    ///
    /// If `update_in` errors (window/workspace dropped mid-spawn), cancels
    /// the half-mounted session so the PTY doesn't zombie.
    pub(crate) fn spawn_agent_tab(
        &self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        cwd: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        let runtime = self.cli_runtime.clone();
        let cwd_for_tab = cwd.clone();
        let model: Option<String> = None;
        let effort: Option<String> = None;

        cx.spawn_in(window, async move |_root, cx| {
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
                env: Vec::new(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                custom_command: None,
            };
            let session_id = match runtime.start_session(cfg).await {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, adapter = adapter_id, "start_session failed");
                    return;
                }
            };
            let backend = match runtime.backend_for(session_id) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(?err, "backend_for after start_session");
                    let _ = runtime.cancel(session_id).await;
                    return;
                }
            };
            let term_id = match runtime.terminal_session_id(session_id) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, "terminal_session_id after start_session");
                    let _ = runtime.cancel(session_id).await;
                    return;
                }
            };
            let status_rx = match runtime.subscribe_status(session_id) {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(?err, "subscribe_status after start_session");
                    let _ = runtime.cancel(session_id).await;
                    return;
                }
            };

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
}

// `build_project_panes` + restore helpers live in
// `crate::project_panes_factory` so this file stays under the 800-LOC cap.
// `impl Focusable for WorkspaceRoot` lives in `shell::workspace_ops` for
// the same reason.

impl Render for WorkspaceRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Push sidebar data down before LeftRail::render runs in the tree.
        self.refresh_left_rail(cx);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        // Status-bar pane count = visible pane-group leaves in the active
        // project (1 when no splits, N after Cmd+D).
        let active_panes = self.active_project_panes();
        let pane_count = active_panes
            .as_ref()
            .map(|p| p.read(cx).manager().in_order_groups().len())
            .unwrap_or(0);

        // Aggregate open-agent-tab count across every group in the active
        // project. Same semantics as before — users care about total
        // agents in flight, not just the foreground group's.
        let agent_count = active_panes
            .as_ref()
            .map(|p| p.read(cx).agent_count(cx))
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
        let left_chrome = if self.left_rail_open {
            density.w_left_rail
        } else {
            0.0
        };
        let right_chrome = if right_open {
            f32::from(DEFAULT_PANEL_WIDTH)
        } else {
            0.0
        };
        if let Some(panes) = active_panes.as_ref() {
            let chrome = left_chrome + right_chrome;
            panes.update(cx, |p, cx| p.set_chrome_width(chrome, cx));
        }

        // Top-row pane groups hoist their per-group strips INTO the
        // top-bar row (mirroring tree column widths). Lower vertical-
        // split rows render their strips inline above their bodies.
        let workspace_tab_strip: Option<AnyElement> = active_panes
            .as_ref()
            .and_then(|panes| panes.read(cx).topmost_tab_strip(cx));

        // Center column body: active project's ProjectPanes (which renders
        // its group tree internally), or welcome placeholder when no
        // project is active.
        let center_body: AnyElement = match active_panes.clone() {
            Some(view) => view.into_any_element(),
            None => main_area::view(theme, density, typography).into_any_element(),
        };

        // Per-column composition. Each column owns its own 40px header on top
        // of its body so the tab strip cannot extend across the side panels.
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

        // Activity tabs are owned by exactly one header: when the sidebar is
        // open, they live in the right column's header; when closed,
        // `right_tabs` is already `None` and the center header just appends
        // the right-toggle button.
        let (center_right_tabs, right_column_tabs) = if right_open {
            (None, right_tabs)
        } else {
            (right_tabs, None)
        };

        let center_column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .child(top_bar::center_header(
                self.left_rail_open,
                right_open,
                workspace_tab_strip,
                center_right_tabs,
                theme,
                density,
                typography,
            ))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .child(center_body),
            );

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

        // Reserved drag handle that sits under the macOS title bar
        // overlay region (FullSizeContentView extends the content view
        // up under the system title bar). Without a child here the OS
        // intercepts clicks on the topmost 28px as a title-bar drag,
        // which would hijack chip drag-reorder gestures originating in
        // the tab strip. Sized at 22px to leave the traffic-light
        // glyphs visible at point(12, 12) without pushing the chrome
        // row noticeably down. Background matches the panel chrome so
        // the strip below reads as one continuous header.
        const MAC_TITLEBAR_SAFE_PX: f32 = 22.0;
        let titlebar_spacer = div()
            .h(px(MAC_TITLEBAR_SAFE_PX))
            .w_full()
            .flex_shrink_0()
            .bg(theme.bg_panel);

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            .child(titlebar_spacer)
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _window, cx| {
                this.left_rail_open = !this.left_rail_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenQuickOpen, _window, cx| {
                // Mutex with every other full-window overlay (close-then-open).
                this.close_modal_overlays(cx);
                this.palette
                    .update(cx, |p, cx| p.open(PaletteMode::QuickOpen, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, _window, cx| {
                this.close_modal_overlays(cx);
                this.palette
                    .update(cx, |p, cx| p.open(PaletteMode::Commands, cx));
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
            .on_action(
                cx.listener(|this, _: &RequestOpenAdapterPicker, window, cx| {
                    // Anchor under the `+` button; the click handler stashes
                    // event.position.x. Keyboard path falls back to a post-rail inset.
                    let fallback_anchor = if this.left_rail_open {
                        this.density.w_left_rail + ADAPTER_PICKER_LEFT_INSET
                    } else {
                        ADAPTER_PICKER_LEFT_INSET
                    };
                    let left_anchor = this
                        .active_project_panes()
                        .and_then(|panes| panes.read(cx).take_plus_click_x())
                        .unwrap_or(fallback_anchor);
                    // Mutex: only one full-window popover can hold the click-outside path.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker
                        .update(cx, |p, cx| p.open(left_anchor, window, cx));
                }),
            )
            .on_action(cx.listener(|this, _: &OpenPaneActions, _window, cx| {
                // Right-edge anchor: matches the "..." button position relative to
                // the right column when sidebar is open / center toggle when closed.
                let r_open = this.right_sidebar.as_ref().is_some_and(|s| s.read(cx).open);
                let right_anchor = if r_open {
                    f32::from(DEFAULT_PANEL_WIDTH)
                } else {
                    top_bar::TOGGLE_BUTTON_WIDTH
                };
                let has_siblings = this
                    .active_project_panes()
                    .map(|p| p.read(cx).manager().in_order_groups().len() > 1)
                    .unwrap_or(false);
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| m.close(cx));
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
            .on_action(cx.listener(|this, action: &OpenPaneActionsAt, _window, cx| {
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
            }))
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
            .on_action(cx.listener(|this, action: &OpenTabContextMenuAt, _window, cx| {
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
                let weak = group.downgrade();
                let x = action.x;
                let y = action.y;
                this.pane_actions.update(cx, |p, cx| p.close(cx));
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| {
                    m.open(x, y, weak, group_id, tab_idx, tab_count, tab_kind, cx)
                });
            }))
            .on_action(cx.listener(|this, action: &crate::actions::RequestRenameTabAt, window, cx| {
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
                let initial = group
                    .read(cx)
                    .visible_title(tab_idx)
                    .unwrap_or_default();
                let weak_root: gpui::WeakEntity<WorkspaceRoot> = cx.weak_entity();
                let weak_group = group.downgrade();
                let on_commit: crate::shell::rename_tab_dialog::RenameCallback =
                    std::rc::Rc::new(move |new_title, _window, cx| {
                        if let Some(g) = weak_group.upgrade() {
                            g.update(cx, |g, cx| g.set_tab_title(tab_idx, new_title, cx));
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
                this.rename_tab_dialog = Some(cx.new(|cx| {
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
                }));
                cx.notify();
            }))
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
            .on_action(cx.listener(|this, _: &CloseGroup, window, cx| {
                this.close_active_pane_group(window, cx);
            }))
            .on_action(cx.listener(|this, action: &SplitGroupAt, window, cx| {
                // Tab right-click "Split X" → target a SPECIFIC group
                // (the right-clicked one), not the focused one. We
                // first activate that group so the split lands on it,
                // then perform the directional split. Matches the reference UX's
                // per-tab Split menu behavior.
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
            .on_action(cx.listener(|this, _: &DismissOverlay, _window, cx| {
                // Close every transient overlay so a single Escape dismisses
                // whichever popover is currently visible. Modal dialogs
                // (project picker / workspace create / palette) own their
                // own focus and currently ignore this.
                this.pane_actions.update(cx, |p, cx| p.close(cx));
                this.tab_context_menu.update(cx, |m, cx| m.close(cx));
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
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
            .child(row)
            .child(status_bar::view(
                theme,
                density,
                typography,
                pane_count,
                tty_count,
                agent_count,
                git_state.as_ref(),
            ))
            // Pane Actions dropdown — appended before the palette so the
            // palette (more rare, larger) wins z-order when both are open.
            .child(self.pane_actions.clone())
            // Tab right-click context menu — same z-band as pane_actions.
            // Mutually exclusive with it via close-on-open logic in the
            // OpenPaneActionsAt / OpenTabContextMenuAt action handlers.
            .child(self.tab_context_menu.clone())
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
            .child(self.add_project_dialog.clone())
            // Type-to-confirm dialog for destructive workspace ops. Built
            // per-request; `None` when idle. Wrapped in a full-window
            // overlay here so the inner `ConfirmDialog` card stays pure.
            .when_some(self.confirm_dialog.clone(), |parent, dialog| {
                parent.child(
                    div()
                        .absolute()
                        .inset_0()
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
                        .flex()
                        .flex_col()
                        .items_center()
                        .pt(px(96.0))
                        .child(dialog),
                )
            })
            // Palette modal — appended last so it paints above all other
            // children (last child = topmost z-layer in GPUI).
            .child(self.palette.clone())
    }
}
