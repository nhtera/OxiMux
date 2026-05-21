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
//! │ left rail    │ active tab's MainPane       │ right sidebar    │  ← flex_1
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

use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Subscription, Task, WeakEntity, Window, div, px,
};
use oximux_agents::{AdapterRegistry, AgentRuntime, AgentSessionConfig, CliRuntime};
use oximux_core::AgentAdapter;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};

use crate::notifier::{Notifier, TabId};
use crate::state::AppState;

use crate::actions::{
    OpenCommandPalette, OpenCommitDialog, OpenPaneActions, OpenQuickOpen, RequestOpenAdapterPicker,
    SelectExplorerTab, SelectSearchTab, SelectSourceControlTab, ToggleLeftSidebar,
    ToggleRightSidebar,
};
use crate::shell::{
    adapter_picker::{AdapterPicker, AdapterSelection, OnSelect},
    command_palette::{PaletteModal, entry::PaletteMode},
    left_rail::LeftRail,
    main_area,
    main_pane::MainPane,
    pane_actions::PaneActionsMenu,
    right_sidebar::{
        RightSidebar, activity_bar::render_tab_buttons, layout::DEFAULT_PANEL_WIDTH, tab::RightTab,
    },
    status_bar,
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView, spawn_local_pty},
    top_bar,
    workspace_tabs::{self, WorkspaceTabs},
};

