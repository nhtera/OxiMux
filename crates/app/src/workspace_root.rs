//! WorkspaceRoot — the top-level view mounted into the GPUI window.
//!
//! Composition (vertical):
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐  ← TopBar (40px)
//! ├──────────┬──────────────────────────┬───────────────────┤
//! │ Sidebar  │        MainPane          │   RightSidebar    │  ← HSplit
//! │ (240px)  │     (Phase 1 grid)       │ panel(360) + bar  │
//! ├──────────┴──────────────────────────┴───────────────────┤  ← StatusBar (22px)
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! The right column renders only when a repository was opened at startup.
//! The left rail collapses to zero width when `left_rail_open` is false
//! (toggled via Cmd+B). Without a repo, layout collapses to `Sidebar | MainPane | StatusBar`.
//!
//! Action routing note: sidebar tab-select keybindings (cmd-l, cmd-shift-e/f/g)
//! are registered here on the outermost div — not on RightSidebar's div.
//! RightSidebar is a sibling of MainPane, so its div is never in the ancestor
//! chain when TerminalView holds focus. The outer WorkspaceRoot div IS the
//! common ancestor of every focused element.

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Window, div, px,
};
use oximux_git::Repository;
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    OpenCommandPalette, OpenQuickOpen, SelectExplorerTab, SelectSearchTab, SelectSourceControlTab,
    ToggleLeftSidebar, ToggleRightSidebar,
};
use crate::shell::{
    command_palette::{PaletteModal, entry::PaletteMode},
    left_rail::LeftRail,
    main_area,
    main_pane::MainPane,
    right_sidebar::{
        RightSidebar, activity_bar::render_tab_buttons, layout::DEFAULT_PANEL_WIDTH,
        tab::RightTab,
    },
    status_bar,
    tabbed_pane::TabbedPane,
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView},
    top_bar,
};

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
    main_pane: Option<Entity<MainPane>>,
    right_sidebar: Option<Entity<RightSidebar>>,
    left_rail: Entity<LeftRail>,
    palette: Entity<PaletteModal>,
    /// Whether the left rail (workspaces + nav) is visible. Toggled via Cmd+B.
    left_rail_open: bool,
}

impl WorkspaceRoot {
    pub fn new(repo: Option<Repository>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::charcoal();
        let density = Density::cockpit();
        let typography = Typography::cockpit();

        let main_pane = spawn_initial_pane(theme, density, typography.clone(), window, cx);
        let right_sidebar = repo
            .clone()
            .map(|r| cx.new(|cx| RightSidebar::new(r, theme, density, typography.clone(), cx)));
        let left_rail =
            cx.new(|cx| LeftRail::new(repo, theme, density, typography.clone(), window, cx));
        let palette = cx.new(|_| PaletteModal::new(theme, density, typography.clone()));

        Self {
            theme,
            density,
            typography,
            main_pane,
            right_sidebar,
            left_rail,
            palette,
            left_rail_open: true,
        }
    }

    /// Test-only inspector for the left-rail visibility flag.
    #[doc(hidden)]
    pub fn left_rail_open(&self) -> bool {
        self.left_rail_open
    }
}

fn spawn_initial_pane(
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<MainPane>> {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        ..SpawnConfig::default()
    };
    let session_id = match backend.spawn(cfg) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "pty spawn failed; rendering placeholder");
            return None;
        }
    };
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
    let initial_pane = cx.new(|cx| TabbedPane::new(initial_view, cx));
    Some(cx.new(|cx| MainPane::new(initial_pane, theme, density, typography, cx)))
}

impl Render for WorkspaceRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        let main = div().flex().flex_1().h_full().min_w(px(0.));
        let main = match self.main_pane.clone() {
            Some(view) => main.child(view),
            None => main.child(main_area::view(theme, density, typography)),
        };

        let pane_count = self
            .main_pane
            .as_ref()
            .map(|mp| mp.read(cx).leaf_count())
            .unwrap_or(0);

        // Gate the left rail on `left_rail_open`. Full collapse (zero width)
        // matches the reference UX's pattern — no permanent mini-strip.
        let row = div().flex().flex_row().flex_1().min_h(px(0.));
        let row = if self.left_rail_open {
            row.child(self.left_rail.clone())
        } else {
            row
        };
        let row = row.child(main);

        let row = match self.right_sidebar.clone() {
            Some(sidebar) => row.child(sidebar),
            None => row,
        };

        // Route poll state to the status bar via the RightSidebar getter.
        let git_state = self
            .right_sidebar
            .as_ref()
            .map(|s| s.read(cx).latest_poll_state().clone());

        // Activity-bar tabs live in the global top_bar (the reference UX pattern). Compose
        // them here so top_bar stays decoupled from RightSidebar internals.
        // Tabs hide when the panel is closed — clicking the panel-right toggle
        // re-opens the panel via ToggleRightSidebar.
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

        // Push current chrome width into MainPane so the PTY grid matches the
        // actual visible area. Without this, opening the right sidebar would
        // leave the terminal sized to the old (wider) MainPane and content
        // would render past the sidebar boundary.
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
        if let Some(mp) = self.main_pane.as_ref() {
            mp.update(cx, |pane, cx| {
                pane.set_chrome_width(left_chrome + right_chrome, cx);
            });
        }

        // Sidebar action handlers live on the outermost div — the common ancestor
        // of every focused element (TerminalView → MainPane → row → here).
        // Placing them on RightSidebar's div would break dispatch when the terminal
        // holds focus (RightSidebar is a sibling, not an ancestor, of TerminalView).
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
            .child(top_bar::view(
                self.left_rail_open,
                right_open,
                right_tabs,
                theme,
                density,
                typography,
            ))
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
            // Palette modal — appended last so it paints above all other
            // children (last child = topmost z-layer in GPUI, same pattern
            // as `terminal_search_overlay`).
            .child(self.palette.clone())
    }
}
