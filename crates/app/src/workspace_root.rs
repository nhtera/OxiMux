//! WorkspaceRoot — the top-level view mounted into the GPUI window.
//!
//! Composition (vertical):
//!
//! ```text
//! ┌─────────────────────────────────────────────┐  ← TopBar (36px)
//! ├──────────┬──────────────────────────────────┤
//! │ Sidebar  │            MainArea              │  ← HSplit
//! │ (240px)  │                                  │
//! ├──────────┴──────────────────────────────────┤  ← StatusBar (22px)
//! └─────────────────────────────────────────────┘
//! ```

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::{main_area, sidebar, status_bar, top_bar};

pub struct WorkspaceRoot {
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl WorkspaceRoot {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            theme: Theme::charcoal(),
            density: Density::cockpit(),
            typography: Typography::cockpit(),
        }
    }
}

impl Render for WorkspaceRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;

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
                    .child(main_area::view(theme, density, typography)),
            )
            .child(status_bar::view(theme, density, typography))
    }
}
