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
mod pane_agents_launch;
mod pane_keybindings;
mod pane_notifications;
mod pane_terminal;
mod pane_voice;
mod view;

pub use nav::SettingsPane;

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Point,
    Subscription, Window, point, px,
};
use gpui_component::input::{InputEvent, InputState};
use oximux_settings::{
    AgentLaunchSettings, CommitMessageAiSettings, Density, DictationSettings, TerminalSettings,
    Theme, Typography,
};
use oximux_storage::SettingsRepo;

use crate::notifier::{AgentNotifySettings, Notifier};

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
    /// Working copy of the per-agent launch defaults; reseeded from the live
    /// global at each `open()`. Edits mutate this, then write
    /// `agent_launch.toml`; the watcher reloads + swaps the global.
    pub(crate) agent_launch: AgentLaunchSettings,
    /// Working copy of the voice-dictation settings; reseeded from the live
    /// global at each `open()`. Edits mutate this, then write `dictation.toml`.
    pub(crate) dictation: DictationSettings,
    /// Live notification prefs shared with the notifier. The Agents pane
    /// toggles flip these atomics directly (interior mutability) so the
    /// change takes effect on the next dispatch without a reload.
    pub(crate) notify: Arc<AgentNotifySettings>,
    /// Flat KV store the notification toggles persist into (keys in
    /// [`crate::notifier::keys`]), so prefs survive a restart.
    pub(crate) notify_repo: SettingsRepo,
    /// Live dispatch sink for the test-notification button (and the
    /// availability hint next to it).
    pub(crate) notifier: Arc<dyn Notifier>,
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
    /// Voice pane's custom-words editor input. Lazily built on `open()` (needs a
    /// `Window`), seeded from `dictation.custom_words`. `None` before first open.
    pub(super) custom_words_input: Option<Entity<InputState>>,
    /// Keeps the custom-words `InputEvent::Change` → parse+persist subscription
    /// alive.
    _custom_words_sub: Option<Subscription>,
    /// The custom-words value at `open()`, so `close()` can flush a pending edit
    /// that was typed but never committed via blur/Enter (clicking dead space
    /// doesn't blur a gpui input) — otherwise closing the modal would silently
    /// drop the typed dictionary.
    custom_words_seed: Vec<String>,
    /// Working copy of the `keybindings.toml` override map; reseeded from
    /// disk at each `open()`. Edits persist + apply to the live keymap
    /// immediately (see `pane_keybindings`).
    pub(crate) keybind_overrides: BTreeMap<String, String>,
    /// Action id currently capturing a new chord, if any. While set, a
    /// keystroke interceptor swallows every key press app-wide so a bound
    /// chord can be recorded without dispatching its action.
    pub(crate) recording_action: Option<&'static str>,
    /// The interceptor subscription — alive exactly while recording.
    pub(super) recording_sub: Option<Subscription>,
}

/// Parse the custom-words editor field into a de-duplicated dictionary. Splits
/// on commas and newlines only (NOT spaces) so a multi-word entry like
/// "New York" can be one dictionary unit — the matcher collapses spaces when
/// scoring. Trims, drops blanks, keeps first-seen order (case-insensitive
/// dedup). `sanitized()` re-applies the same cleanup on load.
pub(super) fn parse_custom_words(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .filter(|w| seen.insert(w.to_lowercase()))
        .map(str::to_string)
        .collect()
}

impl SettingsModal {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        notify: Arc<AgentNotifySettings>,
        notify_repo: SettingsRepo,
        notifier: Arc<dyn Notifier>,
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
            agent_launch: AgentLaunchSettings::default(),
            dictation: DictationSettings::default(),
            notify,
            notify_repo,
            notifier,
            pos: None,
            drag_grab: None,
            search_input: None,
            _search_sub: None,
            custom_words_input: None,
            _custom_words_sub: None,
            custom_words_seed: Vec::new(),
            keybind_overrides: BTreeMap::new(),
            recording_action: None,
            recording_sub: None,
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
        self.agent_launch = cx
            .try_global::<AgentLaunchSettings>()
            .cloned()
            .unwrap_or_default();
        self.dictation = cx
            .try_global::<DictationSettings>()
            .cloned()
            .unwrap_or_default();
        // Reseed the keybinding overrides from disk (hand edits since boot
        // show up here; load problems were already toasted at boot).
        self.keybind_overrides = crate::keybindings_settings::load_overrides().0;
        self.recording_action = None;
        self.recording_sub = None;
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

