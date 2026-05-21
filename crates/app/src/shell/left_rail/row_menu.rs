//! Per-workspace-row action menu — small popover anchored under the "…"
//! trigger button. Routes the user's selection back to `WorkspaceRoot`
//! via a `WeakEntity` callback, mirroring the pattern established by the
//! adapter / project pickers.
//!
//! Closed state is `workspace: None`. Open state pins the carried
//! `Workspace` + screen-relative `(x, y)` anchor and renders the popover.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_core::Workspace;
use oximux_settings::{Density, Theme, Typography};

use crate::workspace_root::WorkspaceRoot;

/// Width of the menu card.
const MENU_WIDTH: f32 = 160.0;
/// Vertical padding around the card content.
const CARD_PADDING: f32 = 6.0;
/// One row height.
const ITEM_HEIGHT: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Y offset below the trigger button so the menu doesn't visually overlap it.
const ANCHOR_Y_OFFSET: f32 = 4.0;

/// Action the user picked from the row menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRowAction {
    Rename,
    Archive,
    Delete,
}

impl WorkspaceRowAction {
    fn label(self) -> &'static str {
        match self {
            Self::Rename => "Rename",
            Self::Archive => "Archive",
            Self::Delete => "Delete",
        }
    }

    fn is_destructive(self) -> bool {
        matches!(self, Self::Delete)
    }
}

const ACTIONS: &[WorkspaceRowAction] = &[
    WorkspaceRowAction::Rename,
    WorkspaceRowAction::Archive,
    WorkspaceRowAction::Delete,
];

pub struct WorkspaceRowMenu {
    /// `None` when closed; `Some` carries both the target workspace and
    /// the screen-pixel anchor point for the popover.
    open_for: Option<(Workspace, f32, f32)>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl WorkspaceRowMenu {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        weak_root: WeakEntity<WorkspaceRoot>,
    ) -> Self {
        Self {
            open_for: None,
            weak_root,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// Open the menu anchored at (x, y) for the given workspace.
    pub fn open(&mut self, workspace: Workspace, x: f32, y: f32, cx: &mut Context<Self>) {
        self.open_for = Some((workspace, x, y + ANCHOR_Y_OFFSET));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        cx.notify();
    }

    fn dispatch(&self, action: WorkspaceRowAction, window: &mut Window, cx: &mut gpui::App) {
        let Some((workspace, ..)) = self.open_for.clone() else {
            return;
        };
        let _ = self.weak_root.update(cx, |root, cx| match action {
            WorkspaceRowAction::Rename => root.request_rename_workspace(workspace, window, cx),
            WorkspaceRowAction::Archive => root.archive_workspace(workspace, cx),
            WorkspaceRowAction::Delete => root.request_delete_workspace(workspace, window, cx),
        });
    }
}

impl Render for WorkspaceRowMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((_, x, y)) = self.open_for.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(CARD_PADDING))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        for (ix, &action) in ACTIONS.iter().enumerate() {
            let fg = if action.is_destructive() {
                theme.status_error
            } else {
                theme.fg_base
            };
            let row = div()
                .id(("row-menu-item", ix))
                .flex()
                .flex_row()
                .items_center()
                .h(px(ITEM_HEIGHT))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .text_size(px(typography.t_body_md))
                .text_color(fg)
                .child(action.label())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        this.dispatch(action, window, cx);
                        this.close(cx);
                    }),
                );
            card = card.child(row);
        }

        div()
            .absolute()
            .inset_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(MENU_WIDTH))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_flag_only_for_delete() {
        assert!(!WorkspaceRowAction::Rename.is_destructive());
        assert!(!WorkspaceRowAction::Archive.is_destructive());
        assert!(WorkspaceRowAction::Delete.is_destructive());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(WorkspaceRowAction::Rename.label(), "Rename");
        assert_eq!(WorkspaceRowAction::Archive.label(), "Archive");
        assert_eq!(WorkspaceRowAction::Delete.label(), "Delete");
    }

    #[test]
    fn action_list_order_is_rename_archive_delete() {
        assert_eq!(
            ACTIONS,
            &[
                WorkspaceRowAction::Rename,
                WorkspaceRowAction::Archive,
                WorkspaceRowAction::Delete,
            ]
        );
    }
}
