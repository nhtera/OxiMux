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
    div, prelude::FluentBuilder, px,
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
            // Multi-line field (so Enter is captured for send and long drafts
            // wrap) but pinned to a fixed height at the render site — see the
            // pill in `render`. `auto_grow`'s `max_rows > 1` keeps it multi-line
            // without the fixed `multi_line`'s reserved 2-row scroll space (which
            // shows a scrollbar even when empty).
            InputState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("Message Claude…  (↵ to send)")
        });
        let sub = cx.subscribe(&input, |_this, _input, ev: &InputEvent, cx| {
            // Repaint ONLY the composer on edits — the transcript is untouched.
            // Focus/Blur repaint too so the pill's border can track focus (a
            // brighter ring while typing), like a native chat field.
            if matches!(ev, InputEvent::Change | InputEvent::Focus | InputEvent::Blur) {
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typo = &self.typography;
        let can_send = !self.disconnected;
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);

        // No status line here: the composer keeps a FIXED footprint so sending
        // (turn start/end) never resizes it. Live turn/disconnect state is shown
        // in the transcript instead — the way a native chat surfaces it —
        // leaving the composer a calm, stable pill.

        // Circular ↑ send button pinned to the bottom-right of the pill. A
        // mouse target is the primary send affordance because keyboard ↵ can be
        // swallowed by some input methods (e.g. Vietnamese Telex eats Enter
        // before the app sees it).
        let (send_bg, send_fg) = if can_send {
            (theme.status_info, theme.bg_base)
        } else {
            (theme.bg_panel_alt, theme.fg_subtle)
        };
        let send_button = div()
            .id("agent-chat-send")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(send_bg)
            .text_color(send_fg)
            .text_size(px(typo.t_body_md))
            .when(can_send, |s| {
                s.cursor_pointer().hover(|s| s.opacity(0.85))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, window, cx| this.submit(window, cx)),
            )
            .child(SharedString::from("↑"));

        // The pill: a rounded, focus-reactive frame holding the borderless input
        // and, inline on the right, the send button. The input is given an
        // EXPLICIT fixed height — the multi-line input element lays out at
        // height:100% of its parent, so without a concrete height it stretches
        // into dead space (auto-grow's content-height is circular in this
        // embedding). A fixed height keeps the composer a stable, compact pill
        // that never resizes when a message is sent; long drafts scroll inside
        // it. `appearance(false)` drops the input's own box so it doesn't nest.
        let pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .w_full()
            .rounded(px(14.0))
            .border_1()
            .border_color(if focused { theme.focus_ring } else { theme.border_input })
            .bg(theme.bg_panel_alt)
            .px(px(density.pad_panel))
            .py(px(density.pad_row))
            .child(
                div().flex_1().child(
                    Input::new(&self.input)
                        .appearance(false)
                        .h(px(24.0))
                        .text_size(px(typo.t_body_md)),
                ),
            )
            .child(send_button);

        div()
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .border_t_1()
            .border_color(theme.border_inactive)
            .p(px(density.pad_panel))
            .child(
                // Match the transcript's centered reading column so the pill
                // lines up with the messages above it on wide windows.
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(super::CONTENT_MAX_W))
                    .child(pill),
            )
    }
}
