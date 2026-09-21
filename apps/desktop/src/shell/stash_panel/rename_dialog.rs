//! RenameStashDialog — the new-message prompt for a stash rename.
//!
//! Same shape as its two siblings ([`PushStashDialog`](super::push_dialog) and
//! [`BranchFromStashDialog`](super::branch_dialog)): a single-line input, a
//! paragraph of copy, Cancel + a primary verb. Three peers following one form
//! is this section's established pattern; the shared skeleton behind them is
//! owed an extraction and has not had one yet.
//!
//! # The copy carries a disclosure the other two do not need
//!
//! Git has no `stash rename`. Renaming `stash@{N}` drops every entry down to
//! and including the target and re-stores them, so an operation the user
//! thinks touches one row touches N+1. That is stated here, with the actual
//! count, because it is the difference between "this is a label edit" and
//! "this rewrites the stack's reflog". [`Repository::stash_rename`] carries
//! the recovery contract that makes it safe; the user is told what it costs.
//!
//! # The input takes focus on open
//!
//! Not the card's own handle — a gpui-component `Input` only receives text
//! when its `InputState` handle is focused, and the Escape/Enter handler below
//! only sees a keystroke once something inside the dialog holds focus.
//! Without it the modal opens inert. Verified live: Escape did nothing until
//! the field had been clicked.
//!
//! # Validation lives nowhere
//!
//! Unlike the branch dialog, there is nothing to validate: a stash message is
//! free text and `git stash store -m` takes it literally, quotes, `$` and all.
//! The button is disabled only for an empty field, and only because an empty
//! message would leave the row painting its "(no message)" stand-in — which is
//! a legal state, just never one worth typing on purpose.
//!
//! [`Repository::stash_rename`]: oximux_git::Repository::stash_rename

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, Window,
};
use gpui_component::input::{Input, InputState};
use oximux_settings::{Density, Theme, Typography};

use super::form;
use std::rc::Rc;

/// Fired with the message the user typed, trimmed and known non-empty.
pub type StashMessageCallback = Rc<dyn Fn(String, &mut Window, &mut App) + 'static>;

pub struct RenameStashPrompt {
    pub on_confirm: StashMessageCallback,
    pub on_cancel: Option<super::push_dialog::CancelCallback>,
    /// Seed for the input — the stash's current message, so a rename is an
    /// edit rather than a retype.
    pub current_message: String,
    /// How many entries sit above this one, and therefore how many the
    /// sequence takes off and puts back. Drives the disclosure below.
    pub depth: usize,
}

pub struct RenameStashDialog {
    message_input: Entity<InputState>,
    depth: usize,
    on_confirm: Option<StashMessageCallback>,
    on_cancel: Option<super::push_dialog::CancelCallback>,
    confirmed: bool,
    cancelled: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl RenameStashDialog {
    pub fn new(
        prompt: RenameStashPrompt,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let RenameStashPrompt {
            on_confirm,
            on_cancel,
            current_message,
            depth,
        } = prompt;
        let message_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Stash message");
            state.set_value(current_message.clone(), window, cx);
            state
        });
        // Focus the INPUT, not this dialog's own handle. A gpui-component
        // `Input` only receives text when its own `InputState` handle is
        // focused, and Escape/Enter only reach the key handler below once
        // something inside the dialog has focus at all — without this the
        // modal opens inert: typing goes nowhere and Escape does not dismiss
        // it until the user clicks the field. Found live.
        window.focus(&message_input.read(cx).focus_handle(cx), cx);
        Self {
            message_input,
            depth,
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

    fn try_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirmed || self.cancelled {
            return;
        }
        let message = self.message_input.read(cx).value().trim().to_string();
        // Enter on an empty field must not fire — the button is disabled for
        // exactly this state and the key path has to agree with it.
        if message.is_empty() {
            return;
        }
        if let Some(cb) = self.on_confirm.take() {
            cb(message, window, cx);
        }
        self.confirmed = true;
        cx.notify();
    }

    /// Fire `on_cancel` and flip `cancelled` so the host slot drops us.
    /// Idempotent.
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

impl Focusable for RenameStashDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RenameStashDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(
            &mut self.theme,
            &mut self.density,
            &mut self.typography,
            cx,
        );
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let empty = self.message_input.read(cx).value().trim().is_empty();
        let note: SharedString = rewrite_note(self.depth).into();

        form::form_card(
            &self.focus_handle,
            &theme,
            &density,
            cx.listener(|dlg, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => dlg.try_confirm(window, cx),
                    "escape" => dlg.cancel(window, cx),
                    _ => {}
                }
            }),
        )
        .child(form::form_title("Rename stash", &theme, typography))
        .child(form::form_field_label("Message", &theme, typography))
        .child(Input::new(&self.message_input))
        .child(form::form_note(note, &theme, typography))
        .child(form::form_buttons(
            "rename-stash-cancel-button",
            "rename-stash-confirm-button",
            "Rename",
            empty,
            &density,
            cx.listener(|dlg, _: &ClickEvent, window, cx| dlg.cancel(window, cx)),
            cx.listener(|dlg, _: &ClickEvent, window, cx| dlg.try_confirm(window, cx)),
        ))
    }
}

/// What the rename will actually do to the stack, in the user's terms.
///
/// The count is the point. At the top of the stack this really is a label
/// edit; four entries down it is eleven git calls, and the honest copy
/// changes shape rather than hedging with "may affect other entries".
fn rewrite_note(depth: usize) -> String {
    match depth {
        0 => "Git has no rename, so this stash is removed and re-added under the new message. \
              Its contents and date are unchanged."
            .to_string(),
        1 => "Git has no rename, so this stash and the 1 entry above it are removed and re-added \
              in the same order. Contents and dates are unchanged."
            .to_string(),
        n => format!(
            "Git has no rename, so this stash and the {n} entries above it are removed and \
             re-added in the same order. Contents and dates are unchanged."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_top_of_the_stack_is_described_as_touching_only_itself() {
        let note = rewrite_note(0);
        assert!(note.contains("this stash is removed"), "{note}");
        assert!(!note.contains("above it"), "{note}");
    }

    #[test]
    fn one_entry_above_is_singular() {
        assert!(rewrite_note(1).contains("the 1 entry above it"));
    }

    #[test]
    fn several_entries_above_are_counted_not_hedged() {
        let note = rewrite_note(4);
        assert!(note.contains("the 4 entries above it"), "{note}");
    }

    // The disclosure exists to be specific; a copy that never names a number
    // would be the hedge this replaced.
    #[test]
    fn every_depth_promises_contents_and_dates_survive() {
        for d in [0, 1, 7] {
            assert!(rewrite_note(d).contains("unchanged"), "depth {d}");
        }
    }
}
