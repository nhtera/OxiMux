//! Pane content variants — terminal or editor leaf.
//!
//! Each leaf of `MainPane.tree` resolves to a `PaneContent` value held by
//! `panes: HashMap<PaneId, PaneContent>`. The enum carries the per-leaf
//! entity (TerminalView or EditorView) and exposes uniform accessors for
//! focus state, focus-handle identity, and a discriminator (`is_editor`).
//!
//! Multi-file editing is handled at the workspace-tab level: opening a
//! new file creates a new `WorkspaceTab` of kind `Editor`, not a sub-tab
//! inside an existing pane leaf. That keeps the tab strip flat — one
//! horizontal row mixing terminal and editor tabs — matching the
//! industry-standard editor convention.
//!
//! PTY-only operations (set_target_grid, serialize_buffer, prefill_grid,
//! external_id, title) stay on `TerminalView` and are reached by matching
//! `PaneContent::Terminal(view)` at the call site. The enum deliberately
//! does NOT proxy them — `set_target_grid` makes no sense for an editor
//! leaf, and a single fall-through implementation would silently no-op
//! editor leaves at the dispatch layer instead of letting callers handle
//! the variant explicitly.

use std::path::Path;

use gpui::{App, Entity, FocusHandle, Focusable};
use oximux_editor::EditorView;

use crate::shell::terminal_view::TerminalView;

/// Content payload for a single pane leaf.
pub enum PaneContent {
    Terminal(Entity<TerminalView>),
    Editor(Entity<EditorView>),
}

impl PaneContent {
    /// Focus handle for the underlying view. Used by the host to focus a
    /// leaf and to compare against `Window::focused()` for click-to-focus
    /// reconciliation.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Terminal(view) => view.read(cx).focus_handle(cx),
            Self::Editor(view) => view.read(cx).focus_handle(cx),
        }
    }

    /// Whether this content currently owns platform focus. Both variants
    /// mirror window focus into a local boolean via `cx.on_focus` /
    /// `cx.on_blur` subscriptions installed at construction, so this is a
    /// plain read with no `&Window` dependency.
    pub fn focused(&self, cx: &App) -> bool {
        match self {
            Self::Terminal(view) => view.read(cx).focused(),
            Self::Editor(view) => view.read(cx).focused(),
        }
    }

    /// `true` if this leaf is an editor; `false` for terminals. Used by
    /// PTY-only paths (grid dispatch, buffer capture) to skip editor
    /// leaves without an explicit match.
    pub fn is_editor(&self) -> bool {
        matches!(self, Self::Editor(_))
    }

    /// Source path for an editor leaf; `None` for terminals.
    pub fn editor_path<'a>(&'a self, cx: &'a App) -> Option<&'a Path> {
        match self {
            Self::Terminal(_) => None,
            Self::Editor(view) => Some(view.read(cx).file_path()),
        }
    }
}
