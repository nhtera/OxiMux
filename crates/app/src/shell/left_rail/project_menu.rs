//! Per-project-header action menu — small popover anchored under the
//! "…" trigger button in a project group header. Routes the user's
//! selection back to `WorkspaceRoot` via a `WeakEntity` callback, mirroring
//! the per-workspace `row_menu` pattern.
//!
//! Closed state is `open_for: None`. Open state pins the carried `Project`
//! plus a screen-relative `(x, y)` anchor and renders the popover.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};

use crate::workspace_root::WorkspaceRoot;

/// Width of the menu card. Wider than the workspace row menu because
/// "Reveal in Finder" needs more horizontal room than "Rename".
const MENU_WIDTH: f32 = 180.0;
/// One row height — matches the workspace row menu for visual parity.
const ROW_MENU_ITEM_H: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Y offset below the trigger button so the menu doesn't overlap it.
const ANCHOR_Y_OFFSET: f32 = 4.0;

/// Action the user picked from the project menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRowAction {
    RevealInFinder,
    CopyPath,
    Remove,
}

impl ProjectRowAction {
    fn label(self) -> &'static str {
        match self {
            Self::RevealInFinder => "Reveal in Finder",
            Self::CopyPath => "Copy Path",
            Self::Remove => "Remove Project",
        }
    }

    fn is_destructive(self) -> bool {
        matches!(self, Self::Remove)
    }
}

/// `Remove` is separated from the non-destructive actions above it by a
/// divider so the destructive option can't be hit by reflex.
const ACTIONS: &[ProjectRowAction] = &[
    ProjectRowAction::RevealInFinder,
    ProjectRowAction::CopyPath,
    ProjectRowAction::Remove,
];

pub struct ProjectRowMenu {
    /// `None` when closed; `Some` carries the target project and the
    /// screen-pixel anchor point for the popover.
    open_for: Option<(Project, f32, f32)>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ProjectRowMenu {
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

    /// Open the menu anchored at (x, y) for the given project.
    pub fn open(&mut self, project: Project, x: f32, y: f32, cx: &mut Context<Self>) {
        self.open_for = Some((project, x, y + ANCHOR_Y_OFFSET));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        cx.notify();
    }

    fn dispatch(&self, action: ProjectRowAction, window: &mut Window, cx: &mut gpui::App) {
        let Some((project, ..)) = self.open_for.clone() else {
            return;
        };
        let _ = self.weak_root.update(cx, |root, cx| match action {
            ProjectRowAction::RevealInFinder => root.reveal_project_in_finder(&project),
            ProjectRowAction::CopyPath => root.copy_project_path(&project, cx),
            ProjectRowAction::Remove => root.request_remove_project(project, window, cx),
        });
    }
}

impl Render for ProjectRowMenu {
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
            .p(px(density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        for (ix, &action) in ACTIONS.iter().enumerate() {
            // Hairline divider above the destructive action so it reads as
            // a separate group and isn't hit by reflex.
            if action.is_destructive() && ix > 0 {
                card = card.child(
                    div()
                        .my(px(density.pad_overlay))
                        .h(px(1.0))
                        .w_full()
                        .bg(theme.border_inactive),
                );
            }
            let fg = if action.is_destructive() {
                theme.status_error
            } else {
                theme.fg_base
            };
            let row = div()
                .id(("project-menu-item", ix))
                .flex()
                .flex_row()
                .items_center()
                .h(px(ROW_MENU_ITEM_H))
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
    fn destructive_flag_only_for_remove() {
        assert!(!ProjectRowAction::RevealInFinder.is_destructive());
        assert!(!ProjectRowAction::CopyPath.is_destructive());
        assert!(ProjectRowAction::Remove.is_destructive());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(ProjectRowAction::RevealInFinder.label(), "Reveal in Finder");
        assert_eq!(ProjectRowAction::CopyPath.label(), "Copy Path");
        assert_eq!(ProjectRowAction::Remove.label(), "Remove Project");
    }

    #[test]
    fn remove_is_last_so_the_divider_precedes_it() {
        assert_eq!(ACTIONS.last(), Some(&ProjectRowAction::Remove));
        assert!(
            !ACTIONS[..ACTIONS.len() - 1]
                .iter()
                .any(|a| a.is_destructive())
        );
    }
}
