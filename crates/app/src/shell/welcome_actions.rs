//! Actionable launch panel for the welcome surface.

use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    Window, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    OpenAddProjectDialog, OpenCommandPalette, OpenProjectPicker, OpenWorkspaceCreate,
};
use crate::shell::welcome_flow;

const ACTION_CARD_W: f32 = 250.0;
const ACTION_CARD_H: f32 = 70.0;
const ACTION_ICON_SIZE: f32 = 16.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LaunchActionKind {
    OpenProject,
    AddProject,
    NewWorkspace,
    CommandPalette,
}

#[derive(Clone, Copy)]
struct LaunchActionSpec {
    kind: LaunchActionKind,
    icon: &'static str,
    title: &'static str,
    subtitle: &'static str,
    shortcut: &'static str,
    primary: bool,
}

const LAUNCH_ACTIONS: &[LaunchActionSpec] = &[
    LaunchActionSpec {
        kind: LaunchActionKind::OpenProject,
        icon: "icons/folder-open.svg",
        title: "Open project",
        subtitle: "Resume a repo or pick a folder",
        shortcut: "\u{2318}O",
        primary: true,
    },
    LaunchActionSpec {
        kind: LaunchActionKind::NewWorkspace,
        icon: "icons/git-branch.svg",
        title: "New workspace",
        subtitle: "Create an isolated worktree",
        shortcut: "\u{2318}\u{21E7}N",
        primary: false,
    },
    LaunchActionSpec {
        kind: LaunchActionKind::AddProject,
        icon: "icons/plus.svg",
        title: "Add project",
        subtitle: "Browse, clone, or attach remote",
        shortcut: "\u{2318}O",
        primary: false,
    },
    LaunchActionSpec {
        kind: LaunchActionKind::CommandPalette,
        icon: "icons/keyboard.svg",
        title: "Command palette",
        subtitle: "Find actions and custom prompts",
        shortcut: "\u{2318}\u{21E7}P",
        primary: false,
    },
];

pub fn launch_panel(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(density.pad_panel))
        .child(action_row(0, theme, density, typography))
        .child(action_row(2, theme, density, typography))
        .child(welcome_flow::flow_strip(theme, density, typography))
}

fn action_row(
    start: usize,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div().flex().flex_row().gap(px(density.pad_panel)).children(
        LAUNCH_ACTIONS[start..start + 2]
            .iter()
            .map(|spec| action_card(*spec, theme, density, typography)),
    )
}

fn action_card(
    spec: LaunchActionSpec,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let mut shell = div()
        .flex()
        .flex_row()
        .items_center()
        .w(px(ACTION_CARD_W))
        .h(px(ACTION_CARD_H))
        .px(px(density.pad_panel + 2.0))
        .gap(px(density.pad_panel))
        .rounded(px(density.r_card))
        .border_1()
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay).border_color(theme.border_active))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                dispatch_launch_action(spec.kind, window, cx);
            },
        );

    if spec.primary {
        shell = shell
            .bg(theme.bg_panel_alt)
            .border_color(theme.border_active);
    } else {
        shell = shell.bg(theme.bg_panel).border_color(theme.border_inactive);
    }

    shell
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(28.0))
                .rounded(px(density.r_xs))
                .bg(theme.bg_overlay)
                .child(
                    svg()
                        .path(spec.icon)
                        .size(px(ACTION_ICON_SIZE))
                        .text_color(theme.fg_muted),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(typography.t_body_md))
                        .font_weight(typography.w_semibold)
                        .text_color(theme.fg_base)
                        .truncate()
                        .child(spec.title),
                )
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_subtle)
                        .truncate()
                        .child(spec.subtitle),
                ),
        )
        .child(shortcut_chip(spec.shortcut, theme, density, typography))
}

fn shortcut_chip(
    label: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(20.0))
        .px(px(6.0))
        .rounded(px(density.r_chip))
        .border_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_muted)
        .child(label)
}

fn dispatch_launch_action(kind: LaunchActionKind, window: &mut Window, cx: &mut App) {
    match kind {
        LaunchActionKind::OpenProject => window.dispatch_action(Box::new(OpenProjectPicker), cx),
        LaunchActionKind::AddProject => window.dispatch_action(Box::new(OpenAddProjectDialog), cx),
        LaunchActionKind::NewWorkspace => {
            window.dispatch_action(Box::new(OpenWorkspaceCreate), cx);
        }
        LaunchActionKind::CommandPalette => {
            window.dispatch_action(Box::new(OpenCommandPalette), cx);
        }
    }
}
