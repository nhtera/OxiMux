//! RenameTabDialog — small modal that lets the user override a tab's
//! visible title. Pre-fills with the tab's current visible title; Save
//! commits the override via the host callback (the host calls
//! `PaneGroup::set_tab_title(tab_idx, Some(new))`). Reset clears the
//! override (passes `None` to the callback so the default label returns).
//!
//! Mounted by `WorkspaceRoot` in response to the `RequestRenameTabAt`
//! action — same pattern as `ConfirmDialog`: built per-request, dropped
//! by setting the `Option` back to `None` after commit/cancel.

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
use std::rc::Rc;

/// Outcome passed to the host callback when the user dismisses the
/// dialog. The host MUST always clear its `rename_tab_dialog` Option
/// regardless of variant — otherwise Cancel leaves the modal mounted.
pub enum RenameOutcome {
    /// User typed a new title and pressed Save / Enter.
    Save(SharedString),
    /// User clicked Reset — clear any custom title override.
    Reset,
    /// User clicked Cancel / pressed Escape — no mutation.
    Cancel,
}

/// Boxed callback fired on any dismiss path. `Rc` (not `Arc`) because
/// GPUI views are single-threaded on the foreground executor.
pub type RenameCallback = Rc<dyn Fn(RenameOutcome, &mut Window, &mut App) + 'static>;

pub struct RenameTabDialog {
    title: SharedString,
    input_state: Entity<InputState>,
    on_commit: Option<RenameCallback>,
    closed: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl RenameTabDialog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        title: SharedString,
        initial_value: SharedString,
        on_commit: RenameCallback,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input_state = cx.new(|cx| InputState::new(window, cx).placeholder("New title"));
        // Apply the initial value AFTER constructing the entity — set_value
        // wants a fresh (window, cx) pair, and the closure inside cx.new
        // can't surface them cleanly. Same pattern as workspace_dialog's
        // open_rename().
        input_state.update(cx, |s, cx| s.set_value(initial_value, window, cx));
        Self {
            title,
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

    /// FocusHandle of the inner Input — the host action handler focuses
    /// this AFTER mounting the dialog so keystrokes land in the field
    /// without the user needing to click first.
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input_state.read(cx).focus_handle(cx)
    }

    fn commit_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input_state.read(cx).value().to_string();
        let trimmed = typed.trim();
        // Empty save = no-op (use Reset to clear the override).
        if trimmed.is_empty() {
            return;
        }
        let value = SharedString::from(trimmed.to_string());
        if let Some(cb) = self.on_commit.take() {
            cb(RenameOutcome::Save(value), window, cx);
        }
        self.closed = true;
        cx.notify();
    }

    fn commit_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_commit.take() {
            cb(RenameOutcome::Reset, window, cx);
        }
        self.closed = true;
        cx.notify();
    }

    fn commit_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cb) = self.on_commit.take() {
            cb(RenameOutcome::Cancel, window, cx);
        }
        self.closed = true;
        cx.notify();
    }
}

impl Focusable for RenameTabDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RenameTabDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let typed_non_empty = !self
            .input_state
            .read(cx)
            .value()
            .to_string()
            .trim()
            .is_empty();
        let can_save = typed_non_empty && !self.closed;
        let can_reset = !self.closed;

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => this.commit_save(window, cx),
                    "escape" => this.commit_cancel(window, cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                // Swallow clicks inside the card so they don't bubble to
                // any overlay dismiss handler above.
            })
            .flex()
            .flex_col()
            .w(px(420.0))
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
                    .child(SharedString::from(
                        "Sets a custom title on this tab. Reset restores the default label.",
                    )),
            )
            .child(Input::new(&self.input_state))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("rename-tab-reset")
                            .label("Reset")
                            .disabled(!can_reset)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_reset(window, cx);
                            })),
                    )
                    .child(
                        Button::new("rename-tab-cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_cancel(window, cx);
                            })),
                    )
                    .child(
                        Button::new("rename-tab-save")
                            .primary()
                            .label("Save")
                            .disabled(!can_save)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.commit_save(window, cx);
                            })),
                    ),
            )
    }
}
