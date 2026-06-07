//! PrCreateDialog — compose a pull request before creating it.
//!
//! Replaces the bare commit-derived `gh pr create --fill` with an editable
//! surface: a title field, a multi-line body, a draft toggle, and a
//! "Draft from commits" button that asks the host to fill title + body from the
//! branch's commit subjects. The host (`SourceControlPanel`) mounts it
//! per-request — built on open, dropped by clearing the host's `Option` after
//! any dismiss — and turns the `Create` outcome into a forge `create_pr` call.
//! Mirrors `review_note_popover`'s entity/callback shape.

use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, SharedString, Styled, Window,
    div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::CreatePrOptions;

/// Outcome handed to the host callback. `Create`/`Cancel` dismiss the dialog;
/// `DraftFromCommits` does NOT — the host fills the fields and pushes results
/// back via [`PrCreateDialog::apply_generated`].
pub enum PrDialogOutcome {
    /// User pressed Create / Cmd+Enter with a non-empty title.
    Create(CreatePrOptions),
    /// User clicked "Draft from commits" — host should fill title + body.
    DraftFromCommits,
    /// User clicked Cancel / pressed Escape.
    Cancel,
}

/// Boxed callback. `Rc` (not `Arc`) because GPUI views are single-threaded on
/// the foreground executor.
pub type PrDialogCallback = Rc<dyn Fn(PrDialogOutcome, &mut Window, &mut App) + 'static>;

pub struct PrCreateDialog {
    title_state: Entity<InputState>,
    body_state: Entity<InputState>,
    draft: bool,
    /// True while the host is running AI generation — disables inputs +
    /// buttons and flips the AI button label to a working state.
    generating: bool,
    on_commit: Option<PrDialogCallback>,
    closed: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl PrCreateDialog {
    pub fn new(
        on_commit: PrDialogCallback,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_state =
            cx.new(|cx| InputState::new(window, cx).placeholder("Pull request title"));
        let body_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Describe the change (optional)…")
        });
        Self {
            title_state,
            body_state,
            draft: false,
            generating: false,
            on_commit: Some(on_commit),
            closed: false,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// FocusHandle of the title field — the host focuses this after mounting.
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.title_state.read(cx).focus_handle(cx)
    }

    /// Mark drafting in flight (or done). Driven by the host around the
    /// title/body fetch so the dialog reflects the working state.
    pub fn set_generating(&mut self, generating: bool, cx: &mut Context<Self>) {
        self.generating = generating;
        cx.notify();
    }

    /// Fill the title + body fields from a host-produced draft. Clears the
    /// generating flag.
    pub fn apply_generated(
        &mut self,
        title: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.title_state
            .update(cx, |s, cx| s.set_value(SharedString::from(title), window, cx));
        self.body_state
            .update(cx, |s, cx| s.set_value(SharedString::from(body), window, cx));
        self.generating = false;
        cx.notify();
    }

    fn commit_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.generating {
            return;
        }
        let title = self.title_state.read(cx).value().trim().to_string();
        // No title → nothing to create (the button is also disabled in this
        // state, but key chords reach here directly).
        if title.is_empty() {
            return;
        }
        let body = self.body_state.read(cx).value().to_string();
        let opts = CreatePrOptions {
            title,
            body,
            base: None,
            draft: self.draft,
        };
        if let Some(cb) = self.on_commit.take() {
            cb(PrDialogOutcome::Create(opts), window, cx);
        }
        self.closed = true;
        cx.notify();
    }

    fn request_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.generating {
            return;
        }
        // Non-closing: clone the callback so it survives for a later Create.
        if let Some(cb) = self.on_commit.clone() {
            self.generating = true;
            cb(PrDialogOutcome::DraftFromCommits, window, cx);
            cx.notify();
        }
    }

    fn commit_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_commit.take() {
            cb(PrDialogOutcome::Cancel, window, cx);
        }
        self.closed = true;
        cx.notify();
    }

    fn toggle_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = !self.draft;
        cx.notify();
    }
}

impl Focusable for PrCreateDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PrCreateDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let busy = self.generating;
        // Create needs a title; reflect that in the button's enabled state so it
        // doesn't look clickable-but-inert (the chord/handler guard the same).
        let title_empty = self.title_state.read(cx).value().trim().is_empty();
        let draft_label = if self.draft { "Draft: on" } else { "Draft: off" };
        let draft_btn_label = if busy { "Drafting…" } else { "Draft from commits" };

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    // Cmd/Ctrl+Enter creates; bare Enter inserts a newline in
                    // the multi-line body, so only the chord commits.
                    "enter"
                        if event.keystroke.modifiers.platform
                            || event.keystroke.modifiers.control =>
                    {
                        this.commit_create(window, cx)
                    }
                    "escape" => this.commit_cancel(window, cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                // Swallow clicks inside the card so they don't bubble to the
                // overlay's dismiss handler.
            })
            .flex()
            .flex_col()
            .w(px(480.0))
            .p(px(density.pad_panel * 2.0))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(SharedString::from("Create pull request")),
            )
            .child(Input::new(&self.title_state))
            .child(Input::new(&self.body_state))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("pr-draft-toggle")
                            .label(draft_label)
                            .disabled(busy)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, _window, cx| {
                                dlg.toggle_draft(cx);
                            })),
                    )
                    .child(
                        Button::new("pr-draft-from-commits")
                            .label(draft_btn_label)
                            .disabled(busy)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.request_draft(window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(typography.t_body_sm))
                            .text_color(theme.fg_subtle)
                            .child(SharedString::from("⌘↵ create · Esc cancel")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("pr-cancel").label("Cancel").on_click(
                            cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_cancel(window, cx);
                            }),
                        ),
                    )
                    .child(
                        Button::new("pr-create")
                            .primary()
                            .label("Create")
                            .disabled(busy || self.closed || title_empty)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_create(window, cx);
                            })),
                    ),
            )
    }
}
