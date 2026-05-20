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

use gpui::{
    AnyElement, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, div, px,
};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    OpenCommandPalette, OpenCommitDialog, OpenPaneActions, OpenQuickOpen, SelectExplorerTab,
    SelectSearchTab, SelectSourceControlTab, ToggleLeftSidebar, ToggleRightSidebar,
};
use crate::shell::{
    command_palette::{PaletteModal, entry::PaletteMode},
    left_rail::LeftRail,
    main_area,
    main_pane::MainPane,
    pane_actions::PaneActionsMenu,
    right_sidebar::{
        RightSidebar, activity_bar::render_tab_buttons, layout::DEFAULT_PANEL_WIDTH, tab::RightTab,
    },
    status_bar,
    terminal_view::{TerminalView, spawn_local_pty},
    top_bar,
    workspace_tabs::{self, WorkspaceTabs},
};

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
    workspace_tabs: Option<Entity<WorkspaceTabs>>,
    right_sidebar: Option<Entity<RightSidebar>>,
    left_rail: Entity<LeftRail>,
    palette: Entity<PaletteModal>,
    pane_actions: Entity<PaneActionsMenu>,
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
}

impl WorkspaceRoot {
    pub fn new(repo: Option<Repository>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::charcoal();
        let density = Density::cockpit();
        let typography = Typography::cockpit();

        let workspace_tabs =
            spawn_initial_workspace(theme, density, typography.clone(), window, cx);
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

        // Pause status polling when the window blurs; force an immediate
        // refresh on focus regain via StatusPoller::kick().
        let window_activation_observer =
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                if let Some(rs) = &this.right_sidebar {
                    rs.update(cx, |sidebar, _cx| sidebar.set_polling_focused(active));
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
            left_rail_open: true,
            _workspace_tabs_observer: workspace_tabs_observer,
            _window_activation_observer: window_activation_observer,
        }
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
    Some(cx.new(|cx| WorkspaceTabs::new(initial_pane, theme, density, typography, cx)))
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
            // TODO(phase-07): replace 0 with AgentRuntime::active_count().
            .child(status_bar::view(
                theme,
                density,
                typography,
                pane_count,
                0,
                git_state.as_ref(),
            ))
            // Pane Actions dropdown — appended before the palette so the
            // palette (more rare, larger) wins z-order when both are open.
            .child(self.pane_actions.clone())
            // Palette modal — appended last so it paints above all other
            // children (last child = topmost z-layer in GPUI).
            .child(self.palette.clone())
    }
}
