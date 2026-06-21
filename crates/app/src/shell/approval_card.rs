//! ApprovalCard — quick-reply overlay for an agent's blocking approval prompt.
//!
//! When an agent session's status becomes `NeedsApproval`, the host
//! `PaneGroup` mounts this card over the agent pane. The buttons answer the
//! agent's own TUI prompt by sending pre-formed bytes to its PTY through the
//! `QuickReplyToAgent` action (explicit session routing — a background agent's
//! card always replies to ITS OWN prompt, never the focused tab). This is a
//! fire-and-forget keystroke send: the agent's readline is already blocked
//! waiting for input, so the bytes land exactly as if typed. No relay change,
//! no hook blocking.
//!
//! Approve/Deny are shown only for the Claude Code adapter. Its approval menu
//! always lists `1. Yes` first and pre-highlighted (so `1`+Enter approves),
//! and `Esc` cancels the prompt no matter how many options it shows. Every
//! adapter also gets a free-text reply for prompts that expect typed input.
//!
//! Auto-dismiss is host-driven: `PaneGroup` drops the card the moment the
//! status leaves `NeedsApproval`. The internal `ApprovalState` machine only
//! guards against a double-send when an approval prompt re-appears immediately
//! after a reply (an unconsumed or rapidly-repeated prompt).

use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, SharedString, Styled, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_core::{AgentAdapter, AgentSessionId, AgentStatus};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::QuickReplyToAgent;
use crate::ui::FloatingSurface;

/// Bytes the Claude Code approval menu consumes in raw-PTY mode. Option `1`
/// ("Yes") is always first and pre-highlighted, so `1` + carriage return
/// (raw-mode Enter) approves. Deny sends `Esc`, which the menu maps to cancel
/// for ANY option count — unlike a fixed `2`, which on the 3-option menu
/// selects "Yes, and don't ask again" (an approve that also writes a permanent
/// allow-rule). Confirmed against a captured live session — see
/// `tests/fixtures/claude-approval-bytes.txt`.
pub const APPROVE_BYTES: &[u8] = b"1\r";
pub const DENY_BYTES: &[u8] = b"\x1b";

/// Maximum free-text reply length accepted from the field. Bounds the bytes
/// written to the PTY; longer input is truncated in the send path.
const FREETEXT_MAX_CHARS: usize = 512;

/// After a reply is sent, the controls disarm for this long so a prompt still
/// on screen (not yet consumed by the agent) can't be answered twice by an
/// over-eager double click. The window then re-arms on its own — there is no
/// path that leaves the card permanently inoperable.
const SEND_DEBOUNCE: Duration = Duration::from_millis(1000);

/// Replace every control character (ESC, CR, LF, TAB, …) with a space before a
/// free-text reply reaches the PTY. This neutralises two hazards at once: a
/// terminal escape sequence smuggled into the byte stream, and an embedded
/// newline (e.g. from a paste) that would submit the line early. A space
/// (rather than removal) keeps word boundaries readable; the single intended
/// line terminator is appended by the caller.
fn sanitize_reply(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Double-send guard. After a reply goes out the controls disarm for
/// `SEND_DEBOUNCE`, then re-arm by elapsed time — so a slow or persistent
/// prompt can never strand the card disabled. The send methods re-check this
/// at click time, so a double-send is blocked even if a stale frame still
/// paints the buttons as live. Pure logic, unit-tested without a GPUI window.
#[derive(Debug, Clone, PartialEq)]
enum ApprovalState {
    /// Controls are live — ready to send.
    Idle,
    /// A reply was just sent; controls disarmed until `sent_at + SEND_DEBOUNCE`.
    Pending { sent_at: Instant },
}

impl ApprovalState {
    fn on_send(&mut self, now: Instant) {
        *self = ApprovalState::Pending { sent_at: now };
    }

    fn can_send(&self, now: Instant) -> bool {
        match self {
            ApprovalState::Idle => true,
            ApprovalState::Pending { sent_at } => now.duration_since(*sent_at) >= SEND_DEBOUNCE,
        }
    }
}

pub struct ApprovalCard {
    session_id: AgentSessionId,
    /// Numeric Approve/Deny buttons are Claude-Code-specific (its menu is
    /// numbered); other adapters get the free-text reply only.
    numeric_menu: bool,
    /// The `NeedsApproval(reason)` string, shown so the user knows what is
    /// being approved without switching to the pane.
    reason: String,
    state: ApprovalState,
    freetext: Entity<InputState>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ApprovalCard {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: AgentSessionId,
        adapter: AgentAdapter,
        reason: String,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let freetext =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type a reply to the agent…"));
        Self {
            session_id,
            numeric_menu: matches!(adapter, AgentAdapter::ClaudeCode),
            reason,
            state: ApprovalState::Idle,
            freetext,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
        }
    }

    pub fn session_id(&self) -> AgentSessionId {
        self.session_id
    }

    /// Host hook: refresh the displayed reason from the agent's live status
    /// each render so a card reused across two prompts of the same session
    /// (e.g. workspace-trust → tool-approval) shows the current ask. The
    /// double-send guard is time-based and needs no status input.
    pub fn note_status(&mut self, status: &AgentStatus) {
        if let AgentStatus::NeedsApproval(reason) = status
            && self.reason != *reason
        {
            self.reason = reason.clone();
        }
    }

    fn dispatch_reply(&mut self, reply_bytes: String, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(
            Box::new(QuickReplyToAgent {
                session_id: self.session_id,
                reply_bytes,
            }),
            cx,
        );
        self.state.on_send(Instant::now());
        cx.notify();
    }

    fn send_bytes(&mut self, bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.can_send(Instant::now()) {
            return;
        }
        self.dispatch_reply(String::from_utf8_lossy(bytes).into_owned(), window, cx);
    }

    fn send_freetext(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.can_send(Instant::now()) {
            return;
        }
        let raw = self.freetext.read(cx).value().to_string();
        let cleaned: String = sanitize_reply(&raw).chars().take(FREETEXT_MAX_CHARS).collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            return;
        }
        let reply_bytes = format!("{cleaned}\r");
        // Clear the field so a re-shown card starts empty.
        self.freetext
            .update(cx, |s, cx| s.set_value(SharedString::from(""), window, cx));
        self.dispatch_reply(reply_bytes, window, cx);
    }
}

