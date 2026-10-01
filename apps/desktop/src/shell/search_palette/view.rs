//! `SearchPalette` — the overlay entity. Owns the query input, the frozen
//! [`PaletteState`] for the current open, the project-filter popover and the
//! scroll handle; forwards keys into the state machine and emits
//! [`SearchPaletteEvent`]s. Activation is `WorkspaceRoot`'s job, so this view
//! never mutates another entity.

use std::rc::Rc;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, Render, ScrollHandle, Subscription, Window, div,
};
use gpui_component::input::{
    Enter as InputEnter, Escape as InputEscape, InputEvent, InputState, MoveDown, MoveUp,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{SearchPaletteFocusFilter, SearchPaletteQuickSelect};
use crate::shell::search_palette::keys::SEARCH_PALETTE_KEY_CONTEXT;
use crate::shell::search_palette::model::{Snapshot, Target};
use crate::shell::search_palette::render_chrome::{ChromeInput, build_overlay};
use crate::shell::search_palette::state::{Outcome, PaletteState};

const PLACEHOLDER: &str = "Search chats, terminals, worktrees, settings, and actions\u{2026}";

pub enum SearchPaletteEvent {
    /// The palette closed. `restore` is the focus to hand back (Esc, or an
    /// action that must run against the surface the user came from); `None`
    /// lets `WorkspaceRoot` take focus while it activates the target.
    Closed { restore: Option<FocusHandle> },
    Activate(Target),
}

impl EventEmitter<SearchPaletteEvent> for SearchPalette {}

pub struct SearchPalette {
    /// `Some` exactly while open: the snapshot taken at open time plus the
    /// query / selection / filter state over it.
    state: Option<PaletteState>,
    filter_open: bool,
    /// Highlighted project row in the filter popover.
    filter_cursor: usize,
    query_input: Option<Entity<InputState>>,
    _query_sub: Option<Subscription>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    /// Focus at open time, handed back on Esc.
    prior_focus: Option<FocusHandle>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl SearchPalette {
    pub fn new(theme: Theme, density: Density, typography: Typography, cx: &mut Context<Self>) -> Self {
        Self {
            state: None,
            filter_open: false,
            filter_cursor: 0,
            query_input: None,
            _query_sub: None,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            prior_focus: None,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.is_some()
    }

    /// Open over `snapshot`, focusing the query field. `restore` is the focus
    /// to hand back on Esc (`None` → the root takes it).
    pub fn open(
        &mut self,
        snapshot: Snapshot,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prior_focus = restore;
        self.state = Some(PaletteState::new(snapshot));
        self.filter_open = false;
        self.filter_cursor = 0;
        let input = self.ensure_query_input(window, cx);
        input.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(PLACEHOLDER, window, cx);
        });
        let input_focus = input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
        // Opened from a rail-row mouse-down, the synchronous focus is
        // clobbered by GPUI's post-click focus dispatch; re-assert next frame.
        cx.defer_in(window, move |this, window, cx| {
            if this.is_open() && !this.filter_open {
                window.focus(&input_focus, cx);
            }
        });
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    /// Dismiss (Esc / click outside / ⌘J again). Hands focus back to where
    /// the user was.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        let restore = self.prior_focus.clone();
        self.finish(restore, cx);
    }

    /// Close because another overlay is opening: no event, so no focus
    /// hand-back can land after (and steal from) the overlay taking over.
    pub fn hand_off(&mut self, cx: &mut Context<Self>) {
        self.state = None;
        self.filter_open = false;
        self.prior_focus = None;
        cx.notify();
    }

    fn finish(&mut self, restore: Option<FocusHandle>, cx: &mut Context<Self>) {
        // Emit only on a real open→closed transition: `close` also runs on an
        // already-closed palette before other overlays open, and a stray
        // Closed would steal focus from the overlay that just opened.
        if self.state.take().is_some() {
            cx.emit(SearchPaletteEvent::Closed { restore });
        }
        self.filter_open = false;
        self.prior_focus = None;
        cx.notify();
    }

    fn ensure_query_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.query_input {
            return input.clone();
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        let sub = cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, _window, cx| {
            if matches!(ev, InputEvent::Change) {
                let value = input.read(cx).value().to_string();
                if let Some(state) = this.state.as_mut() {
                    state.set_query(&value);
                    this.scroll.scroll_to_item(0);
                    cx.notify();
                }
            }
        });
        self.query_input = Some(input.clone());
        self._query_sub = Some(sub);
        input
    }

    fn nav(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(state) = self.state.as_mut() {
            state.move_selection(delta);
            if let Some(i) = state.selected {
                self.scroll.scroll_to_item(i);
            }
            cx.notify();
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(idx) = self.state.as_ref().and_then(|s| s.selected) {
            self.activate_at(idx, cx);
        }
    }

    pub(super) fn activate_at(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(outcome) = self.state.as_mut().and_then(|s| s.activate_at(idx)) else {
            return;
        };
        match outcome {
            Outcome::Run(target) => self.run(target, cx),
            Outcome::Expanded => {
                if let Some(i) = self.state.as_ref().and_then(|s| s.selected) {
                    self.scroll.scroll_to_item(i);
                }
                cx.notify();
            }
        }
    }

    fn run(&mut self, target: Target, cx: &mut Context<Self>) {
        // An action dispatches against the focused surface, so it needs the
        // user's original focus back first; everything else lands its own.
        let restore = matches!(target, Target::Action(_))
            .then(|| self.prior_focus.clone())
            .flatten();
        self.finish(restore, cx);
        cx.emit(SearchPaletteEvent::Activate(target));
    }

    fn quick_select(&mut self, digit: u8, cx: &mut Context<Self>) {
        if let Some(target) = self.state.as_ref().and_then(|s| s.quick_select(digit)) {
            self.run(target, cx);
        }
    }

    // ── project filter popover ────────────────────────────────────────

    pub(super) fn toggle_filter_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.as_ref().is_none_or(|s| s.snapshot.project_count() <= 1) {
            return;
        }
        self.filter_open = !self.filter_open;
        // While the popover is up the palette root holds focus, so arrows,
        // Space and Enter drive the checklist instead of the query field.
        if self.filter_open {
            window.focus(&self.focus_handle, cx);
        } else if let Some(input) = &self.query_input {
            let f = input.read(cx).focus_handle(cx);
            window.focus(&f, cx);
        }
        cx.notify();
    }

    pub(super) fn toggle_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if let Some(state) = self.state.as_mut() {
            state.toggle_project_filter(project_id);
            self.scroll.scroll_to_item(0);
            cx.notify();
        }
    }

    pub(super) fn clear_filter(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.state.as_mut() {
            state.clear_filter();
            cx.notify();
        }
    }

    fn filter_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .state
            .as_ref()
            .map(|s| s.snapshot.projects.iter().map(|p| p.project_id.clone()).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        match key {
            "up" => self.filter_cursor = (self.filter_cursor + ids.len() - 1) % ids.len(),
            "down" => self.filter_cursor = (self.filter_cursor + 1) % ids.len(),
            "enter" | "space" => {
                let id = ids[self.filter_cursor.min(ids.len() - 1)].clone();
                self.toggle_project(&id, cx);
            }
            // Tab arrives as `SearchPaletteFocusFilter` (bound in this context).
            "escape" => self.toggle_filter_popover(window, cx),
            _ => return,
        }
        cx.notify();
    }
}

