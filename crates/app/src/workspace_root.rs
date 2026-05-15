//! WorkspaceRoot — the top-level view mounted into the GPUI window.
//!
//! Composition (vertical):
//!
//! ```text
//! ┌─────────────────────────────────────────────┐  ← TopBar (36px)
//! ├──────────┬──────────────────────────────────┤
//! │ Sidebar  │            MainPane              │  ← HSplit
//! │ (240px)  │      (Phase 1: pane grid)        │
//! ├──────────┴──────────────────────────────────┤  ← StatusBar (22px)
//! └─────────────────────────────────────────────┘
//! ```
//!
//! Phase 1 step 5 swaps in `MainPane`, which owns a tree of terminal panes.
//! The initial PTY spawn happens here (fallible); on failure we render the
//! placeholder so the shell still composes cleanly.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::{
    main_area,
    main_pane::MainPane,
    sidebar, status_bar,
    tabbed_pane::TabbedPane,
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView},
    top_bar,
};

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
    main_pane: Option<Entity<MainPane>>,
}

impl WorkspaceRoot {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::charcoal();
        let density = Density::cockpit();
        let typography = Typography::cockpit();

        let main_pane = spawn_initial_pane(theme, density, typography.clone(), window, cx);

        Self {
            theme,
            density,
            typography,
            main_pane,
        }
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

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            .child(top_bar::view(theme, density, typography))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    .child(sidebar::view(theme, density, typography))
                    .child(main),
            )
            .child(status_bar::view(theme, density, typography, pane_count))
    }
}
