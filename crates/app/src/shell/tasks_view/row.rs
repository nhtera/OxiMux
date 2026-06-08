//! One issue/PR row + the small status/label/action chips it renders.
//!
//! Data is pre-fetched into [`ForgeItem`]s before the `uniform_list` closure
//! builds these rows, so nothing here touches the network or does per-frame
//! work. Row actions (create workspace, open in browser) dispatch through plain
//! event closures capturing a `WeakEntity<WorkspaceRoot>` + the active project.

use gpui::{
    AnyElement, App, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, WeakEntity, Window, div, px,
};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::ForgeItem;
use crate::shell::open_url::open_url;
use crate::shell::tasks_view::TaskKind;
use crate::workspace_root::WorkspaceRoot;

/// Fixed row height for the virtualized list (two text lines + padding).
pub(super) const TASK_ROW_HEIGHT: f32 = 46.0;

/// Color for a GitHub state string. `OPEN` reads as live (ok), `MERGED` as
/// informational, anything else (`CLOSED`) as muted. Case-insensitive so it
/// tolerates `open`/`OPEN` variants across `gh` versions.
pub(super) fn state_color(state: &str, theme: Theme) -> Hsla {
    match state.to_ascii_uppercase().as_str() {
        "OPEN" => theme.status_ok,
        "MERGED" => theme.status_info,
        _ => theme.fg_muted,
    }
}

/// The workspace name seeded from an issue/PR. `create_workspace_async` derives
/// the slug + `oximux/<slug>` branch from this, so it carries the number for a
/// recognizable branch like `oximux/issue-42-fix-crash`.
pub(super) fn workspace_name_for(kind: TaskKind, item: &ForgeItem) -> String {
    let prefix = match kind {
        TaskKind::Issues => "issue",
        TaskKind::Prs => "pr",
    };
    format!("{prefix} {} {}", item.number, item.title)
}

/// A small static rounded pill (status or label chip).
fn chip(text: String, fg: Hsla, bg: Hsla, density: Density, typography: &Typography) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(bg)
        .text_size(px(typography.t_label_xs))
        .text_color(fg)
        .child(text)
        .into_any_element()
}

/// Build one issue/PR row element. Two lines: `#<number> <title>` + a state
/// chip on the first; label chips + assignee + actions on the second.
pub(super) fn render_task_row(
    item: &ForgeItem,
    kind: TaskKind,
    weak_root: WeakEntity<WorkspaceRoot>,
    project: Project,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let state = chip(
        item.state.to_ascii_lowercase(),
        state_color(&item.state, theme),
        theme.bg_panel_alt,
        density,
        typography,
    );

    let title_line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .child(
            div()
                .flex_none()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(format!("#{}", item.number)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child(item.title.clone()),
        )
        .child(state);

    // Second line: a truncating left cluster (label chips + assignee) and a
    // fixed-width actions cluster on the right that always stays visible.
    let mut left = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .items_center()
        .gap(px(density.gap_inline));
    for label in item.labels.iter().take(2) {
        left = left.child(chip(
            label.name.clone(),
            theme.fg_muted,
            theme.bg_panel_alt,
            density,
            typography,
        ));
    }
    if let Some(assignee) = item.assignees.first() {
        left = left.child(
            div()
                .flex_none()
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(format!("@{}", assignee.login)),
        );
    }
    let meta = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .child(left)
        .child(open_action(item.url.clone(), theme, density, typography))
        .child(create_action(
            workspace_name_for(kind, item),
            weak_root,
            project,
            theme,
            density,
            typography,
        ));

    div()
        .flex()
        .flex_col()
        .h(px(TASK_ROW_HEIGHT))
        .w_full()
        .px(px(density.pad_panel))
        .gap(px(2.0))
        .justify_center()
        .child(title_line)
        .child(meta)
        .into_any_element()
}

/// "Open in browser" action chip.
fn open_action(url: String, theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_muted)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _w, _cx: &mut App| {
            open_url(&url);
        })
        .child("↗".to_string())
        .into_any_element()
}

/// "Create workspace from this" action chip → `create_workspace_async`.
fn create_action(
    name: String,
    weak_root: WeakEntity<WorkspaceRoot>,
    project: Project,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                let _ = weak_root.update(cx, |root, cx| {
                    root.create_workspace_async(project.clone(), name.clone(), None, window, cx);
                });
            },
        )
        .child("+ Workspace".to_string())
        .into_any_element()
}
