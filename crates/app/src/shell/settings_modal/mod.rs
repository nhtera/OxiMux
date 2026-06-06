//! Settings modal — minimal panes wiring settings that already round-trip
//! to disk (terminal + AI commit-message), plus read-only reference panes
//! (keybindings, appearance, about). Opened via the left-rail cog or
//! `Cmd+,`. Mirrors the `project_picker` modal pattern: open/close +
//! click-outside dismiss + `track_focus` for keyboard handling.
//!
//! Editable panes apply immediately — each control mutates a working copy
//! and writes the TOML; the existing file watcher reloads + repaints. The
//! modal never sets the global itself (that would race the debouncer).
//! Rendering lives in [`view`]; this file owns state + persistence.

mod controls;
mod nav;
mod pane_about;
mod pane_agents;
mod pane_keybindings;
mod pane_terminal;
mod view;

pub use nav::SettingsPane;

use gpui::{App, Context, FocusHandle, Focusable, Window};
use oximux_settings::{CommitMessageAiSettings, Density, TerminalSettings, Theme, Typography};

/// Modal card dimensions.
const CARD_WIDTH: f32 = 960.0;
const CARD_HEIGHT: f32 = 640.0;
/// Vertical offset from the top of the viewport (matches project_picker).
const MODAL_TOP_OFFSET: f32 = 64.0;

pub struct SettingsModal {
    pub(super) open: bool,
    pub(super) selected: SettingsPane,
    pub(super) focus_handle: FocusHandle,
    pub(super) theme: Theme,
    pub(super) density: Density,
    pub(super) typography: Typography,
    /// Working copy of the terminal settings; reseeded from the live global
    /// at each `open()`. Edits mutate this, then write `terminal.toml`.
    pub(crate) terminal: TerminalSettings,
    /// Working copy of the AI commit-message settings; same contract.
    pub(crate) ai: CommitMessageAiSettings,
}

impl SettingsModal {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            open: false,
            selected: SettingsPane::Terminal,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            terminal: TerminalSettings::default(),
            ai: CommitMessageAiSettings::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the modal, seeding the working copies from the live globals so
    /// the panes reflect what is currently on disk and applied.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal = cx
            .try_global::<TerminalSettings>()
            .copied()
            .unwrap_or_default();
        self.ai = cx
            .try_global::<CommitMessageAiSettings>()
            .cloned()
            .unwrap_or_default();
        self.open = true;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    pub(super) fn select_pane(&mut self, pane: SettingsPane, cx: &mut Context<Self>) {
        self.selected = pane;
        cx.notify();
    }

    /// Persist the terminal working copy to `terminal.toml`. The watcher
    /// reloads + repaints; we never set the global here.
    pub(super) fn persist_terminal(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::terminal_settings::save(&self.terminal) {
            tracing::warn!(%err, "settings modal: failed to write terminal.toml");
        }
        cx.notify();
    }

    /// Persist the AI working copy to `commit_message_ai.toml`.
    pub(super) fn persist_ai(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::commit_message_ai_settings::save(&self.ai) {
            tracing::warn!(%err, "settings modal: failed to write commit_message_ai.toml");
        }
        cx.notify();
    }
}

impl Focusable for SettingsModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
