//! Pane content variants — terminal or editor leaf.
//!
//! Carries the per-tab content entity inside a `PaneGroup`. The enum
//! exposes uniform accessors for focus handle, focused state, and a
//! discriminator (`is_editor`). PTY-only operations stay on
//! `TerminalView` and are reached by matching `PaneContent::Terminal`
//! at the call site.

use std::path::Path;

use gpui::{App, Entity, FocusHandle, Focusable};
use oximux_editor::EditorView;

use crate::shell::terminal_view::TerminalView;

pub enum PaneContent {
    Terminal(Entity<TerminalView>),
    Editor(Entity<EditorView>),
}

impl PaneContent {
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(view) => view.read(cx).focus_handle(cx),
            Self::Editor(view) => view.read(cx).focus_handle(cx),
        }
    }

    pub fn focused(&self, cx: &App) -> bool {
        match self {
            Self::Terminal(view) => view.read(cx).focused(),
            Self::Editor(view) => view.read(cx).focused(),
        }
    }

    pub fn is_editor(&self) -> bool {
        matches!(self, Self::Editor(_))
    }

    pub fn editor_path<'a>(&'a self, cx: &'a App) -> Option<&'a Path> {
        match self {
            Self::Terminal(_) => None,
            Self::Editor(view) => Some(view.read(cx).file_path()),
        }
    }
}
