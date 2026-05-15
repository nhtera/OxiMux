//! WorkspaceRoot — the top-level view mounted into the GPUI window.
//!
//! Composition (vertical):
//!
//! ```text
//! ┌─────────────────────────────────────────────┐  ← TopBar (36px)
//! ├──────────┬──────────────────────────────────┤
//! │ Sidebar  │            MainArea              │  ← HSplit
//! │ (240px)  │  (Phase 1: hosts TerminalView)   │
//! ├──────────┴──────────────────────────────────┤  ← StatusBar (22px)
//! └─────────────────────────────────────────────┘
//! ```
//!
//! Phase 1 step 4 mounts a single `TerminalView` entity into the main area.
//! If PTY spawn fails (uncommon — would be EPERM / missing $SHELL), the
//! original placeholder hint renders so the shell still composes.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use oximux_pty::{PortablePtyBackend, SpawnConfig, TerminalBackend};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::{
    main_area, sidebar, status_bar,
    terminal_view::{DEFAULT_COLS, DEFAULT_ROWS, TerminalView},
    top_bar,
};

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
    terminal: Option<Entity<TerminalView>>,
}

impl WorkspaceRoot {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::charcoal();
        let density = Density::cockpit();
        let typography = Typography::cockpit();

        let terminal = spawn_default_terminal(theme, density, typography.clone(), window, cx);

        Self {
            theme,
            density,
            typography,
            terminal,
        }
    }
}

fn spawn_default_terminal(
    theme: Theme,
    density: Density,
    typography: Typography,
    window: &mut Window,
    cx: &mut Context<WorkspaceRoot>,
) -> Option<Entity<TerminalView>> {
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
    Some(
        cx.new(|cx| {
            TerminalView::mount(backend, session_id, theme, density, typography, window, cx)
        }),
    )
}

impl Render for WorkspaceRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

        let main = div().flex().flex_1().h_full().min_w(px(0.));
        let main = match self.terminal.clone() {
            Some(view) => main.child(view),
            None => main.child(main_area::view(theme, density, typography)),
        };

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
            .child(status_bar::view(theme, density, typography))
    }
}
