//! Window-level terminal actions that need more than the terminal view:
//! Insert Path (it opens the command palette) and the read-only toggle
//! (reachable from the palette, after which focus is the window's, not the
//! terminal's). Both act on the active pane group's visible terminal.

use super::*;
use crate::actions::{InsertPathIntoTerminal, ToggleTerminalReadOnly};
use crate::shell::terminal_view::TerminalView;

impl WorkspaceRoot {
    /// The terminal Insert Path and the read-only toggle act on: the floating
    /// terminal when it has focus, else the active pane group's visible one.
    fn active_terminal(&self, window: &Window, cx: &gpui::App) -> Option<Entity<TerminalView>> {
        if let Some(floating) = self.focused_floating_terminal(window, cx) {
            return Some(floating);
        }
        match self.focused_dictation_sink(cx)? {
            crate::shell::agent_chat::HudSink::Terminal(view) => view.upgrade(),
            _ => None,
        }
    }

    fn focused_floating_terminal(&self, window: &Window, cx: &gpui::App) -> Option<Entity<TerminalView>> {
        self.floating_terminal
            .as_ref()
            .filter(|_| self.floating_terminal_visible)
            .and_then(|f| f.read(cx).active_view().cloned())
            .filter(|v| gpui::Focusable::focus_handle(v.read(cx), cx).contains_focused(window, cx))
    }

    /// Before the command palette takes focus, note the floating terminal if
    /// it has it. The palette closes before it runs a command, and focus must
    /// go back to that terminal for the command to find it. (A pane terminal
    /// needs nothing: the panes track it without focus.)
    pub(super) fn remember_palette_terminal(&mut self, window: &Window, cx: &gpui::App) {
        self.palette_terminal = self.focused_floating_terminal(window, cx).map(|v| v.downgrade());
    }

    fn no_terminal_toast(cx: &mut Context<Self>) {
        crate::shell::toast::toast(cx, crate::shell::toast::ToastKind::Info, "Focus a terminal first.");
    }

    /// `true` (and the user is told) when the agent session text is about to
    /// be sent to runs in a read-only terminal: its PTY write goes through the
    /// agent runtime, past the terminal view's own input gate.
    pub(super) fn refuse_locked_agent(&self, session_id: oximux_core::AgentSessionId, cx: &mut Context<Self>) -> bool {
        let term = self.cli_runtime.terminal_session_id(session_id).ok();
        let locked = self.active_project_panes().is_some_and(|panes| {
            panes
                .read(cx)
                .groups_iter()
                .any(|g| g.read(cx).agent_session_read_only(session_id, term, cx))
        });
        if locked {
            crate::shell::toast::toast(
                cx,
                crate::shell::toast::ToastKind::Info,
                "That agent's terminal is read-only — unlock it to send text to it.",
            );
        }
        locked
    }

    fn on_insert_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Same gates as Quick Open: never over a confirm, and mutex with every
        // other full-window overlay.
        if self.confirm_pending(cx) {
            return;
        }
        self.close_modal_overlays(cx);
        let Some(view) = self.active_terminal(window, cx) else {
            Self::no_terminal_toast(cx);
            return;
        };
        if view.read(cx).is_read_only() {
            crate::shell::toast::toast(cx, crate::shell::toast::ToastKind::Info, "This terminal is read-only.");
            return;
        }
        let Some(root) = view
            .read(cx)
            .live_cwd()
            .or_else(|| self.active_project_panes().map(|p| p.read(cx).cwd().clone()))
        else {
            return;
        };
        self.palette_terminal = Some(view.downgrade());
        self.palette.update(cx, |p, cx| p.open_insert_path(root, window, cx));
    }

    fn on_toggle_read_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_terminal(window, cx) else {
            Self::no_terminal_toast(cx);
            return;
        };
        view.update(cx, |v, cx| v.toggle_read_only(cx));
        // Reached from the palette, focus is the window's; give it back.
        let handle = gpui::Focusable::focus_handle(view.read(cx), cx);
        window.focus(&handle, cx);
    }

    /// The palette's events: a picked path goes to the terminal that asked;
    /// on close, focus returns there (or to the window).
    pub(super) fn on_palette_event(&mut self, ev: &PaletteEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            PaletteEvent::InsertPath(path) => {
                let Some(view) = self.palette_terminal.as_ref().and_then(|v| v.upgrade()) else {
                    return;
                };
                let typed = view.update(cx, |v, cx| v.insert_path(path, cx));
                if !typed {
                    crate::shell::toast::toast(
                        cx,
                        crate::shell::toast::ToastKind::Warning,
                        "That path cannot be typed safely in this shell.",
                    );
                }
            }
            // A close that lands after the palette was opened again (⌥⌘P over
            // an open palette closes it, then reopens) is stale: the new
            // picker owns the target and the focus.
            PaletteEvent::Closed if self.palette.read(cx).is_open() => {}
            PaletteEvent::Closed => match self.palette_terminal.take().and_then(|v| v.upgrade()) {
                Some(view) => {
                    let handle = gpui::Focusable::focus_handle(view.read(cx), cx);
                    window.focus(&handle, cx);
                }
                None => self.focus_handle.focus(window, cx),
            },
        }
    }
}

/// Register the actions above on the window's root element.
pub(super) fn register<E: InteractiveElement>(el: E, cx: &mut Context<WorkspaceRoot>) -> E {
    el.on_action(cx.listener(|this, _: &InsertPathIntoTerminal, window, cx| this.on_insert_path(window, cx)))
        .on_action(cx.listener(|this, _: &ToggleTerminalReadOnly, window, cx| this.on_toggle_read_only(window, cx)))
}
