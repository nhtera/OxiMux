//! Prompt composer bar — an elevated multi-line draft input docked under the
//! active agent pane, with `@file` autocomplete and draft-before-send.
//!
//! Submodules:
//! - [`mention_parser`] — pure `@file` token parsing (no I/O).
//! - [`send_formatter`] — pure draft → PTY bytes (bracketed-paste aware).
//! - [`mention_resolver`] — async file list + fuzzy ranking for the dropdown.
//!
//! The bar itself is a GPUI view. Typing `@` opens a fuzzy file dropdown
//! (ranked over the project's rg file index); the selection inserts an
//! `@path/to/file` token. `⌘↵` submits the draft to the host, which formats it
//! (bracketed paste when the agent shell supports it) and routes it through the
//! existing `SendTextToActiveAgent` path. `Esc` closes the dropdown, then the
//! bar.

pub mod mention_parser;
pub mod mention_resolver;
pub mod send_formatter;

use std::ops::Range;
use std::path::PathBuf;

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::{
    Enter as InputEnter, Escape as InputEscape, Input, InputEvent, InputState, MoveDown, MoveUp,
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;
use mention_parser::pending_mention;
use mention_resolver::{MAX_SUGGESTIONS, rank, scan_candidates};

/// Event the composer emits to its host on dismissal. The host (`PaneGroup`)
/// subscribes, formats + sends a `Submit` draft, and drops the composer.
pub enum ComposerOutcome {
    /// `⌘↵` with a non-empty draft — host formats + sends it.
    Submit(String),
    /// `Esc` (with the dropdown already closed) or a programmatic close.
    Dismiss,
}

pub struct ComposerBar {
    input: Entity<InputState>,
    /// Cached rg file index for `@` autocomplete; scanned once per composer.
    candidates: Vec<String>,
    candidates_loaded: bool,
    /// Current dropdown rows (ranked paths) + cursor.
    suggestions: Vec<String>,
    selected: usize,
    dropdown_open: bool,
    /// Byte span of the in-progress `@query` to replace when a file is chosen.
    pending_range: Option<Range<usize>>,
    closed: bool,
    focus_handle: FocusHandle,
    _input_sub: Subscription,
    _scan_task: Option<Task<()>>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ComposerBar {
    pub fn new(
        root: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Message the agent…  @ to mention a file")
        });
        let input_sub = cx.subscribe_in(&input, window, |this, _input, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.on_input_changed(window, cx);
            }
        });

        // Kick the file index off the tokio runtime (rg via tokio::process),
        // then fold the result back on the UI thread — same shape Quick Open
        // uses. Missing rg / no runtime degrades to an empty list.
        let scan_task = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let (tx, rx) = tokio::sync::oneshot::channel::<Vec<String>>();
                handle.spawn(async move {
                    let _ = tx.send(scan_candidates(root).await);
                });
                Some(cx.spawn(async move |this, cx| {
                    if let Ok(files) = rx.await {
                        let _ = this.update(cx, |c, cx| {
                            c.candidates = files;
                            c.candidates_loaded = true;
                            c.refresh_suggestions(cx);
                            cx.notify();
                        });
                    }
                }))
            }
            Err(_) => None,
        };

        Self {
            input,
            candidates: Vec::new(),
            candidates_loaded: false,
            suggestions: Vec::new(),
            selected: 0,
            dropdown_open: false,
            pending_range: None,
            closed: false,
            focus_handle: cx.focus_handle(),
            _input_sub: input_sub,
            _scan_task: scan_task,
            theme,
            density,
            typography,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// FocusHandle of the inner Input — the host focuses this after mounting so
    /// keystrokes land in the draft without a click first.
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// Recompute the dropdown from the current draft + cursor.
    fn on_input_changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_suggestions(cx);
        cx.notify();
    }

    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        let cursor = self.input.read(cx).cursor();
        match pending_mention(&value, cursor) {
            Some(pm) => {
                self.suggestions = rank(&self.candidates, &pm.query, MAX_SUGGESTIONS);
                self.pending_range = Some(pm.range);
                self.selected = 0;
                self.dropdown_open = true;
            }
            None => {
                self.dropdown_open = false;
                self.pending_range = None;
                self.suggestions.clear();
            }
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.suggestions.is_empty() {
            return;
        }
        let len = self.suggestions.len() as isize;
        self.selected = (((self.selected as isize + delta) % len + len) % len) as usize;
        cx.notify();
    }

    /// Insert the selected suggestion, replacing the in-progress `@query`.
    fn accept_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(range), Some(path)) =
            (self.pending_range.clone(), self.suggestions.get(self.selected).cloned())
        else {
            return;
        };
        let value = self.input.read(cx).value().to_string();
        if range.end > value.len() {
            return;
        }
        let next = format!("{}@{} {}", &value[..range.start], path, &value[range.end..]);
        self.input
            .update(cx, |s, cx| s.set_value(SharedString::from(next), window, cx));
        self.dropdown_open = false;
        self.pending_range = None;
        self.suggestions.clear();
        // Keep typing in the draft after picking a file.
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.input.read(cx).value().to_string();
        if draft.trim().is_empty() {
            return;
        }
        // The host sends + drops us; setting `closed` guards against a second
        // emit if another event lands before the drop completes.
        if self.closed {
            return;
        }
        self.closed = true;
        cx.emit(ComposerOutcome::Submit(draft));
    }

    fn dismiss(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        cx.emit(ComposerOutcome::Dismiss);
    }

    fn render_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let mut list = div()
            .flex()
            .flex_col()
            .w_full()
            .max_h(px(160.0))
            .overflow_hidden()
            .gap(px(1.0));

        if self.suggestions.is_empty() {
            let msg = if self.candidates_loaded {
                "No matching files"
            } else {
                "Scanning files…"
            };
            return list.child(
                div()
                    .px(px(density.pad_panel))
                    .py(px(4.0))
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(msg)),
            );
        }

        for (i, path) in self.suggestions.iter().enumerate() {
            let selected = i == self.selected;
            let row = div()
                .id(("compose-suggestion", i))
                .px(px(density.pad_panel))
                .py(px(4.0))
                .w_full()
                .rounded(px(density.r_card * 0.5))
                .text_size(px(typography.t_body_sm))
                .text_color(if selected { theme.fg_base } else { theme.fg_muted })
                .when(selected, |s| s.bg(theme.hover_overlay))
                .child(SharedString::from(path.clone()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.selected = i;
                    this.accept_selected(window, cx);
                }));
            list = list.child(row);
        }
        list
    }
}

