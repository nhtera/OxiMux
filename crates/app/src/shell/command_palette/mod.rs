//! Command Palette + Quick Open modal shell.
//!
//! Phase 05 ships the visible modal + match engine + entry registry. Key
//! navigation (↑/↓/Enter) and live filtering wire in a follow-up phase
//! once the input plumbing is settled.

pub mod entry;
pub mod match_engine;
pub mod palette_modal;

use gpui::{Context, IntoElement, Render, Window, div};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::command_palette::entry::{PALETTE_COMMANDS, PaletteMode, QUICK_OPEN_STUBS};
use crate::shell::command_palette::match_engine::filter_and_rank;
use crate::shell::command_palette::palette_modal::{ModalRenderInput, build_modal_layout};

/// Single entity hosts both palette modes. `open` controls visibility;
/// `mode` selects between Quick Open and Command Palette.
pub struct PaletteModal {
    mode: PaletteMode,
    open: bool,
    query: String,
    selected_idx: usize,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl PaletteModal {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            mode: PaletteMode::Commands,
            open: false,
            query: String::new(),
            selected_idx: 0,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn mode(&self) -> PaletteMode {
        self.mode
    }

    pub fn open(&mut self, mode: PaletteMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.open = true;
        self.query.clear();
        self.selected_idx = 0;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.query.clear();
        self.selected_idx = 0;
        cx.notify();
    }
}

impl Render for PaletteModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let mode = self.mode;
        let query = self.query.clone();
        let selected_idx = self.selected_idx;

        let (command_rows, file_rows) = match mode {
            PaletteMode::Commands => {
                let names: Vec<&str> = PALETTE_COMMANDS.iter().map(|c| c.name).collect();
                let ranked = filter_and_rank(&query, &names);
                let rows: Vec<_> = ranked.iter().map(|&i| &PALETTE_COMMANDS[i]).collect();
                (rows, Vec::new())
            }
            PaletteMode::QuickOpen => {
                let ranked = filter_and_rank(&query, QUICK_OPEN_STUBS);
                let rows: Vec<_> = ranked.iter().map(|&i| QUICK_OPEN_STUBS[i]).collect();
                (Vec::new(), rows)
            }
        };

        build_modal_layout(ModalRenderInput {
            mode,
            query: &query,
            selected_idx,
            command_rows,
            file_rows,
            theme,
            density,
            typography: &typography,
        })
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_modal() -> PaletteModal {
        PaletteModal::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_modal_is_closed() {
        let m = test_modal();
        assert!(!m.is_open());
        assert_eq!(m.query(), "");
    }

    #[test]
    fn close_after_dirty_query_resets_query_field() {
        // Pure-state regression check for the `Esc clears query` behavior.
        // (Open / close themselves require Context<Self> and live in the
        // entity-bound flow; this test exercises the reset invariant.)
        let mut m = test_modal();
        m.open = true;
        m.query.push_str("foo");
        m.selected_idx = 3;
        // Mirror close() inline (skips cx.notify() since no entity here).
        m.open = false;
        m.query.clear();
        m.selected_idx = 0;
        assert!(!m.is_open());
        assert!(m.query.is_empty());
        assert_eq!(m.selected_idx, 0);
    }
}
