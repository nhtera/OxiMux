//! ReviewNotePopover — compose / edit / delete one diff review note.
//!
//! Opened when the reviewer clicks a line's gutter note marker. Pre-fills
//! with the existing note body (if any); Save commits the typed text, Delete
//! removes the note, Cancel/Escape dismisses without change. The host
//! (`DiffView`) mounts it like `ConfirmDialog` — built per-request, dropped
//! by setting the host's `Option` back to `None` after any dismiss path — and
//! turns the outcome into a `ReviewNoteStore` mutation + a `DiffReviewNoteRepo`
//! write. Mirrors `rename_tab_dialog`'s entity/callback shape; the input is
//! multi-line so a note can span several lines.

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

use crate::ui::FloatingSurface;
use std::rc::Rc;

use crate::shell::diff_view::review_notes::NoteAnchor;

/// Outcome handed to the host callback on any dismiss path. The host MUST
/// always clear its `note_popover` Option regardless of variant — otherwise
/// Cancel leaves the popover mounted.
pub enum ReviewNoteOutcome {
    /// User typed a (non-empty) body and pressed Save / Cmd+Enter.
    Save(String),
    /// User clicked Delete — remove the note at this anchor.
    Delete,
    /// User clicked Cancel / pressed Escape — no mutation.
    Cancel,
}

/// Boxed callback fired on dismiss. `Rc` (not `Arc`) because GPUI views are
/// single-threaded on the foreground executor.
pub type ReviewNoteCallback = Rc<dyn Fn(ReviewNoteOutcome, &mut Window, &mut App) + 'static>;

pub struct ReviewNotePopover {
    anchor: NoteAnchor,
    /// True when a note already exists at the anchor — gates the Delete
    /// button (nothing to delete on a fresh line).
    has_existing: bool,
    input_state: Entity<InputState>,
    on_commit: Option<ReviewNoteCallback>,
    closed: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ReviewNotePopover {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        anchor: NoteAnchor,
        existing_body: Option<String>,
        on_commit: ReviewNoteCallback,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let has_existing = existing_body.is_some();
        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Write a review note for this line…")
        });
        // Apply the initial value AFTER constructing the entity — set_value
        // wants a fresh (window, cx) pair the cx.new closure can't surface.
        if let Some(body) = existing_body {
            input_state.update(cx, |s, cx| s.set_value(SharedString::from(body), window, cx));
        }
        Self {
            anchor,
            has_existing,
            input_state,
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

    /// FocusHandle of the inner Input — the host focuses this after mounting
    /// so keystrokes land in the field without a click first.
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input_state.read(cx).focus_handle(cx)
    }

    fn commit_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input_state.read(cx).value().to_string();
        let trimmed = typed.trim();
        // An empty body Saves as a Delete — clearing the text is "remove the
        // note", matching the store's empty-body-removes contract.
        let outcome = if trimmed.is_empty() {
            ReviewNoteOutcome::Delete
        } else {
            ReviewNoteOutcome::Save(trimmed.to_string())
        };
        if let Some(cb) = self.on_commit.take() {
            cb(outcome, window, cx);
        }
        self.closed = true;
        cx.notify();
    }

    fn commit_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_commit.take() {
            cb(ReviewNoteOutcome::Delete, window, cx);
        }
        self.closed = true;
        cx.notify();
    }

    fn commit_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_commit.take() {
            cb(ReviewNoteOutcome::Cancel, window, cx);
        }
        self.closed = true;
        cx.notify();
    }
}

impl Focusable for ReviewNotePopover {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewNotePopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let side = match self.anchor.side {
            oximux_core::NoteSide::Old => "old",
            oximux_core::NoteSide::New => "new",
        };
        let header = format!("Note · {} L{} ({side})", self.anchor.path, self.anchor.line);
        let can_delete = self.has_existing && !self.closed;

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    // Cmd/Ctrl+Enter saves; a bare Enter inserts a newline
                    // (multi-line field), so only the modifier chord commits.
                    "enter" if event.keystroke.modifiers.platform || event.keystroke.modifiers.control => {
                        this.commit_save(window, cx)
                    }
                    "escape" => this.commit_cancel(window, cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                // Swallow clicks inside the card so they don't bubble to any
                // overlay dismiss handler above.
            })
            .flex()
            .flex_col()
            .w(px(420.0))
            .p(px(density.pad_panel * 2.0))
            .floating_chrome(&theme, &density)
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(SharedString::from(header)),
            )
            .child(Input::new(&self.input_state))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from("⌘↵ to save · Esc to cancel")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("review-note-delete")
                            .label("Delete")
                            .disabled(!can_delete)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_delete(window, cx);
                            })),
                    )
                    .child(
                        Button::new("review-note-cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_cancel(window, cx);
                            })),
                    )
                    .child(
                        Button::new("review-note-save")
                            .primary()
                            .label("Save")
                            .disabled(self.closed)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_save(window, cx);
                            })),
                    ),
            )
    }
}