impl Focusable for ApprovalCard {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ApprovalCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let can_send = self.state.can_send(Instant::now());
        let numeric = self.numeric_menu;
        let reason = self.reason.clone();

        // Header: amber attention label + the prompt reason.
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.status_warn)
                    .child(SharedString::from("⚠ Approval needed")),
            )
            .when(!reason.is_empty(), |s| {
                s.child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .child(SharedString::from(reason)),
                )
            });

        // Status hint flips between the keyboard tip and the post-send wait.
        let hint = if can_send {
            "⌘↵ to send reply"
        } else {
            "Reply sent — waiting for the agent…"
        };

        // Numeric Approve/Deny row, Claude Code only.
        let numeric_row = numeric.then(|| {
            div()
                .flex()
                .flex_row()
                .gap(px(density.gap_inline))
                .child(
                    Button::new("approval-approve")
                        .success()
                        .label("Approve · 1")
                        .disabled(!can_send)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.send_bytes(APPROVE_BYTES, window, cx);
                        })),
                )
                .child(
                    Button::new("approval-deny")
                        .danger()
                        .label("Deny · Esc")
                        .disabled(!can_send)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.send_bytes(DENY_BYTES, window, cx);
                        })),
                )
        });

        // Free-text reply row — field + Send button, available for every
        // adapter (the only path for non-numeric prompts).
        let freetext_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(density.gap_inline))
            .child(div().flex_1().child(Input::new(&self.freetext)))
            .child(
                Button::new("approval-send")
                    .primary()
                    .label("Send")
                    .disabled(!can_send)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.send_freetext(window, cx);
                    })),
            );

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Cmd/Ctrl+Enter sends the free-text reply; a bare Enter is
                // left to the field so it never auto-submits a half-typed line.
                if event.keystroke.key.as_str() == "enter"
                    && (event.keystroke.modifiers.platform || event.keystroke.modifiers.control)
                {
                    this.send_freetext(window, cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                // Swallow clicks inside the card so they don't reach the
                // terminal beneath (which would move the agent's cursor).
            })
            .flex()
            .flex_col()
            .w(px(460.0))
            .p(px(density.pad_panel * 2.0))
            .gap(px(density.gap_inline))
            .floating_chrome(&theme, &density)
            .child(header)
            .when_some(numeric_row, |s, row| s.child(row))
            .child(freetext_row)
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(hint)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_and_deny_bytes_match_live_prompt() {
        // Locked against a captured live Claude Code prompt. Option 1 ("Yes")
        // is first + pre-highlighted, so `1` + CR (raw-mode Enter) approves.
        // Deny is `Esc`, which cancels for any option count — a fixed `2` would
        // pick "Yes, and don't ask again" on the 3-option menu (approve + rule).
        assert_eq!(APPROVE_BYTES, b"1\r");
        assert_eq!(DENY_BYTES, b"\x1b");
    }

    #[test]
    fn debounce_blocks_double_send_then_rearms() {
        let t0 = Instant::now();
        let mut state = ApprovalState::Idle;
        assert!(state.can_send(t0), "fresh card is ready to send");

        // User clicks Approve — bytes go out, controls disarm.
        state.on_send(t0);
        assert!(
            !state.can_send(t0 + Duration::from_millis(100)),
            "a rapid second click within the window is blocked",
        );

        // After the debounce window the controls re-arm on their own — even if
        // the prompt is still on screen, the card is never permanently stuck.
        assert!(state.can_send(t0 + SEND_DEBOUNCE), "re-arms at the window edge");
        assert!(
            state.can_send(t0 + Duration::from_secs(5)),
            "stays armed well past the window",
        );
    }

    #[test]
    fn sanitize_reply_neutralizes_control_chars() {
        // ESC, newline, CR, and tab all collapse to a space — escape-safe and
        // no embedded line terminator can submit the reply early.
        assert_eq!(sanitize_reply("hello\u{1b}world"), "hello world");
        assert_eq!(sanitize_reply("line1\nline2"), "line1 line2");
        assert_eq!(sanitize_reply("a\tb\rc"), "a b c");
        // Plain text is untouched.
        assert_eq!(sanitize_reply("approve please"), "approve please");
    }
}