/// Approximate horizontal inset (CSS px) of the `+` button from its
/// column's left edge. Used by the adapter picker's anchor calculation;
/// the strip can scroll when overflowing so this is intentionally a
/// stable approximation rather than a live layout query.
const ADAPTER_PICKER_LEFT_INSET: f32 = 8.0;

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
    workspace_tabs: Option<Entity<WorkspaceTabs>>,
    right_sidebar: Option<Entity<RightSidebar>>,
    left_rail: Entity<LeftRail>,
    palette: Entity<PaletteModal>,
    pane_actions: Entity<PaneActionsMenu>,
    /// Inline adapter-picker popover. Anchored to the `+` button via a
    /// left-edge offset; opened by the `RequestOpenAdapterPicker` action
    /// (button click or Cmd+Shift+A). Owns its own detection cache.
    adapter_picker: Entity<AdapterPicker>,
    /// CLI agent runtime — owns every agent session's PTY backend and
    /// publishes `AgentStatusStream` per session. One per workspace, held
    /// behind `Arc` so `WorkspaceTabs::close_tab` can cancel and so
    /// `spawn_agent_tab` can drive `start_session`.
    cli_runtime: Arc<CliRuntime>,
    /// Cached registry of the four built-in adapters. Drives the picker's
    /// detect-available list and resolves the chosen `AgentAdapter` to a
    /// concrete adapter at spawn time.
    adapter_registry: Arc<AdapterRegistry>,
    /// Whether the left rail (workspaces + nav) is visible. Toggled via Cmd+B.
    left_rail_open: bool,
    /// Forwards WorkspaceTabs notifications (open/close/select tab) up to
    /// WorkspaceRoot so the top_bar tab strip rebuilds on the next render.
    /// Without this, the strip — which lives in WorkspaceRoot::render, not
    /// inside WorkspaceTabs — would stay stale until something else notified
    /// us.
    _workspace_tabs_observer: Option<Subscription>,
    /// Pauses + kicks the StatusPoller when the window loses / regains focus
    /// (mirrors the standard `document.hasFocus()` gate). Held to keep the
    /// subscription alive for the lifetime of the workspace.
    _window_activation_observer: Subscription,
    /// Drains tab-ids posted by the macOS notifier's click watcher and
    /// activates the matching agent tab. Dropping the task cancels the
    /// loop — bound to WorkspaceRoot's lifetime so a window close stops
    /// trying to dispatch focus events into a torn-down view tree.
    _click_router: Task<()>,
    /// Boot-time snapshot of persisted state (`recent_projects`, workspaces
    /// per project, interrupted sessions) + repo handles for on-demand
    /// reads/writes. Hydrated once by `main.rs` before the window opens;
    /// step 5+ consume it via `pub(crate)` access. Marked `dead_code`-
    /// allowed until step 5's project picker lands — step 4 is plumbing.
    #[allow(dead_code)]
    pub(crate) app_state: AppState,
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

        let workspace_tabs = spawn_initial_workspace(
            theme,
            density,
            typography.clone(),
            cli_runtime.clone(),
            notifier.clone(),
            window,
            cx,
        );
        let workspace_tabs_observer = workspace_tabs
            .as_ref()
            .map(|ws| cx.observe(ws, |_, _, cx| cx.notify()));
        let right_sidebar = repo.clone().map(|r| {
            cx.new(|cx| RightSidebar::new(r, theme, density, typography.clone(), window, cx))
        });
        let left_rail =
            cx.new(|cx| LeftRail::new(repo, theme, density, typography.clone(), window, cx));
        let palette = cx.new(|_| PaletteModal::new(theme, density, typography.clone()));
        let pane_actions = cx.new(|_| PaneActionsMenu::new(theme, density, typography.clone()));

        // Adapter picker: weak self-reference in the on_select closure so
        // the picker can route the user's choice back to `spawn_agent_tab`
        // / `spawn_local_terminal_tab` without holding a strong cycle.
        let weak_self: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let on_select: OnSelect = Box::new(move |selection, window, cx| {
            let weak = weak_self.clone();
            let _ = weak.update(cx, |this, cx| match selection {
                AdapterSelection::NewTerminal => this.spawn_local_terminal_tab(window, cx),
                AdapterSelection::Adapter { kind, id } => {
                    this.spawn_agent_tab(kind, id, window, cx)
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
                        let activated = root.workspace_tabs.as_ref().is_some_and(|tabs_entity| {
                            tabs_entity.update(cx, |tabs, cx| {
                                tabs.set_active_by_tab_id(tab_id, window, cx)
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
            workspace_tabs,
            right_sidebar,
            left_rail,
            palette,
            pane_actions,
            adapter_picker,
            cli_runtime,
            adapter_registry,
            left_rail_open: true,
            _workspace_tabs_observer: workspace_tabs_observer,
            _window_activation_observer: window_activation_observer,
            _click_router: click_router,
            app_state,
        }
    }

    /// Open a fresh local-PTY tab. Used by the picker's "+ New terminal"
    /// row so the popover and the keyboard path stay in sync.
    fn spawn_local_terminal_tab(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace_tabs.clone() else {
            return;
        };
        ws.update(cx, |tabs, cx| tabs.open_tab(window, cx));
    }

    /// Spawn the chosen agent in a new workspace tab. Runs the
    /// start_session → backend_for → terminal_session_id → subscribe_status
    /// chain, then hands the assembled handles to `WorkspaceTabs::push_agent_tab`.
    ///
    /// If `update_in` errors (window/workspace dropped mid-spawn), cancels
    /// the half-mounted session so the PTY doesn't zombie (C2 fix
    /// transferred from step 9b's `on_new_agent`).
    fn spawn_agent_tab(
        &self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ws) = self.workspace_tabs.clone() else {
            return;
        };
        let runtime = self.cli_runtime.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        cx.spawn_in(window, async move |_root, cx| {
            // `adapter_id` arrives from the row the user clicked — the
            // picker holds the `RegistryEntry` slug at click time, so we
            // skip a redundant `detect_available` walk here (M1 fix from
            // review 260520-1830).
            let cfg = AgentSessionConfig {
                adapter,
                worktree_path: cwd,
                prompt: None,
                model: None,
                effort: None,
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

            let mount_result = ws.update_in(cx, |tabs, window, cx| {
                tabs.push_agent_tab(
                    adapter_id, session_id, status_rx, backend, term_id, window, cx,
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
    /// settings panel + tests; the main consumer is the `WorkspaceTabs`
    /// strip, which received its own `Arc` clone at construction time.
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
}

fn spawn_initial_workspace(
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
    notifier: Arc<dyn Notifier>,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<WorkspaceTabs>> {
    let (backend, session_id) = spawn_local_pty()?;
    let typography_for_view = typography.clone();
    let initial_view = cx.new(|cx| {
        TerminalView::mount(
            backend,
            session_id,
            theme,
            density,
            typography_for_view,
            window,
            cx,
        )
    });
    let initial_pane =
        cx.new(|cx| MainPane::new(initial_view, theme, density, typography.clone(), cx));
    Some(cx.new(|cx| {
        WorkspaceTabs::new(
            initial_pane,
            theme,
            density,
            typography,
            cli_runtime,
            notifier,
            window,
            cx,
        )
    }))
}

impl Render for WorkspaceRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        // Use the active tab's pane count for the status-bar readout. Inactive
        // tabs' panes still exist but the user only sees the active grid.
        let pane_count = self
            .workspace_tabs
            .as_ref()
            .and_then(|ws| ws.read(cx).active_pane())
            .map(|mp| mp.read(cx).leaf_count())
            .unwrap_or(0);

        // Open-agent-tab count for the status-bar readout. Phase 3 step 9:
        // replaces the hardcoded 0 with a live count sourced from
        // `WorkspaceTabKind::Agent` entries.
        //
        // NOTE (M5, review 260520-1700): semantic asymmetry — `pane_count`
        // reads the active tab only (via `active_pane().leaf_count()`),
        // whereas `agent_count` aggregates across every workspace tab. For
        // v1 single-workspace the difference is academic; the choice is
        // deliberate because users care about total agents in flight
        // ("I have 3 agents running"), not how many are in the foreground.
        // If a multi-window or per-workspace status bar lands later, this
        // is the seam to revisit.
        let agent_count = self
            .workspace_tabs
            .as_ref()
            .map(|ws| ws.read(cx).agent_count())
            .unwrap_or(0);

        // Route poll state to the status bar via the RightSidebar getter.
        let git_state = self
            .right_sidebar
            .as_ref()
            .map(|s| s.read(cx).latest_poll_state().clone());

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

        // Push current chrome width into every workspace tab so PTY grids
        // match the actual visible area. Each MainPane caches the width and
        // re-applies on render; propagating to inactive tabs avoids a stale
        // grid flash on tab switch.
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
        if let Some(ws) = self.workspace_tabs.as_ref() {
            let chrome = left_chrome + right_chrome;
            ws.update(cx, |ws, cx| ws.set_chrome_width(chrome, cx));
        }

        // Workspace tab strip for the center column's header. None when the
        // workspace failed to bootstrap a PTY.
        let workspace_tab_strip = self
            .workspace_tabs
            .as_ref()
            .map(|ws| workspace_tabs::render_tab_strip(ws.clone(), cx));

        // Center column body: active MainPane via WorkspaceTabs, or welcome
        // placeholder when no PTY backend bootstrapped.
        let center_body: AnyElement = match self.workspace_tabs.clone() {
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

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            .on_action(cx.listener(|this, _: &ToggleLeftSidebar, _window, cx| {
                this.left_rail_open = !this.left_rail_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenQuickOpen, _window, cx| {
                this.palette
                    .update(cx, |p, cx| p.open(PaletteMode::QuickOpen, cx));
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, _window, cx| {
                this.palette
                    .update(cx, |p, cx| p.open(PaletteMode::Commands, cx));
            }))
            .on_action(
                cx.listener(|this, _: &RequestOpenAdapterPicker, window, cx| {
                    // Anchor the picker's LEFT edge under the `+` button.
                    // The button is rendered inline after the last tab, so
                    // its window-x drifts with tab count + label widths +
                    // scroll state. The click handler stashes the captured
                    // event.position.x; we consume it here. Keyboard path
                    // (Cmd+Shift+A) has no click position — fall back to a
                    // static post-rail inset.
                    let fallback_anchor = if this.left_rail_open {
                        this.density.w_left_rail + ADAPTER_PICKER_LEFT_INSET
                    } else {
                        ADAPTER_PICKER_LEFT_INSET
                    };
                    let left_anchor = this
                        .workspace_tabs
                        .as_ref()
                        .and_then(|ws| ws.read(cx).take_plus_click_x())
                        .unwrap_or(fallback_anchor);
                    // M2 (review 260520-1830): both popovers register a
                    // full-window overlay; if both opened simultaneously
                    // the lower-z one would have no click-outside dismiss
                    // path. Close pane-actions first to enforce mutex.
                    this.pane_actions.update(cx, |p, cx| p.close(cx));
                    this.adapter_picker
                        .update(cx, |p, cx| p.open(left_anchor, window, cx));
                }),
            )
            .on_action(cx.listener(|this, _: &OpenPaneActions, _window, cx| {
                // Anchor the dropdown's RIGHT edge at the "..." button's
                // right edge so it feels visually attached.
                // When right sidebar is OPEN: center column ends where the
                // right column begins, so "..." button right edge sits at
                // `DEFAULT_PANEL_WIDTH` from window right.
                // When CLOSED: "..." button is just left of the right
                // toggle in the center header, so its right edge sits at
                // `TOGGLE_BUTTON_WIDTH` from window right.
                let r_open = this.right_sidebar.as_ref().is_some_and(|s| s.read(cx).open);
                let right_anchor = if r_open {
                    f32::from(DEFAULT_PANEL_WIDTH)
                } else {
                    top_bar::TOGGLE_BUTTON_WIDTH
                };
                // M2 mutex (see RequestOpenAdapterPicker above).
                this.adapter_picker.update(cx, |p, cx| p.close(cx));
                this.pane_actions
                    .update(cx, |p, cx| p.open(right_anchor, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleRightSidebar, _window, cx| {
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |s, cx| s.toggle(cx));
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
                // Cmd+K: jump to Source Control tab and focus the inline
                // commit subject input. Phase 04 replaces the legacy
                // modal CommitDialog with this inline area.
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
                agent_count,
                git_state.as_ref(),
            ))
            // Pane Actions dropdown — appended before the palette so the
            // palette (more rare, larger) wins z-order when both are open.
            .child(self.pane_actions.clone())
            // Adapter picker — same z-band as pane_actions; only one of
            // them can be open at a time so order between them is moot.
            .child(self.adapter_picker.clone())
            // Palette modal — appended last so it paints above all other
            // children (last child = topmost z-layer in GPUI).
            .child(self.palette.clone())
    }
}
