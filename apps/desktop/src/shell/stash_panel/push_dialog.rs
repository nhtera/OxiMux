//! PushStashDialog — small form modal for `git stash push`.
//!
//! Two body fields: a single-line message input and an
//! "Include untracked files" checkbox. Not a `ConfirmDialog` — that
//! primitive is title + body + buttons, with no room for form fields,
//! so this is its own thing and the destructive callers stay unchanged.
//!
//! # Scoped mode
//!
//! With a [`PushStashScope`] the same dialog stashes a *selection* rather
//! than the whole worktree: it names the paths, and it takes the
//! include-untracked decision away from the user when the selection needs
//! `-u` to be correct. That is not paternalism — clearing the box makes git
//! refuse the pathspec outright (`did not match any file(s) known to git`)
//! and stash nothing at all, including the tracked files selected beside it.
//! The count is shown so the forced flag is legible rather than mysterious.
//!
//! Lifecycle mirrors `ConfirmDialog`: caller mounts on demand,
//! observes `is_confirmed()` / `is_cancelled()` to drop the slot.
//! On confirm fires the supplied callback with `(message,
//! include_untracked)`; on cancel fires `on_cancel` if set. Enter
//! triggers confirm; Escape triggers cancel.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, Styled, Window, div, px,
};
use gpui_component::{
    Disableable as _,
    checkbox::Checkbox,
    input::{Input, InputState},
};
use oximux_settings::{Density, Theme, Typography};

use super::form;
use std::path::PathBuf;
use std::rc::Rc;

/// How many paths the scoped dialog lists before collapsing the rest into a
/// `+N more` line. Eight rows is about the tallest list that keeps the modal
/// shorter than the smallest supported window at 150% UI scale.
const MAX_LISTED_PATHS: usize = 8;

/// Callback fired when the user clicks Push with the form filled. The
/// message is `None` when the input was left empty — the caller maps
/// to `repo.stash_push(msg.as_deref(), include_untracked, &[])`. `Rc` (not
/// `Arc`) for the same reason as `ConfirmCallback`: GPUI views run on
/// the single foreground executor.
pub type PushCallback = Rc<dyn Fn(Option<String>, bool, &mut Window, &mut App) + 'static>;

/// Callback fired when the user dismisses the dialog (Esc / Cancel).
pub type CancelCallback = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Restricts a push to a named set of paths, and carries the two facts the
/// user cannot see from the path list alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushStashScope {
    /// Repo-relative paths, already sorted and deduped by the planner.
    pub paths: Vec<PathBuf>,
    /// Untracked paths in the selection, for the checkbox label.
    pub untracked_count: usize,
    /// The selection contains a staged rename, so the push runs an unstage
    /// first and the rename comes back unstaged.
    pub has_rename: bool,
    /// `-u` is mandatory for this selection; the checkbox is forced on.
    pub needs_untracked: bool,
}

pub struct PushStashPrompt {
    pub on_confirm: PushCallback,
    pub on_cancel: Option<CancelCallback>,
    /// `None` stashes the whole worktree — the header `+` button's flavour.
    pub scope: Option<PushStashScope>,
}

