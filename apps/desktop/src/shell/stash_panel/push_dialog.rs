//! PushStashDialog — small form modal for `git stash push`.
//!
//! Two body fields: a single-line message input and an
//! "Include untracked files" checkbox. Not a `ConfirmDialog` — that
//! primitive is title + body + buttons, with no room for form fields,
//! so this is its own thing and the destructive callers stay unchanged.
//!
//! Lifecycle mirrors `ConfirmDialog`: caller mounts on demand,
//! observes `is_confirmed()` / `is_cancelled()` to drop the slot.
//! On confirm fires the supplied callback with `(message,
//! include_untracked)`; on cancel fires `on_cancel` if set. Enter
//! triggers confirm; Escape triggers cancel.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, SharedString, Styled, Window, div, px,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputState},
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;
use std::rc::Rc;

/// Callback fired when the user clicks Push with the form filled. The
/// message is `None` when the input was left empty — the caller maps
/// to `repo.stash_push(msg.as_deref(), include_untracked)`. `Rc` (not
/// `Arc`) for the same reason as `ConfirmCallback`: GPUI views run on
/// the single foreground executor.
pub type PushCallback = Rc<dyn Fn(Option<String>, bool, &mut Window, &mut App) + 'static>;

/// Callback fired when the user dismisses the dialog (Esc / Cancel).
pub type CancelCallback = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

pub struct PushStashPrompt {
    pub on_confirm: PushCallback,
    pub on_cancel: Option<CancelCallback>,
}

pub struct PushStashDialog {
    message_input: Entity<InputState>,
    include_untracked: bool,
    on_confirm: Option<PushCallback>,
    on_cancel: Option<CancelCallback>,
    confirmed: bool,
    cancelled: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl PushStashDialog {
    pub fn new(
        prompt: PushStashPrompt,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let PushStashPrompt {
            on_confirm,
            on_cancel,
        } = prompt;
        let message_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Message (optional)"));
        Self {
            message_input,
            include_untracked: false,
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

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Test/inspection helper — current message buffer.
    pub fn message_value(&self, cx: &App) -> String {
        self.message_input.read(cx).value().to_string()
    }

    /// Test/inspection helper — current include-untracked toggle state.
    pub fn include_untracked_value(&self) -> bool {
        self.include_untracked
    }

    fn try_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirmed || self.cancelled {
            return;
        }
        let typed = self.message_input.read(cx).value().to_string();
        let msg = if typed.trim().is_empty() {
            None
        } else {
            Some(typed)
        };
        let include_untracked = self.include_untracked;
        if let Some(cb) = self.on_confirm.take() {
            cb(msg, include_untracked, window, cx);
        }
        self.confirmed = true;
        cx.notify();
    }

    /// Trigger the cancel pathway: fire `on_cancel` (if registered) and
    /// flip `cancelled = true` so the host observer can drop the
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

impl Focusable for PushStashDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PushStashDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let checked = self.include_untracked;
        let dialog_weak = cx.entity().downgrade();

        let title: SharedString = "Push new stash".into();
        let checkbox_label: SharedString = "Include untracked files".into();

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(
                |dlg, event: &KeyDownEvent, window, cx| {
                    // Bubble-phase: only act on the keys we care about
                    // so text input inside the Input widget keeps
                    // working for everything else.
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
            .floating_chrome(&theme, &density)
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Message"),
            )
            .child(Input::new(&self.message_input))
            .child(
                Checkbox::new("push-stash-include-untracked")
                    .checked(checked)
                    .label(checkbox_label)
                    .on_click(move |new_checked: &bool, _window, cx| {
                        let value = *new_checked;
                        let _ = dialog_weak.update(cx, |dlg, cx| {
                            dlg.include_untracked = value;
                            cx.notify();
                        });
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("push-stash-cancel-button")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.cancel(window, cx);
                            })),
                    )
                    .child(
                        Button::new("push-stash-confirm-button")
                            .primary()
                            .label("Push stash")
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.try_confirm(window, cx);
                            })),
                    ),
            )
    }
}
