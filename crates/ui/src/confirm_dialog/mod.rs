//! ConfirmDialog — reusable confirm modal with two intensities.
//!
//! - **Type-to-confirm** (`expected` non-empty): the destructive button is
//!   enabled only once the user types `expected` exactly. Used for
//!   irreversible, on-disk operations (revert file, drop stash, remove
//!   worktree) where a reflex click would lose data.
//! - **Plain confirm** (`expected` empty): no text field; the destructive
//!   button is enabled immediately. Used for reversible / non-destructive
//!   operations (e.g. removing a project from the cockpit — files on disk
//!   are untouched).
//!
//! Either way a Cancel button and Escape dismiss the dialog. The callback
//! runs on confirm; the dialog flips `confirmed = true` so the host can
//! drop it from the modal stack.
//!
//! Pure match predicate lives at `logic::is_match` so tests run without
//! GPUI.

pub mod logic;

use crate::confirm_dialog::logic::is_match;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, SharedString, Styled, Window, div, px,
    prelude::FluentBuilder,
};
use gpui_component::{
    Disableable, Sizable,
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
    /// Optional middle action, turning the dialog into a three-way choice
    /// (e.g. unsaved-changes: Save / Discard / Cancel). When present the
    /// primary confirm button renders as the safe default (`primary`) and
    /// this secondary button takes the destructive `danger` styling. Firing
    /// it runs the callback and resolves the dialog like confirm. `None`
    /// keeps the classic two-button (Cancel + danger Confirm) layout.
    pub secondary: Option<ConfirmSecondary>,
}

/// A third "secondary" action for [`ConfirmPrompt`] (the destructive middle
/// option in a three-way prompt).
pub struct ConfirmSecondary {
    pub label: SharedString,
    pub on_click: ConfirmCallback,
}

pub struct ConfirmDialog {
    title: SharedString,
    body: SharedString,
    expected: SharedString,
    confirm_label: SharedString,
    input_state: Entity<InputState>,
    on_confirm: Option<ConfirmCallback>,
    on_cancel: Option<ConfirmCallback>,
    /// Label + callback for the optional destructive middle action. `None`
    /// for the classic two-button layout.
    secondary_label: Option<SharedString>,
    on_secondary: Option<ConfirmCallback>,
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
            secondary,
        } = prompt;
        let (secondary_label, on_secondary) = match secondary {
            Some(ConfirmSecondary { label, on_click }) => (Some(label), Some(on_click)),
            None => (None, None),
        };
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
            secondary_label,
            on_secondary,
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

    /// `true` when this dialog gates confirmation behind typing `expected`.
    /// Empty `expected` → plain confirm (button enabled immediately).
    fn requires_typing(&self) -> bool {
        !self.expected.is_empty()
    }

    pub fn typed_matches(&self, cx: &App) -> bool {
        let typed = self.input_state.read(cx).value().to_string();
        is_match(&typed, &self.expected)
    }

    /// Whether the destructive button may fire right now.
    fn can_confirm(&self, cx: &App) -> bool {
        (!self.requires_typing() || self.typed_matches(cx)) && !self.confirmed
    }

    fn try_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_confirm(cx) {
            return;
        }
        if let Some(cb) = self.on_confirm.take() {
            cb(window, cx);
        }
        self.confirmed = true;
        cx.notify();
    }

    /// Fire the optional middle action (e.g. "Discard") and resolve the
    /// dialog. No-op if no secondary action is registered or already resolved.
    fn try_secondary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirmed || self.cancelled {
            return;
        }
        if let Some(cb) = self.on_secondary.take() {
            cb(window, cx);
            self.confirmed = true;
            cx.notify();
        }
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
        let can_confirm = self.can_confirm(cx);
        let requires_typing = self.requires_typing();
        let prompt = format!("Type {} to confirm:", self.expected);

        let confirm_label = self.confirm_label.clone();
        let secondary_label = self.secondary_label.clone();
        let has_secondary = secondary_label.is_some();
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
            .w(px(360.0))
            .p(px(density.pad_panel + density.pad_overlay))
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
                    .line_height(px(typography.t_body_sm * 1.45))
                    .child(self.body.clone()),
            )
            // Type-to-confirm field — shown only when a match string is
            // required. Plain-confirm dialogs (empty `expected`) skip it.
            .when(requires_typing, |this| {
                this.child(
                    div()
                        .text_size(px(typography.t_label_caps))
                        .text_color(theme.fg_subtle)
                        .child(prompt),
                )
                .child(Input::new(&self.input_state))
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .mt(px(density.pad_panel))
                    .child(
                        Button::new("cancel-button")
                            .small()
                            .outline()
                            .label("Cancel")
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.cancel(window, cx);
                            })),
                    )
                    // Destructive middle action (e.g. "Discard"), present only
                    // for three-way prompts. Always enabled — it doesn't gate
                    // on the type-to-confirm field.
                    .when_some(secondary_label, |row, label| {
                        row.child(
                            Button::new("secondary-button")
                                .small()
                                .danger()
                                .label(label)
                                .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                    dlg.try_secondary(window, cx);
                                })),
                        )
                    })
                    .child(
                        // Primary/safe default when a destructive secondary
                        // exists (Save); otherwise this IS the destructive
                        // action (danger) for the classic two-button prompt.
                        Button::new("confirm-button")
                            .small()
                            .map(|b| if has_secondary { b.primary() } else { b.danger() })
                            .label(confirm_label)
                            .disabled(!can_confirm)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.try_confirm(window, cx);
                            })),
                    ),
            )
    }
}