pub struct PushStashDialog {
    message_input: Entity<InputState>,
    include_untracked: bool,
    scope: Option<PushStashScope>,
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
            scope,
        } = prompt;
        let message_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Message (optional)"));
        // Focus the INPUT, not this dialog's own handle. A gpui-component
        // `Input` only receives text when its own `InputState` handle is
        // focused, and the Escape/Enter key handler below only sees a
        // keystroke once something inside the dialog holds focus — without
        // this the modal opens INERT: typing goes nowhere and Escape does not
        // dismiss it until the user clicks the field. Found live on the
        // rename dialog, then confirmed here.
        window.focus(&message_input.read(cx).focus_handle(cx), cx);
        // Pre-set from the scope so the value is right even if the user hits
        // Enter without ever looking at the checkbox.
        let include_untracked = scope.as_ref().is_some_and(|s| s.needs_untracked);
        Self {
            message_input,
            include_untracked,
            scope,
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

    /// Test/inspection helper — the path scope this dialog was mounted with.
    pub fn scope(&self) -> Option<&PushStashScope> {
        self.scope.as_ref()
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
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let checked = self.include_untracked;
        let dialog_weak = cx.entity().downgrade();

        let scope = self.scope.clone();
        let title: SharedString = match &scope {
            Some(s) if s.paths.len() == 1 => "Stash 1 selected file".into(),
            Some(s) => format!("Stash {} selected files", s.paths.len()).into(),
            None => "Push new stash".into(),
        };
        // The forced flag is stated in the label rather than left to a
        // disabled box the user has to guess about.
        let checkbox_label: SharedString = match &scope {
            Some(s) if s.untracked_count > 0 => format!(
                "Include untracked files ({} in selection)",
                s.untracked_count
            )
            .into(),
            Some(s) if s.needs_untracked => "Include untracked files (required)".into(),
            _ => "Include untracked files".into(),
        };
        let lock_untracked = scope.as_ref().is_some_and(|s| s.needs_untracked);

        form::form_card(
            &self.focus_handle,
            &theme,
            &density,
            cx.listener(|dlg, event: &KeyDownEvent, window, cx| {
                // Bubble-phase: only act on the keys we care about so text
                // input inside the Input widget keeps working for the rest.
                match event.keystroke.key.as_str() {
                    "enter" => dlg.try_confirm(window, cx),
                    "escape" => dlg.cancel(window, cx),
                    _ => {}
                }
            }),
        )
            .child(form::form_title(title, &theme, typography))
            .children(scope.as_ref().map(|s| path_list(s, theme, density, typography)))
            .child(form::form_field_label("Message", &theme, typography))
            .child(Input::new(&self.message_input))
            .child(
                Checkbox::new("push-stash-include-untracked")
                    .checked(checked)
                    .disabled(lock_untracked)
                    .label(checkbox_label)
                    .on_click(move |new_checked: &bool, _window, cx| {
                        let value = *new_checked;
                        let _ = dialog_weak.update(cx, |dlg, cx| {
                            // Belt and braces: a disabled Checkbox should not
                            // fire, but the flag is the difference between a
                            // complete stash and a silently partial one.
                            if dlg.scope.as_ref().is_some_and(|s| s.needs_untracked) {
                                return;
                            }
                            dlg.include_untracked = value;
                            cx.notify();
                        });
                    }),
            )
            .children(
                scope
                    .as_ref()
                    .filter(|s| s.has_rename)
                    .map(|_| form::form_note(RENAME_NOTE, &theme, typography)),
            )
            .child(form::form_buttons(
                "push-stash-cancel-button",
                "push-stash-confirm-button",
                "Push stash",
                // Never disabled: a stash message is optional, unlike the
                // branch and rename forms.
                false,
                &density,
                cx.listener(|dlg, _: &ClickEvent, window, cx| dlg.cancel(window, cx)),
                cx.listener(|dlg, _: &ClickEvent, window, cx| dlg.try_confirm(window, cx)),
            ))
    }
}

/// What the user is told before a rename rides into a stash.
///
/// Not a warning about data: the content is preserved and git re-detects the
/// rename on the next `git add`. It is a warning about *staging*, because the
/// only sequence that stashes a renamed file without corrupting the index
/// unstages the rename first — see `git_panel::stash_selection`.
const RENAME_NOTE: &str = "A renamed file is in this selection. Stashing it unstages the rename; \
     the content is preserved and git re-detects the rename when you stage it again.";

/// The scoped dialog's path list. Caps at [`MAX_LISTED_PATHS`] rows and
/// collapses the remainder — a 400-file selection is a legitimate thing to
/// stash and must not produce a modal taller than the window.
fn path_list(
    scope: &PushStashScope,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let shown = scope.paths.len().min(MAX_LISTED_PATHS);
    let hidden = scope.paths.len() - shown;
    let mut list = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(density.gap_inline))
        .rounded(px(density.r_xs))
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_muted);
    for path in scope.paths.iter().take(shown) {
        list = list.child(
            div()
                .truncate()
                .child(path.to_string_lossy().into_owned()),
        );
    }
    if hidden > 0 {
        list = list.child(
            div()
                .text_color(theme.fg_subtle)
                .child(format!("+{hidden} more")),
        );
    }
    list
}

