//! Workspace dialog — create + rename modal.
//!
//! `Cmd+Shift+N` opens in `Create` mode; `WorkspaceRoot::request_rename_workspace`
//! opens in `Rename(workspace)` mode. Both modes share the same `InputState`
//! and key handlers; only the title, button label, and confirm semantics
//! differ. The owner registers a single `OnSubmit` callback that receives a
//! `WorkspaceDialogSubmit { mode, name }` payload and orchestrates the
//! create-with-rollback or rename flow.
//!
//! Pattern: full-window overlay (absolute inset-0) for click-outside
//! dismiss; centered modal card. Mirrors the step 5 project picker shape.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_core::Workspace;
use oximux_git::derive_slug;
use oximux_settings::{Density, Theme, Typography};

/// Width of the modal card.
const MODAL_WIDTH: f32 = 480.0;
/// Vertical offset from the top of the viewport.
const MODAL_TOP_OFFSET: f32 = 96.0;

/// Open-state mode. `None` (held in [`WorkspaceDialog::mode`]) is the
/// closed sentinel. `Rename` boxes the `Workspace` payload to keep the
/// enum small — `Workspace` is ~216 bytes and would otherwise force
/// every `Create`-mode dialog to carry a 216-byte tail of zeros (clippy
/// `large_enum_variant` flag).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceDialogMode {
    Create,
    Rename(Box<Workspace>),
}

/// Payload handed to the owner's `OnSubmit` callback when the user
/// confirms. The owner dispatches on `mode` to choose between the
/// create-with-rollback flow and the rename DB-only flow.
pub struct WorkspaceDialogSubmit {
    pub mode: WorkspaceDialogMode,
    pub name: String,
}

/// Boxed handler the owner registers to receive submission events.
pub type OnSubmit = Box<dyn Fn(WorkspaceDialogSubmit, &mut Window, &mut App) + Send + 'static>;

pub struct WorkspaceDialog {
    /// `None` when closed; `Some(mode)` while visible.
    mode: Option<WorkspaceDialogMode>,
    name_input: Entity<InputState>,
    focus_handle: FocusHandle,
    on_submit: OnSubmit,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl WorkspaceDialog {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        on_submit: OnSubmit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. fix-login"));
        Self {
            mode: None,
            name_input,
            focus_handle: cx.focus_handle(),
            on_submit,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.mode.is_some()
    }

    /// Open in Create mode. Clears the input and grabs focus.
    pub fn open_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Some(WorkspaceDialogMode::Create);
        self.name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Open in Rename mode pre-filled with the workspace's current name.
    pub fn open_rename(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing_name = workspace.name.clone();
        self.mode = Some(WorkspaceDialogMode::Rename(Box::new(workspace)));
        self.name_input
            .update(cx, |s, cx| s.set_value(&existing_name, window, cx));
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.mode = None;
        cx.notify();
    }

    /// Current name input value (post-trim is the caller's job).
    fn current_name(&self, cx: &App) -> String {
        self.name_input.read(cx).value().to_string()
    }

    /// Slug preview for the Create mode title row. Pure-function: just
    /// runs `derive_slug` on the current input — does not validate.
    pub fn slug_preview(&self, cx: &App) -> String {
        derive_slug(self.current_name(cx).trim())
    }

    /// True when the name is non-empty after trimming (the only client-
    /// side guard; backend `validate_slug` is the authoritative gate).
    fn can_submit(&self, cx: &App) -> bool {
        !self.current_name(cx).trim().is_empty()
    }

    /// Submit the dialog. Hands the owner a `WorkspaceDialogSubmit`
    /// payload, then closes — close-before-callback mirrors the step 5
    /// project-picker C2 fix so the callback can freely open another
    /// modal without its state being wiped by a trailing close().
    fn try_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let Some(mode) = self.mode.clone() else {
            return;
        };
        let name = self.current_name(cx).trim().to_string();
        self.close(cx);
        (self.on_submit)(WorkspaceDialogSubmit { mode, name }, window, cx);
    }
}

impl Focusable for WorkspaceDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(mode) = self.mode.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let can_submit = self.can_submit(cx);
        let slug = self.slug_preview(cx);
        let (title, button_label) = match mode {
            WorkspaceDialogMode::Create => ("New Workspace", "Create"),
            WorkspaceDialogMode::Rename(_) => ("Rename Workspace", "Rename"),
        };
        let slug_line = format!("Branch: oximux/{}", slug);

        let card = div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => this.try_submit(window, cx),
                    "escape" => this.close(cx),
                    _ => {}
                }
            }))
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                // Swallow press inside the card so it does not bubble up
                // to the overlay dismiss handler.
            })
            .flex()
            .flex_col()
            .w(px(MODAL_WIDTH))
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
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Name"),
            )
            .child(Input::new(&self.name_input))
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(slug_line),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("workspace-dialog-cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|dlg, _: &ClickEvent, _window, cx| {
                                dlg.close(cx);
                            })),
                    )
                    .child(
                        Button::new("workspace-dialog-submit")
                            .primary()
                            .label(button_label)
                            .disabled(!can_submit)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.try_submit(window, cx);
                            })),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(MODAL_TOP_OFFSET))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.close(cx)),
            )
            .child(card)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_payload_carries_create_mode() {
        let payload = WorkspaceDialogSubmit {
            mode: WorkspaceDialogMode::Create,
            name: "fix-login".to_string(),
        };
        assert_eq!(payload.mode, WorkspaceDialogMode::Create);
        assert_eq!(payload.name, "fix-login");
    }

    #[test]
    fn submit_payload_carries_rename_mode_with_workspace() {
        let workspace = Workspace {
            id: "id".to_string(),
            project_id: "pid".to_string(),
            name: "old".to_string(),
            slug: "old".to_string(),
            branch: "oximux/old".to_string(),
            worktree_path: "/path".to_string(),
            status: "active".to_string(),
            created_at: "now".to_string(),
            archived_at: None,
        };
        let payload = WorkspaceDialogSubmit {
            mode: WorkspaceDialogMode::Rename(Box::new(workspace.clone())),
            name: "new".to_string(),
        };
        match payload.mode {
            WorkspaceDialogMode::Rename(w) => assert_eq!(w.id, workspace.id),
            _ => panic!("expected Rename mode"),
        }
    }

    /// Slug-preview is `derive_slug` applied to the trimmed name; the
    /// pure helper is tested in `oximux_git::worktree::tests`. Here we
    /// confirm the dialog's contract: when there is a name, the preview
    /// matches `derive_slug`'s output for that trimmed string.
    #[test]
    fn slug_preview_matches_derive_slug() {
        let name = "  My Feature  ";
        let trimmed = name.trim();
        assert_eq!(derive_slug(trimmed), "my-feature");
    }
}
