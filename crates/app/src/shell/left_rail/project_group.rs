//! Project group renderer — header row (folder icon, name, count chip,
//! hover-visible "+" button) followed by zero or more Workspace rows.
//!
//! The "+" button always sets the project as active first, then
//! dispatches `OpenWorkspaceCreate` — single code path regardless of
//! whether the project was already active when the user clicked.

use gpui::{
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, SharedString,
    Styled, WeakEntity, div, px, svg,
};
use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::OpenWorkspaceCreate;
use crate::shell::left_rail::workspace_row::{build_workspace_row_plan, render_workspace_row};
use crate::workspace_root::WorkspaceRoot;

/// Header height in CSS pixels (matches WORKSPACES section header).
const HEADER_HEIGHT: f32 = 28.0;
/// Folder + plus icon size in the header.
const HEADER_ICON_SIZE: f32 = 14.0;
/// Plus button square size.
const PLUS_BTN_SIZE: f32 = 18.0;

/// Pure plan for one project group's header. Workspace rows are computed
/// separately by `workspace_row::build_workspace_row_plan`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectGroupPlan {
    pub project_name: String,
    pub workspace_count: usize,
    /// `true` when this is the currently active project — the header
    /// gets a slightly stronger foreground colour to match the mockup.
    pub is_active: bool,
}

pub fn build_project_group_plan(
    project: &Project,
    workspaces: &[Workspace],
    is_active: bool,
) -> ProjectGroupPlan {
    ProjectGroupPlan {
        project_name: project.name.clone(),
        workspace_count: workspaces.len(),
        is_active,
    }
}

/// Render one project group: header + its workspace rows.
///
/// `latest_status_for(workspace_id)` resolves the latest agent-session
/// status for the dot color; the lookup is injected so the caller decides
/// whether to query a HashMap (cached map) or call the repo directly.
#[allow(clippy::too_many_arguments)]
pub fn render_project_group(
    plan: ProjectGroupPlan,
    project: Project,
    workspaces: Vec<Workspace>,
    latest_status_for: impl Fn(&str) -> Option<AgentStatus>,
    active_workspace_id: Option<&str>,
    weak_root: WeakEntity<WorkspaceRoot>,
    on_row_menu: impl Fn(Workspace, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let group_name: SharedString = format!("project-group-{}", project.id).into();

    let header = build_header(
        plan,
        project.clone(),
        group_name.clone(),
        weak_root.clone(),
        theme,
        density,
        typography,
    );

    let mut col = div().flex().flex_col().w_full().child(header);

    for workspace in workspaces {
        let row_group: SharedString = format!("ws-row-{}", workspace.id).into();
        let is_active = active_workspace_id == Some(workspace.id.as_str());
        let latest = latest_status_for(&workspace.id);
        let row_plan = build_workspace_row_plan(&workspace, is_active, latest.as_ref(), theme);
        let row_id: SharedString = format!("ws-row-{}", workspace.id).into();

        let on_menu = on_row_menu.clone();
        let workspace_for_menu = workspace.clone();
        let menu_handler =
            move |ev: &MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
                let pos = ev.position;
                on_menu(
                    workspace_for_menu.clone(),
                    f32::from(pos.x),
                    f32::from(pos.y),
                    window,
                    cx,
                );
            };

        let row_handler =
            |_ev: &MouseDownEvent, _window: &mut gpui::Window, _cx: &mut gpui::App| {
                // v1: clicking a workspace row in the rail is a no-op. Activation
                // of a workspace's tabs lands in a later phase.
            };

        col = col.child(render_workspace_row(
            row_plan,
            row_id,
            row_group,
            theme,
            density,
            typography,
            row_handler,
            menu_handler,
        ));
    }

    col
}

fn build_header(
    plan: ProjectGroupPlan,
    project: Project,
    group_name: SharedString,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let header_fg = if plan.is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };

    let folder_icon = svg()
        .path("icons/folder.svg")
        .size(px(HEADER_ICON_SIZE))
        .text_color(header_fg);

    let title = div()
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_semibold)
        .text_color(header_fg)
        .child(plan.project_name);

    let count_chip = div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_body_sm * 0.8))
        .text_color(theme.fg_subtle)
        .child(plan.workspace_count.to_string());

    let plus_id: SharedString = format!("project-plus-{}", project.id).into();
    let weak_for_plus = weak_root.clone();
    let plus_btn = div()
        .id(plus_id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(PLUS_BTN_SIZE))
        .rounded(px(density.r_xs))
        .text_color(theme.fg_muted)
        .invisible()
        .group_hover(group_name.clone(), |s| s.visible())
        .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(HEADER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            let _ = weak_for_plus.update_in(cx, |root, window, cx| {
                root.set_active_project(project.clone(), window, cx);
            });
            window.dispatch_action(Box::new(OpenWorkspaceCreate), cx);
        });

    div()
        .group(group_name)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(HEADER_HEIGHT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(folder_icon)
        .child(title)
        .child(count_chip)
        .child(div().flex_1())
        .child(plus_btn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, name: &str) -> Project {
        Project {
            id: id.to_string(),
            name: name.to_string(),
            root_path: format!("/tmp/{id}"),
            default_branch: "main".to_string(),
            created_at: "2026-05-21T00:00:00Z".to_string(),
            last_opened_at: None,
        }
    }

    fn workspace(id: &str, project_id: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: project_id.to_string(),
            name: id.to_string(),
            slug: id.to_string(),
            branch: format!("oximux/{id}"),
            worktree_path: format!("/tmp/{project_id}/{id}"),
            status: "active".to_string(),
            created_at: "2026-05-21T00:00:00Z".to_string(),
            archived_at: None,
        }
    }

    #[test]
    fn plan_count_matches_workspace_list_len() {
        let p = project("p1", "OxiMux");
        let ws = vec![workspace("a", "p1"), workspace("b", "p1")];
        let plan = build_project_group_plan(&p, &ws, true);
        assert_eq!(plan.workspace_count, 2);
    }

    #[test]
    fn plan_carries_active_flag() {
        let p = project("p1", "OxiMux");
        let active = build_project_group_plan(&p, &[], true);
        let inactive = build_project_group_plan(&p, &[], false);
        assert!(active.is_active);
        assert!(!inactive.is_active);
    }

    #[test]
    fn plan_empty_workspace_list_is_zero_count() {
        let p = project("p1", "OxiMux");
        let plan = build_project_group_plan(&p, &[], true);
        assert_eq!(plan.workspace_count, 0);
    }

    #[test]
    fn plan_name_matches_project_name() {
        let p = project("p1", "OxiMux");
        let plan = build_project_group_plan(&p, &[], true);
        assert_eq!(plan.project_name, "OxiMux");
    }
}
