//! Project group renderer — header row (folder icon, name, count chip,
//! hover-visible "+" button) followed by zero or more Workspace rows.
//!
//! The "+" button always sets the project as active first, then
//! dispatches `OpenWorkspaceCreate` — single code path regardless of
//! whether the project was already active when the user clicked.

use gpui::{
    Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, div, px, svg,
};
use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::OpenWorkspaceCreate;
use crate::shell::left_rail::LeftRail;
use crate::shell::left_rail::workspace_card::render_workspace_card;
use crate::shell::left_rail::workspace_row::{DiffCounts, build_workspace_card_plan};
use crate::workspace_root::WorkspaceRoot;

/// Chevron / folder glyph size in the header.
const CHEVRON_ICON_SIZE: f32 = 12.0;

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
    /// `true` when the group is collapsed — workspace rows are hidden
    /// and the chevron points right instead of down.
    pub is_collapsed: bool,
}

pub fn build_project_group_plan(
    project: &Project,
    workspaces: &[Workspace],
    is_active: bool,
    is_collapsed: bool,
) -> ProjectGroupPlan {
    ProjectGroupPlan {
        project_name: project.name.clone(),
        workspace_count: workspaces.len(),
        is_active,
        is_collapsed,
    }
}

/// Render one project group: header + its workspace cards.
///
/// `latest_status_for(workspace_id)` resolves the latest agent-session
/// status for the dot color; the lookup is injected so the caller decides
/// whether to query a HashMap (cached map) or call the repo directly.
///
/// `diff_counts` is keyed by worktree path; a missing entry means the count
/// is not yet cached — the card renders without the diff chip.
#[allow(clippy::too_many_arguments)]
pub fn render_project_group(
    plan: ProjectGroupPlan,
    project: Project,
    workspaces: Vec<Workspace>,
    latest_status_for: impl Fn(&str) -> Option<AgentStatus>,
    active_workspace_id: Option<&str>,
    live_worktrees: &std::collections::HashSet<String>,
    diff_counts: &std::collections::HashMap<String, DiffCounts>,
    rail: Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
    on_row_menu: impl Fn(Workspace, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
    on_project_menu: impl Fn(Project, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let group_name: SharedString = format!("project-group-{}", project.id).into();
    let is_collapsed = plan.is_collapsed;

    let header = build_header(
        plan,
        project.clone(),
        group_name.clone(),
        rail,
        weak_root.clone(),
        on_project_menu,
        theme,
        density,
        typography,
    );

    let mut col = div().flex().flex_col().w_full().child(header);

    // Collapsed groups render the header only.
    if is_collapsed {
        return col;
    }

    for workspace in workspaces {
        let row_group: SharedString = format!("ws-row-{}", workspace.id).into();
        let is_active = active_workspace_id == Some(workspace.id.as_str());
        // The main worktree lives at the project root; that row is the
        // project's primary (the repo's main worktree). A primary
        // row with no branch is a non-git folder project → "Folder" badge.
        let is_primary = workspace.worktree_path == project.root_path;
        let is_folder = is_primary && workspace.branch.is_empty();
        let is_live = live_worktrees.contains(&workspace.worktree_path);
        let latest = latest_status_for(&workspace.id);
        // Diff counts are looked up from the pushed-down cache; `None` means
        // not yet available — the card renders without the chip.
        let diff = diff_counts.get(&workspace.worktree_path).cloned();
        let card_plan = build_workspace_card_plan(
            &workspace,
            is_active,
            is_primary,
            is_folder,
            is_live,
            latest.as_ref(),
            diff,
            theme,
        );
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

        // Clicking a card activates the workspace: switch to its project,
        // select it (highlight), and focus the agent tab running in its
        // worktree. `update` + outer `window` — `update_in` returns Err
        // from a mouse-callback context.
        let weak_root_for_row = weak_root.clone();
        let workspace_for_row = workspace.clone();
        let row_handler =
            move |_ev: &MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
                let workspace = workspace_for_row.clone();
                let _ = weak_root_for_row
                    .update(cx, |root, cx| root.activate_workspace(workspace, window, cx));
            };

        col = col.child(render_workspace_card(
            card_plan,
            row_id,
            row_group,
            !is_primary,
            theme,
            density,
            typography,
            row_handler,
            menu_handler,
        ));
    }

    col
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    plan: ProjectGroupPlan,
    project: Project,
    group_name: SharedString,
    rail: Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
    on_project_menu: impl Fn(Project, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let header_fg = if plan.is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    let is_collapsed = plan.is_collapsed;

    // Leading icon slot — matches the reference UX: the folder icon shows at rest
    // and is replaced in-place by a disclosure chevron on header hover
    // (no layout shift, folder stays flush-left). Clicking the chevron
    // toggles collapse; stop_propagation keeps it off the header's
    // activate-project handler. Chevron points right when collapsed,
    // down when expanded.
    let chevron_id: SharedString = format!("project-chevron-{}", project.id).into();
    let chevron_path = if is_collapsed {
        "icons/chevron-right.svg"
    } else {
        "icons/chevron-down.svg"
    };
    let rail_for_chevron = rail.clone();
    let project_id_for_chevron = project.id.clone();
    let icon_slot = div()
        .relative()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .size(px(HEADER_ICON_SIZE + 2.0))
        // Folder — hidden while the header is hovered.
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .group_hover(group_name.clone(), |s| s.invisible())
                .child(
                    svg()
                        .path("icons/folder.svg")
                        .size(px(HEADER_ICON_SIZE))
                        .text_color(header_fg),
                ),
        )
        // Chevron — overlaid, revealed on hover, click toggles collapse.
        .child(
            div()
                .id(chevron_id)
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .invisible()
                .cursor_pointer()
                .text_color(theme.fg_muted)
                .hover(|s| s.text_color(theme.fg_base))
                .group_hover(group_name.clone(), |s| s.visible())
                .child(
                    svg()
                        .path(chevron_path)
                        .size(px(CHEVRON_ICON_SIZE))
                        .text_color(theme.fg_muted),
                )
                .tooltip(|window, cx| {
                    gpui_component::tooltip::Tooltip::new("Collapse / expand").build(window, cx)
                })
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    cx.stop_propagation();
                    let id = project_id_for_chevron.clone();
                    rail_for_chevron.update(cx, |r, cx| r.toggle_collapsed(id, cx));
                }),
        );

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
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(plan.workspace_count.to_string());

    let plus_id: SharedString = format!("project-plus-{}", project.id).into();
    let plus_tooltip: SharedString = format!("Create workspace for {}", project.name).into();
    let weak_for_plus = weak_root.clone();
    let project_for_plus = project.clone();
    let plus_btn = div()
        .id(plus_id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(PLUS_BTN_SIZE))
        .rounded(px(density.r_xs))
        .text_color(theme.fg_muted)
        .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(HEADER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(plus_tooltip.clone()).build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            let project = project_for_plus.clone();
            // `update` + outer `window` — `update_in` does a with_window
            // lookup that returns Err from a mouse callback context.
            let _ = weak_for_plus.update(cx, |root, cx| {
                root.set_active_project(project, window, cx);
            });
            window.dispatch_action(Box::new(OpenWorkspaceCreate), cx);
        });

    // "…" more-actions button — hidden at rest, revealed on header hover
    // (matching the chevron / workspace-row menu affordance). Opens the
    // project popover (Reveal / Copy / Remove) anchored at the click point.
    let menu_id: SharedString = format!("project-menu-{}", project.id).into();
    let project_for_menu = project.clone();
    let menu_btn = div()
        .id(menu_id)
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
                .path("icons/ellipsis.svg")
                .size(px(HEADER_ICON_SIZE))
                .text_color(theme.fg_muted),
        )
        .tooltip(|window, cx| {
            gpui_component::tooltip::Tooltip::new("Project actions").build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            let pos = ev.position;
            on_project_menu(
                project_for_menu.clone(),
                f32::from(pos.x),
                f32::from(pos.y),
                window,
                cx,
            );
        });

    // Click anywhere on the header (folder / title / count chip / gap) to
    // activate this project. The "+" button stop-propagates so it stays
    // a workspace-create shortcut, not an activate-and-open one.
    let header_id: SharedString = format!("project-header-{}", project.id).into();
    let weak_for_header = weak_root.clone();
    let project_for_header = project.clone();
    div()
        .id(header_id)
        .group(group_name)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(HEADER_HEIGHT))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            // `update` + outer `window` — `update_in` does a with_window
            // lookup that returns Err from a mouse callback context.
            let project = project_for_header.clone();
            let _ = weak_for_header.update(cx, |root, cx| {
                root.set_active_project(project, window, cx);
            });
        })
        .child(icon_slot)
        .child(title)
        .child(count_chip)
        .child(div().flex_1())
        .child(menu_btn)
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
            linked_issue: None,
        }
    }

    #[test]
    fn plan_count_matches_workspace_list_len() {
        let p = project("p1", "OxiMux");
        let ws = vec![workspace("a", "p1"), workspace("b", "p1")];
        let plan = build_project_group_plan(&p, &ws, true, false);
        assert_eq!(plan.workspace_count, 2);
    }

    #[test]
    fn plan_carries_active_flag() {
        let p = project("p1", "OxiMux");
        let active = build_project_group_plan(&p, &[], true, false);
        let inactive = build_project_group_plan(&p, &[], false, false);
        assert!(active.is_active);
        assert!(!inactive.is_active);
    }

    #[test]
    fn plan_empty_workspace_list_is_zero_count() {
        let p = project("p1", "OxiMux");
        let plan = build_project_group_plan(&p, &[], true, false);
        assert_eq!(plan.workspace_count, 0);
    }

    #[test]
    fn plan_name_matches_project_name() {
        let p = project("p1", "OxiMux");
        let plan = build_project_group_plan(&p, &[], true, false);
        assert_eq!(plan.project_name, "OxiMux");
    }

    #[test]
    fn plan_carries_collapsed_flag() {
        let p = project("p1", "OxiMux");
        let collapsed = build_project_group_plan(&p, &[], false, true);
        let expanded = build_project_group_plan(&p, &[], false, false);
        assert!(collapsed.is_collapsed);
        assert!(!expanded.is_collapsed);
    }
}
