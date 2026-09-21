//! BranchFromStashDialog — name prompt for `git stash branch <name> <ref>`.
//!
//! Same shape as [`PushStashDialog`](super::push_dialog): a single-line input,
//! a paragraph of copy, Cancel + a primary verb. Not a `ConfirmDialog`, which
//! is title + body + buttons with no room for a field.
//!
//! # The copy has one job the user cannot get anywhere else
//!
//! `git stash branch` creates the branch at the stash's base commit, applies
//! the stash onto it, **and drops the stash**. Nothing on the row says so, and
//! the verb "branch" does not suggest it. People are routinely surprised, so
//! the consumption is stated before the button, not in a toast afterwards.
//!
//! # Validation lives in the op, not here
//!
//! The button is disabled only for an empty name. Whether git will accept the
//! rest is answered by `git check-ref-format --branch` — read-only, and the
//! authority rather than a regex that would drift from the installed binary
//! (see `Repository::is_valid_branch_name`). Running a subprocess per
//! keystroke to grey out a button is not worth it; the op refuses before
//! anything mutating runs and the failure arrives as a toast.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, Window,
};
use gpui_component::input::{Input, InputState};
use oximux_settings::{Density, Theme, Typography};

use super::form;
use std::rc::Rc;

/// Fired with the name the user typed, trimmed and known non-empty.
pub type BranchNameCallback = Rc<dyn Fn(String, &mut Window, &mut App) + 'static>;

pub struct BranchFromStashPrompt {
    pub on_confirm: BranchNameCallback,
    pub on_cancel: Option<super::push_dialog::CancelCallback>,
    /// The stash's message, shown so the user can see which stash they are
    /// branching from — the dialog is modal and the row is behind it.
    pub stash_label: String,
    /// Seed for the input, derived from the stash message by the caller.
    pub suggested_name: String,
}

pub struct BranchFromStashDialog {
    name_input: Entity<InputState>,
    stash_label: String,
    on_confirm: Option<BranchNameCallback>,
    on_cancel: Option<super::push_dialog::CancelCallback>,
    confirmed: bool,
    cancelled: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl BranchFromStashDialog {
    pub fn new(
        prompt: BranchFromStashPrompt,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let BranchFromStashPrompt {
            on_confirm,
            on_cancel,
            stash_label,
            suggested_name,
        } = prompt;
        let name_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Branch name");
            state.set_value(suggested_name.clone(), window, cx);
            state
        });
        // Focus the INPUT, not this dialog's own handle. A gpui-component
        // `Input` only receives text when its own `InputState` handle is
        // focused, and the Escape/Enter key handler below only sees a
        // keystroke once something inside the dialog holds focus — without
        // this the modal opens INERT: typing goes nowhere and Escape does not
        // dismiss it until the user clicks the field. Found live on the
        // rename dialog, then confirmed here.
        window.focus(&name_input.read(cx).focus_handle(cx), cx);
        Self {
            name_input,
            stash_label,
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

    /// Test/inspection helper — current name buffer.
    pub fn name_value(&self, cx: &App) -> String {
        self.name_input.read(cx).value().to_string()
    }

    fn try_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirmed || self.cancelled {
            return;
        }
        let name = self.name_input.read(cx).value().trim().to_string();
        // Enter on an empty field must not fire: the button is disabled for
        // exactly this state and the key path has to agree with it.
        if name.is_empty() {
            return;
        }
        if let Some(cb) = self.on_confirm.take() {
            cb(name, window, cx);
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

impl Focusable for BranchFromStashDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranchFromStashDialog {
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
        let empty = self.name_input.read(cx).value().trim().is_empty();
        let subtitle: SharedString = if self.stash_label.trim().is_empty() {
            "Creates the branch at this stash's base commit.".into()
        } else {
            format!("From “{}”.", self.stash_label.trim()).into()
        };

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
        .child(form::form_title("Branch from stash", &theme, typography))
        .child(form::form_subtitle(subtitle, &theme, typography))
        .child(form::form_field_label("Name", &theme, typography))
        .child(Input::new(&self.name_input))
        .child(form::form_note(CONSUMPTION_NOTE, &theme, typography))
        .child(form::form_buttons(
            "branch-stash-cancel-button",
            "branch-stash-confirm-button",
            "Create branch",
            empty,
            &density,
            cx.listener(|dlg, _: &ClickEvent, window, cx| dlg.cancel(window, cx)),
            cx.listener(|dlg, _: &ClickEvent, window, cx| dlg.try_confirm(window, cx)),
        ))
    }
}

/// The surprise, stated before the button. Verified against git: on success
/// the stash is dropped; on failure the branch may already exist and be
/// checked out, so the copy does not promise the operation is atomic either.
const CONSUMPTION_NOTE: &str =
    "The branch is created at this stash's base commit, the stash is applied onto it, \
     and the stash is removed from the list. If applying fails you will be left on the \
     new branch with the stash still there.";

/// Turn a stash message into a branch-name suggestion.
///
/// Deliberately conservative — lowercase, runs of anything git could object to
/// collapsed to a single `-`, trimmed of leading/trailing separators, capped.
/// It is a seed for a field the user can edit, not a validator: the authority
/// on whether a name is legal is `git check-ref-format`, and the op asks it.
/// An empty result (a message of pure punctuation, or none at all) yields
/// `None` so the caller can fall back rather than seeding an empty field with
/// a stray `-`.
pub fn suggest_branch_name(message: &str) -> Option<String> {
    const MAX_LEN: usize = 40;
    let mut out = String::new();
    for ch in message.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.trim_matches('-').len() >= MAX_LEN {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_becomes_a_kebab_slug() {
        assert_eq!(
            suggest_branch_name("WIP on main: fix the parser"),
            Some("wip-on-main-fix-the-parser".to_string())
        );
    }

    #[test]
    fn punctuation_runs_collapse_and_do_not_bracket_the_name() {
        // A leading `-` is rejected outright by `is_valid_branch_name` (git
        // would read it as an option), and a trailing one is merely ugly —
        // neither should ever reach the field.
        assert_eq!(
            suggest_branch_name("  ***fix***  "),
            Some("fix".to_string())
        );
    }

    #[test]
    fn a_message_of_pure_punctuation_suggests_nothing() {
        assert_eq!(suggest_branch_name("---"), None);
        assert_eq!(suggest_branch_name(""), None);
    }

    #[test]
    fn a_long_message_is_capped() {
        let s = suggest_branch_name(&"a".repeat(200)).expect("some");
        assert!(s.len() <= 40, "got {} chars: {s}", s.len());
    }

    #[test]
    fn a_slug_is_never_left_with_a_trailing_separator_after_the_cap() {
        // The cap can land mid-separator-run; trimming happens after, so the
        // result is still a name git will take.
        let s = suggest_branch_name("fix the thing that broke in the parser yesterday evening")
            .expect("some");
        assert!(!s.starts_with('-') && !s.ends_with('-'), "{s}");
    }
}