impl EventEmitter<ComposerOutcome> for ComposerBar {}

impl Focusable for ComposerBar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ComposerBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.density, &mut self.typography, cx);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let dropdown = self.dropdown_open;

        div()
            .track_focus(&self.focus_handle)
            // Capture arrows / Enter / Escape BEFORE the InputState consumes
            // them, but only while the dropdown is open — otherwise propagate so
            // the draft field keeps its normal cursor movement and newlines.
            .capture_action(cx.listener(|this, _: &MoveUp, _w, cx| {
                if this.dropdown_open {
                    this.move_selection(-1, cx);
                } else {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _w, cx| {
                if this.dropdown_open {
                    this.move_selection(1, cx);
                } else {
                    cx.propagate();
                }
            }))
            // The Input context binds `enter`→Enter{secondary:false} (newline)
            // and `⌘↵`/`ctrl+↵`→Enter{secondary:true}; both dispatch as the
            // `Enter` action and would be consumed by the field before any
            // `on_key_down` could see them, so submit/accept MUST live here.
            .capture_action(cx.listener(|this, action: &InputEnter, window, cx| {
                if this.dropdown_open {
                    // Enter accepts the highlighted file — must not reach the
                    // field (which would insert a newline).
                    this.accept_selected(window, cx);
                } else if action.secondary {
                    // ⌘↵ submits. Consumed so the field gets no trailing newline.
                    this.submit(window, cx);
                } else {
                    // Bare / Shift+Enter falls through to the field (newline).
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                // Not propagating is load-bearing: it suppresses the field's own
                // Escape handler so the first Esc closes the dropdown and the
                // second dismisses the bar.
                if this.dropdown_open {
                    this.dropdown_open = false;
                    this.suggestions.clear();
                    cx.notify();
                } else {
                    this.dismiss(window, cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_e, _w, _cx| {
                // Don't let a click inside the bar reach the terminal beneath.
            })
            .flex()
            .flex_col()
            .w_full()
            .p(px(density.pad_panel))
            .gap(px(density.gap_inline))
            .floating_chrome(&theme, &density)
            .when(dropdown, |s| s.child(self.render_dropdown(cx)))
            .child(Input::new(&self.input))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(density.gap_inline))
                    .child(
                        div()
                            .text_size(px(typography.t_body_sm))
                            .text_color(theme.fg_subtle)
                            .child(SharedString::from(
                                "⌘↵ send · @ mention a file · ⌘I or Esc to close",
                            )),
                    )
                    // Mouse close, since some input methods (e.g. Vietnamese
                    // Telex) swallow Escape before it reaches a focused field.
                    .child(
                        div()
                            .id("compose-close")
                            .px(px(6.0))
                            .text_size(px(typography.t_body_sm))
                            .text_color(theme.fg_subtle)
                            .hover(|s| s.text_color(theme.fg_base))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| this.dismiss(window, cx)),
                            )
                            .child(SharedString::from("✕ Close")),
                    ),
            )
    }
}
