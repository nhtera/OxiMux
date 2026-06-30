//! Pane content variants — terminal split-tree or editor leaf.
//!
//! Carries the per-tab content inside a `PaneGroup`. Terminal tabs can
//! hold MULTIPLE PTYs in a sub-pane split tree (Cmd+D / Cmd+Shift+D);
//! editor tabs are always single. Both expose uniform accessors for
//! focus handle and focused state. PTY-only operations stay reachable
//! via `terminal_active_view()` for ergonomic single-pane access, and
//! via the inner `TerminalSplitTree` for multi-pane iteration.

use std::path::Path;

use gpui::{App, Entity, FocusHandle, Focusable};
use oximux_editor::EditorView;

use crate::shell::browser_view::BrowserView;
use crate::shell::diff_view::DiffView;
use crate::shell::pane_group::sub_pane::TerminalSplitTree;
use crate::shell::tasks_view::TasksView;
use crate::shell::terminal_view::TerminalView;

pub enum PaneContent {
    /// Tree of one or more sub-pane terminal views. A single-pane tab
    /// is a tree-of-one; split via `Cmd+D` / `Cmd+Shift+D`.
    Terminal(TerminalSplitTree),
    Editor(Entity<EditorView>),
    /// Read-only diff renderer for a single tracked file. Constructed by
    /// the SCM panel when the user clicks a changed-file row, then
    /// `load(path, staged)`'d to fetch the patch. Distinct from
    /// `Editor` so we never accidentally try to save / mutate a diff
    /// view, and so the tab kind survives same-file editor + diff
    /// being open simultaneously.
    Diff(Entity<DiffView>),
    /// In-process embedded browser leaf. Owns a native webview layered over
    /// the GPU canvas; persists only its URL. No PTY, no relay.
    Browser(Entity<BrowserView>),
    /// GitHub issue / PR browser for the active project. Singleton per
    /// workspace: the nav rail opens one instance and re-activates it on
    /// subsequent clicks. Not persisted across restarts — the nav re-opens
    /// it from scratch after a session restore.
    Tasks(Entity<TasksView>),
}

impl PaneContent {
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            // Focus always follows the ACTIVE sub-pane so keyboard
            // input lands where the rim glow draws.
            Self::Terminal(tree) => tree
                .active_view()
                .map(|v| v.read(cx).focus_handle(cx))
                .unwrap_or_else(|| cx.focus_handle()),
            Self::Editor(view) => view.read(cx).focus_handle(cx),
            Self::Diff(view) => view.read(cx).focus_handle(cx),
            Self::Browser(view) => view.read(cx).focus_handle(cx),
            Self::Tasks(view) => view.read(cx).focus_handle(cx),
        }
    }

    pub fn focused(&self, cx: &App) -> bool {
        match self {
            Self::Terminal(tree) => tree.active_view().is_some_and(|v| v.read(cx).focused()),
            Self::Editor(view) => view.read(cx).focused(),
            // DiffView is read-only and doesn't track platform focus in
            // a cached flag (unlike `EditorView::focused`). The host's
            // per-leaf focus bookkeeping doesn't consume diff focus for
            // routing — terminals/editor are the focusable surfaces. Return
            // `false` so a diff-tab being active never claims "focused"
            // semantics that callers would route input to.
            Self::Diff(_) => false,
            // The webview owns native first-responder when the user clicks
            // into the page; GPUI's per-leaf focus bookkeeping doesn't route
            // input to it, so report `false` like the diff view.
            Self::Browser(_) => false,
            // Tasks is read-only (no text input); report `false` like Diff.
            Self::Tasks(_) => false,
        }
    }

    pub fn is_editor(&self) -> bool {
        matches!(self, Self::Editor(_))
    }

    pub fn editor_path<'a>(&'a self, cx: &'a App) -> Option<&'a Path> {
        match self {
            Self::Terminal(_) | Self::Diff(_) | Self::Browser(_) | Self::Tasks(_) => None,
            Self::Editor(view) => Some(view.read(cx).file_path()),
        }
    }

    /// Ergonomic accessor for callers that only care about the ACTIVE
    /// sub-pane of a terminal tab. Returns `None` for editor tabs or
    /// degenerate trees. Used by persistence (active scrollback) and
    /// IPC routing (active PTY).
    pub fn terminal_active_view(&self) -> Option<&Entity<TerminalView>> {
        match self {
            Self::Terminal(tree) => tree.active_view(),
            Self::Editor(_) | Self::Diff(_) | Self::Browser(_) | Self::Tasks(_) => None,
        }
    }
}