impl Focusable for SearchPalette {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SearchPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let Some(state) = self.state.as_ref() else {
            return div().into_any_element();
        };
        let entity = cx.entity();
        let activate_entity = entity.clone();
        let on_activate = Rc::new(move |idx: usize, _w: &mut Window, cx: &mut App| {
            activate_entity.update(cx, |p, cx| p.activate_at(idx, cx));
        });
        build_overlay(ChromeInput {
            state,
            query_input: self.query_input.as_ref(),
            filter_open: self.filter_open,
            filter_cursor: self.filter_cursor,
            scroll: &self.scroll,
            entity,
            on_activate,
            viewport: window.viewport_size(),
            theme: self.theme,
            density: self.density,
            typography: &self.typography,
            motion: crate::motion_settings::active(cx),
        })
        .track_focus(&self.focus_handle)
        .key_context(SEARCH_PALETTE_KEY_CONTEXT)
        // The focused `Input` turns nav keys into its own actions before
        // ancestor key listeners run, so they are intercepted at the capture
        // phase (same contract as the Command Palette). Known trade-off:
        // capturing Escape pre-empts IME un-marking, so Esc mid-composition
        // closes the palette.
        .capture_action(cx.listener(|this, _: &InputEscape, _window, cx| {
            cx.stop_propagation();
            this.close(cx);
        }))
        .capture_action(cx.listener(|this, _: &InputEnter, _window, cx| {
            cx.stop_propagation();
            this.confirm(cx);
        }))
        .capture_action(cx.listener(|this, _: &MoveUp, _window, cx| {
            cx.stop_propagation();
            this.nav(-1, cx);
        }))
        .capture_action(cx.listener(|this, _: &MoveDown, _window, cx| {
            cx.stop_propagation();
            this.nav(1, cx);
        }))
        .on_action(cx.listener(|this, _: &SearchPaletteFocusFilter, window, cx| {
            this.toggle_filter_popover(window, cx);
        }))
        .on_action(cx.listener(|this, action: &SearchPaletteQuickSelect, _window, cx| {
            this.quick_select(action.digit, cx);
        }))
        // Only while the filter popover holds focus (the input is unfocused
        // then, so none of the capture handlers above fire).
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if this.filter_open {
                this.filter_key(event.keystroke.key.as_str(), window, cx);
            }
        }))
        .into_any_element()
    }
}
