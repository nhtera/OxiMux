//! The bottom composer row of the agent chat — a status line, the multi-line
//! input, and a Send button — isolated into its OWN entity/view.
//!
//! Why a separate entity: a text input only repaints its typed characters when
//! the view that OWNS it calls `cx.notify()` on each `Change` (gpui-component's
//! `InputState` does not self-repaint when embedded via `Input::new`). If that
//! `notify` lived on `AgentChatView`, every keystroke would rebuild the entire
//! transcript (every bubble + tool card) — visible typing lag. By owning the
//! input here, a keystroke dirties only THIS view; the transcript above stays
//! cached. Submit is surfaced to the parent as a [`ComposerEvent`].

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, SharedString, Styled, Subscription, Window,
    div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use oximux_settings::{Density, Theme, Typography};

/// Raised when the user submits the draft (Enter or the Send button). The
/// parent [`super::AgentChatView`] performs the actual send; the composer has
/// already cleared its input by the time this fires.
pub enum ComposerEvent {
    Submit(String),
}

pub struct ComposerView {
    input: Entity<InputState>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Mirrors the parent's connection state, for the status line + Send button.
    disconnected: bool,
    turn_active: bool,
    /// Repaints this view (only) on each keystroke so the draft stays visible.
    _sub: Subscription,
}

impl EventEmitter<ComposerEvent> for ComposerView {}

impl ComposerView {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            // Auto-grow (not fixed multi-line): starts as a single row and grows
            // with the draft up to 8 rows before scrolling. Fixed `multi_line`
            // reserves 2 rows of scroll space, which shows a scrollbar even on an
            // empty/one-line prompt. `max_rows > 1` keeps it a multi-line field.
            InputState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("Message Claude…  (↵ to send)")
        });
        let sub = cx.subscribe(&input, |_this, _input, ev: &InputEvent, cx| {
            // Repaint ONLY the composer on edits — the transcript is untouched.
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            input,
            theme,
            density,
            typography,
            disconnected: false,
            turn_active: false,
            _sub: sub,
        }
    }

    /// The inner input's focus handle — the parent focuses this on activate so
    /// keystrokes land in the draft without a click first.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// Sync the parent's connection/turn state (drives the status line + whether
    /// Send is enabled). Only repaints when something actually changed.
    pub fn set_state(&mut self, disconnected: bool, turn_active: bool, cx: &mut Context<Self>) {
        if self.disconnected != disconnected || self.turn_active != turn_active {
            self.disconnected = disconnected;
            self.turn_active = turn_active;
            cx.notify();
        }
    }

    /// Read + clear the draft, emitting [`ComposerEvent::Submit`] when it's a
    /// non-empty message and the agent is still connected.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        let text = text.trim().to_string();
        if text.is_empty() || self.disconnected {
            return;
        }
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.emit(ComposerEvent::Submit(text));
    }
}

impl Render for ComposerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let can_send = !self.disconnected;
        let status = if self.disconnected {
            "Disconnected — the agent process exited."
        } else if self.turn_active {
            "Claude is working…"
        } else {
            "Press ↵ to send · or click Send"
        };
        let status_color = if self.disconnected {
            theme.status_error
        } else {
            theme.fg_subtle
        };
        // Filled, unmissable Send button. Keyboard send (↵) can be swallowed by
        // some input methods (e.g. Vietnamese Telex eats Enter before the app
        // sees it), so a reliable mouse target is the primary send affordance.
        let (send_bg, send_fg) = if can_send {
            (theme.status_info, theme.bg_base)
        } else {
            (theme.bg_panel_alt, theme.fg_subtle)
        };
        let send_button = div()
            .id("agent-chat-send")
            .flex()
            .items_center()
            .justify_center()
            .px(px(18.0))
            .py(px(8.0))
            .rounded(px(density.r_chip))
            .bg(send_bg)
            .text_color(send_fg)
            .text_size(px(typo.t_body_sm))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, window, cx| this.submit(window, cx)),
            )
            .child(SharedString::from("Send"));

        div()
            .flex()
            .flex_col()
            .w_full()
            .border_t_1()
            .border_color(theme.border_inactive)
            .p(px(density.pad_panel))
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typo.t_label_xs))
                    .text_color(status_color)
                    .child(SharedString::from(status.to_string())),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(density.gap_inline))
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.input).text_size(px(typo.t_body_md))),
                    )
                    .child(send_button),
            )
    }
}
