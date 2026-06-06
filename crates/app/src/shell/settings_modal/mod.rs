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
mod layout;
mod nav;
mod segmented;
mod pane_about;
mod pane_agents;
mod pane_keybindings;
mod pane_terminal;
mod view;

pub use nav::SettingsPane;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Point,
    Subscription, Window, point, px,
};
use gpui_component::input::{InputEvent, InputState};
use oximux_settings::{CommitMessageAiSettings, Density, TerminalSettings, Theme, Typography};

/// Emitted when the modal closes (any path: ×, Esc, click-outside, toggle).
/// `WorkspaceRoot` listens and returns keyboard focus to itself so global
/// key bindings keep dispatching.
pub enum SettingsModalEvent {
    Closed,
}

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
    /// Top-left of the card. `None` until the user drags (or the first frame
    /// resolves it), at which point it holds the live, viewport-clamped
    /// position. Reset to `None` on each `open()` so the modal re-centers.
    pub(super) pos: Option<Point<Pixels>>,
    /// Cursor offset within the title bar captured on drag start, so the grab
    /// point stays under the cursor instead of snapping the card to a corner.
    pub(super) drag_grab: Option<Point<Pixels>>,
    /// Live filter input shown at the top of the nav. Lazily built on each
    /// `open()` (needs a `Window`); panes read its value to hide rows that
    /// don't match. `None` before the first open.
    pub(super) search_input: Option<Entity<InputState>>,
    /// Keeps the `InputEvent::Change` → repaint subscription alive.
    _search_sub: Option<Subscription>,
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
            pos: None,
            drag_grab: None,
            search_input: None,
            _search_sub: None,
        }
    }

    /// Current filter text (empty when no input exists or it's blank).
    pub(super) fn search_text(&self, cx: &App) -> String {
        self.search_input
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default()
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
        // Build a fresh (empty) search input each open, and repaint the panes
        // whenever its text changes so the filter is live.
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings"));
        self._search_sub = Some(cx.subscribe(&input, |_this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        }));
        let input_focus = input.read(cx).focus_handle(cx);
        self.search_input = Some(input);
        self.open = true;
        self.pos = None;
        self.drag_grab = None;
        // Focus the search field so typing filters immediately. Esc / the per-
        // pane controls still work (key events bubble to the card handler).
        window.focus(&input_focus, cx);
        cx.notify();
    }

    /// Move the card's top-left to `(x, y)`, clamped so it stays within the
    /// viewport. Used by the title-bar drag.
    pub(super) fn set_pos(&mut self, x: f32, y: f32, window: &Window, cx: &mut Context<Self>) {
        let vp = window.viewport_size();
        let max_x = (f32::from(vp.width) - CARD_WIDTH).max(0.0);
        let max_y = (f32::from(vp.height) - CARD_HEIGHT).max(0.0);
        self.pos = Some(point(px(x.clamp(0.0, max_x)), px(y.clamp(0.0, max_y))));
        cx.notify();
    }

    /// The card's effective top-left: the dragged position if any, else
    /// horizontally centered at the standard top offset.
    pub(super) fn resolved_pos(&self, window: &Window) -> Point<Pixels> {
        self.pos.unwrap_or_else(|| {
            let vp = window.viewport_size();
            let x = ((f32::from(vp.width) - CARD_WIDTH) / 2.0).max(0.0);
            point(px(x), px(MODAL_TOP_OFFSET))
        })
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        // Drop the search input so its focus handle can't keep window focus
        // orphaned after the modal is hidden.
        self.search_input = None;
        self._search_sub = None;
        cx.emit(SettingsModalEvent::Closed);
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

impl EventEmitter<SettingsModalEvent> for SettingsModal {}
