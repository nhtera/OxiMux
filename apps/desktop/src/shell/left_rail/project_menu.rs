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
    /// Opt this project in or out of the computer-use tools.
    ///
    /// Lives here rather than in the settings pane deliberately: this is the
    /// one place the user is looking at a specific project and can see which
    /// one they are enabling. Picking a path out of a list in a global pane is
    /// how the wrong repository gets enabled.
    ToggleComputerUse,
    Remove,
}

impl ProjectRowAction {
    /// The row's text, given whether this project already has computer use.
    ///
    /// Takes the state rather than reading it: the menu is rendered from a
    /// snapshot taken when it opened, and a label that re-read global settings
    /// mid-render could disagree with the action the click dispatches.
    fn label(self, computer_use_on: bool) -> &'static str {
        match self {
            Self::RevealInFinder => "Reveal in Finder",
            Self::CopyPath => "Copy Path",
            Self::ToggleComputerUse if computer_use_on => "Turn off computer use",
            Self::ToggleComputerUse => "Turn on computer use",
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
    ProjectRowAction::ToggleComputerUse,
    ProjectRowAction::Remove,
];

pub struct ProjectRowMenu {
    /// `None` when closed; `Some` carries the target project and the
    /// screen-pixel anchor point for the popover.
    open_for: Option<(Project, f32, f32)>,
    /// This project's computer-use state, read once when the menu opened.
    ///
    /// `None` means the master switch is off, and the row is left out entirely
    /// rather than shown doing nothing: the per-project opt-in is one of two
    /// switches, so recording it while the other is off would leave the user
    /// looking at a menu item they had just used and a project that still got
    /// no tools. The master switch lives in Settings, which is where the
    /// feature is discovered in the first place.
    computer_use: Option<bool>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ProjectRowMenu {
    /// `true` while the menu is pinned open for a project row.
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        weak_root: WeakEntity<WorkspaceRoot>,
    ) -> Self {
        Self {
            open_for: None,
            computer_use: None,
            weak_root,
            theme,
            density,
            typography,
        }
    }

    /// Open the menu anchored at (x, y) for the given project.
    pub fn open(&mut self, project: Project, x: f32, y: f32, cx: &mut Context<Self>) {
        self.computer_use = computer_use_state(&project, cx);
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
        let on = self.computer_use.unwrap_or(false);
        let _ = self.weak_root.update(cx, |root, cx| match action {
            ProjectRowAction::RevealInFinder => root.reveal_project_in_finder(&project),
            ProjectRowAction::CopyPath => root.copy_project_path(&project, cx),
            ProjectRowAction::ToggleComputerUse => {
                root.set_computer_use_for_project(&project, !on, cx)
            }
            ProjectRowAction::Remove => root.request_remove_project(project, window, cx),
        });
    }
}

/// Whether `project` has computer use, or `None` when the master switch is off.
///
/// Read at open time rather than per repaint: the answer involves resolving
/// paths, and a menu whose label changed under the cursor would dispatch
/// something other than what the user read.
fn computer_use_state(project: &Project, cx: &gpui::App) -> Option<bool> {
    let settings = cx.try_global::<oximux_settings::ComputerUseSettings>()?;
    settings
        .enabled
        .then(|| settings.is_enabled_for(std::path::Path::new(&project.root_path)))
}

impl Render for ProjectRowMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
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

        let computer_use = self.computer_use;
        let shown = ACTIONS
            .iter()
            .filter(|a| computer_use.is_some() || **a != ProjectRowAction::ToggleComputerUse);

        for (ix, &action) in shown.enumerate() {
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
                .hover(|s| s.bg(theme.hover_overlay))
                .text_size(px(typography.t_body_md))
                .text_color(fg)
                .child(action.label(computer_use.unwrap_or(false)))
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
            .occlude()
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
        assert_eq!(
            ProjectRowAction::RevealInFinder.label(false),
            "Reveal in Finder"
        );
        assert_eq!(ProjectRowAction::CopyPath.label(false), "Copy Path");
        assert_eq!(ProjectRowAction::Remove.label(false), "Remove Project");
    }

    #[test]
    fn the_computer_use_row_says_which_way_it_will_go() {
        // The label has to name the *result* of clicking, not the state: a row
        // reading "Computer use" beside a project already using it tells the
        // user nothing about what the click does.
        assert_eq!(
            ProjectRowAction::ToggleComputerUse.label(false),
            "Turn on computer use"
        );
        assert_eq!(
            ProjectRowAction::ToggleComputerUse.label(true),
            "Turn off computer use"
        );
    }

    /// The rendered rows, for a given master-switch state. Mirrors `render`'s
    /// filter so the omission is asserted rather than inspected by eye.
    fn shown(computer_use: Option<bool>) -> Vec<ProjectRowAction> {
        ACTIONS
            .iter()
            .copied()
            .filter(|a| computer_use.is_some() || *a != ProjectRowAction::ToggleComputerUse)
            .collect()
    }

    #[test]
    fn the_computer_use_row_is_absent_while_the_master_switch_is_off() {
        // Showing it would record a per-project opt-in that changes nothing,
        // because both switches are required — a control that appears to work
        // and does not, which is the failure this row exists to end.
        assert!(!shown(None).contains(&ProjectRowAction::ToggleComputerUse));
        assert!(shown(Some(false)).contains(&ProjectRowAction::ToggleComputerUse));
        assert!(shown(Some(true)).contains(&ProjectRowAction::ToggleComputerUse));
    }

    #[test]
    fn hiding_the_computer_use_row_leaves_the_others_in_order() {
        assert_eq!(
            shown(None),
            vec![
                ProjectRowAction::RevealInFinder,
                ProjectRowAction::CopyPath,
                ProjectRowAction::Remove,
            ]
        );
    }

    #[test]
    fn only_remove_is_destructive_among_the_new_rows() {
        // Turning a capability off is not destructive — nothing is lost and the
        // same menu turns it back on. Marking it so would put it below the
        // divider, reading as a sibling of "Remove Project".
        assert!(!ProjectRowAction::ToggleComputerUse.is_destructive());
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
