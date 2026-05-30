//! ConfirmDialog — reusable type-to-confirm modal.
//!
//! The destructive `Confirm` button is enabled only when the user types
//! the `expected` string exactly. The callback runs on confirm; the dialog
//! flips `confirmed = true` so the host can drop it from the modal stack.
//!
//! Used by:
//!   - revert file (step 11/14) — expected = filename
//!   - drop stash  (step 12)    — expected = "drop"
//!   - remove worktree (step 13) — expected = worktree slug
//!
//! Pure match predicate lives at `logic::is_match` so tests run without
//! GPUI.

pub mod logic;

use crate::shell::confirm_dialog::logic::is_match;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, SharedString, Styled, Window, div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_settings::{Density, Theme, Typography};
use std::rc::Rc;

/// Boxed callback fired when the user clicks Confirm with a matching typed
/// string. `Rc` (not `Arc`) because GPUI views are single-threaded on the
/// foreground executor; `Rc<dyn Fn>` avoids the `Send` bound that the
/// closure would otherwise need.
pub type ConfirmCallback = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Bundle of message + intent fields. Keeps the constructor under clippy's
/// arg-count ceiling and makes the call site read like a builder.
pub struct ConfirmPrompt {
    pub title: SharedString,
    pub body: SharedString,
    pub expected: SharedString,
    pub on_confirm: ConfirmCallback,
    /// Optional override for the destructive button label. Defaults to
    /// `"Confirm"` for back-compat with stash / workspace callers; SCM
    /// discard passes `Some("Delete")` / `"Restore"` / `"Discard"`
    /// keyed off `DiscardKind`.
    pub confirm_label: Option<SharedString>,
    /// Optional callback fired when the user dismisses the dialog
    /// without confirming (Escape, click-outside, explicit cancel).
    /// Use it to clear any host-side "pending request" state so the
    /// dialog can re-mount on the next click.
    pub on_cancel: Option<ConfirmCallback>,
}

pub struct ConfirmDialog {
    title: SharedString,
    body: SharedString,
    expected: SharedString,
    confirm_label: SharedString,
    input_state: Entity<InputState>,
    on_confirm: Option<ConfirmCallback>,
    on_cancel: Option<ConfirmCallback>,
    confirmed: bool,
    cancelled: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ConfirmDialog {
    pub fn new(
        prompt: ConfirmPrompt,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ConfirmPrompt {
            title,
            body,
            expected,
            on_confirm,
            confirm_label,
            on_cancel,
        } = prompt;
        let placeholder = format!("Type {expected} to confirm");
        let input_state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        Self {
            title,
            body,
            expected,
            confirm_label: confirm_label.unwrap_or_else(|| "Confirm".into()),
            input_state,
            on_confirm: Some(on_confirm),
            on_cancel,
            confirmed: false,
            cancelled: false,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
        }
    }

    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    /// `true` once the user has explicitly dismissed the dialog without
    /// confirming. The host observes this flag to drop the dialog from
    /// its slot and clear the matching pending request.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn typed_matches(&self, cx: &App) -> bool {
        let typed = self.input_state.read(cx).value().to_string();
        is_match(&typed, &self.expected)
    }

    fn try_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.typed_matches(cx) {
            return;
        }
        if let Some(cb) = self.on_confirm.take() {
            cb(window, cx);
        }
        self.confirmed = true;
        cx.notify();
    }

    /// Trigger the cancel pathway: fire `on_cancel` (if registered) and
    /// flip `cancelled = true` so the host's observer can drop the
    /// dialog. Idempotent — repeated calls are harmless.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cancelled || self.confirmed {
            return;
        }
        if let Some(cb) = self.on_cancel.take() {
            cb(window, cx);
        }
        self.cancelled = true;
        cx.notify();
    }
}

impl Focusable for ConfirmDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let can_confirm = self.typed_matches(cx) && !self.confirmed;
        let prompt = format!("Type {} to confirm:", self.expected);

        let confirm_label = self.confirm_label.clone();
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(
                |dlg, event: &KeyDownEvent, window, cx| {
                    // Bubble-phase: only act on the keys we care about so
                    // text input inside the Input widget keeps working.
                    match event.keystroke.key.as_str() {
                        "enter" => dlg.try_confirm(window, cx),
                        "escape" => dlg.cancel(window, cx),
                        _ => {}
                    }
                },
            ))
            .flex()
            .flex_col()
            .w(px(440.0))
            .p(px(density.pad_panel * 2.0))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(self.body.clone()),
            )
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child(prompt),
            )
            .child(Input::new(&self.input_state))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("confirm-button")
                            .danger()
                            .label(confirm_label)
                            .disabled(!can_confirm)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.try_confirm(window, cx);
                            })),
                    ),
            )
    }
}