        // Voice pane's custom-words editor: a comma/space separated field seeded
        // from the working copy. On every edit, reparse into `custom_words` and
        // persist (the watcher reloads the global) so the dictionary applies to
        // the next dictation without a modal round-trip.
        self.custom_words_seed = self.dictation.custom_words.clone();
        let seed = self.dictation.custom_words.join(", ");
        let cw_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. OxiMux, ChargeBee, ChatGPT")
                .default_value(seed)
        });
        // Update the in-memory working copy on every keystroke, but only PERSIST
        // (disk write + watcher reload) when the field is committed — on blur or
        // Enter — so typing a dictionary entry isn't a per-character fs::write on
        // the main thread. This matches the modal's persist-on-discrete-action
        // convention (the search field, three lines up, likewise does no I/O).
        self._custom_words_sub =
            Some(
                cx.subscribe(&cw_input, |this, input, ev: &InputEvent, cx| match ev {
                    InputEvent::Change => {
                        let raw = input.read(cx).value().to_string();
                        this.dictation.custom_words = parse_custom_words(&raw);
                    }
                    InputEvent::Blur | InputEvent::PressEnter { .. } => {
                        this.persist_voice(cx);
                    }
                    _ => {}
                }),
            );
        self.custom_words_input = Some(cw_input);

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
        // Emit Closed only on a real open→closed transition. `close_modal_overlays`
        // closes this modal before opening any overlay; an unconditional emit
        // queues a workspace root-refocus that steals focus from a just-opened
        // modal. See the matching guard in `command_palette::PaletteModal::close`.
        let was_open = self.open;
        self.open = false;
        // Flush a custom-words edit that was typed but never committed (blur/Enter)
        // before dropping the input — the working copy is synced on every
        // keystroke, so persist only when it actually changed since open. Without
        // this, typing a dictionary then closing via ✕/Esc would drop the edit.
        if self.custom_words_input.is_some() && self.dictation.custom_words != self.custom_words_seed
        {
            self.persist_voice(cx);
        }
        // Drop the search input so its focus handle can't keep window focus
        // orphaned after the modal is hidden.
        self.search_input = None;
        self._search_sub = None;
        self.custom_words_input = None;
        self._custom_words_sub = None;
        // A close mid-recording must release the keystroke interceptor or
        // every subsequent key press in the app would be swallowed.
        self.recording_action = None;
        self.recording_sub = None;
        if was_open {
            cx.emit(SettingsModalEvent::Closed);
        }
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

    /// Persist the per-agent launch working copy to `agent_launch.toml`. The
    /// watcher reloads + swaps the global; we never set the global here.
    pub(super) fn persist_agent_launch(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::agent_launch_settings::save(&self.agent_launch) {
            tracing::warn!(%err, "settings modal: failed to write agent_launch.toml");
        }
        cx.notify();
    }

    /// Persist the voice working copy to `dictation.toml`. The watcher reloads +
    /// swaps the global; we never set the global here.
    pub(super) fn persist_voice(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = crate::dictation_settings::save(&self.dictation) {
            tracing::warn!(%err, "settings modal: failed to write dictation.toml");
        }
        cx.notify();
    }

    /// Persist one notification pref to the flat settings store as
    /// `"true"`/`"false"`. The live atomic is flipped by the caller before
    /// this runs; persistence only makes the choice survive a restart.
    pub(super) fn persist_notify(&mut self, key: &str, value: bool, cx: &mut Context<Self>) {
        let v = if value { "true" } else { "false" };
        if let Err(err) = self.notify_repo.set(key, v) {
            tracing::warn!(%err, key, "settings modal: failed to persist notification pref");
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
