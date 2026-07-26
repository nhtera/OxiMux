//! Project group renderer — header row (folder icon, name, count chip,
//! hover-visible "+" button) followed by zero or more Workspace rows.
//!
//! The "+" button always sets the project as active first, then
//! dispatches `OpenWorkspaceCreate` — single code path regardless of
//! whether the project was already active when the user clicked.

use gpui::{
    AppContext, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, WeakEntity, div, px, svg,
};
use gpui_component::input::InputState;
use oximux_core::{AgentStatus, Project, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::OpenWorkspaceCreate;
use crate::shell::agent_presentation::AmbientAgent;
use crate::shell::left_rail::LeftRail;
use crate::shell::left_rail::project_drag::{
    ProjectDragPayload, SidebarDragPreview, insertion_side, paint_insertion_line,
};
use crate::shell::left_rail::workspace_card::{RowRenameConfig, render_workspace_card};
use crate::shell::left_rail::workspace_list_render::WorkspaceSortMode;
use crate::shell::left_rail::workspace_row::{DiffCounts, build_workspace_card_plan};
use crate::workspace_root::WorkspaceRoot;

/// Chevron / folder glyph size in the header.
const CHEVRON_ICON_SIZE: f32 = 12.0;

/// Diameter of the per-project identity hue dot in the header.
const IDENTITY_DOT_SIZE: f32 = 6.0;

/// Deterministic low-saturation identity hue for a project, derived from its
/// id so the same project always shows the same accent across restarts AND
/// across Rust toolchain upgrades. Uses a fixed FNV-1a fold over the id bytes
/// rather than `DefaultHasher` (whose SipHash key is process/toolchain-seeded
/// and would reshuffle the palette on a `rustc` upgrade). Low saturation + mid
/// lightness keeps the accent calm and legible on the dark rail without
/// competing with the workspace status dots.
fn project_identity_hue(project_id: &str) -> gpui::Hsla {
    let hash = project_id
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let hue = (hash % 360) as f32 / 360.0;
    gpui::hsla(hue, 0.45, 0.62, 1.0)
}

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
    project_index: usize,
    sort_mode: WorkspaceSortMode,
    workspaces: Vec<Workspace>,
    latest_status_for: impl Fn(&str) -> Option<AgentStatus>,
    latest_adapter_for: impl Fn(&str) -> Option<&'static str>,
    active_workspace_id: Option<&str>,
    live_worktrees: &std::collections::HashSet<String>,
    ambient_by_path: &std::collections::HashMap<String, AmbientAgent>,
    diff_counts: &std::collections::HashMap<String, DiffCounts>,
    workspace_agents: &crate::shell::left_rail::WorkspaceAgentList,
    expanded_workspaces: &std::collections::HashSet<String>,
    focused_agent: Option<&crate::shell::left_rail::RailAgentTarget>,
    rail: Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
    on_row_menu: impl Fn(Workspace, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
    on_project_menu: impl Fn(Project, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
    locate_glow_seq: u64,
    renaming_id: Option<&str>,
    rename_input: Option<Entity<InputState>>,
    compact: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let group_name: SharedString = format!("project-group-{}", project.id).into();
    let is_collapsed = plan.is_collapsed;

    let header = build_header(
        plan,
        project.clone(),
        project_index,
        group_name.clone(),
        rail.clone(),
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

    for (row_index, workspace) in workspaces.into_iter().enumerate() {
        col = col.child(render_workspace_block(
            workspace,
            row_index,
            &project,
            sort_mode,
            &latest_status_for,
            &latest_adapter_for,
            active_workspace_id,
            live_worktrees,
            ambient_by_path,
            diff_counts,
            workspace_agents,
            expanded_workspaces,
            focused_agent,
            &rail,
            &weak_root,
            &on_row_menu,
            true,
            locate_glow_seq,
            renaming_id,
            &rename_input,
            compact,
            theme,
            density,
            typography,
        ));
    }

    col
}

/// Render one workspace's block: the card plus, for multi-agent workspaces,
/// the agent disclosure, optionally wrapped in the active-selection container.
///
/// Extracted so the grouped list and the flat (ungrouped) list render identical
/// rows. `allow_drag` is `false` in flat mode, where cross-project reordering
/// has no meaning.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_workspace_block(
    workspace: Workspace,
    row_index: usize,
    project: &Project,
    sort_mode: WorkspaceSortMode,
    latest_status_for: &impl Fn(&str) -> Option<AgentStatus>,
    latest_adapter_for: &impl Fn(&str) -> Option<&'static str>,
    active_workspace_id: Option<&str>,
    live_worktrees: &std::collections::HashSet<String>,
    ambient_by_path: &std::collections::HashMap<String, AmbientAgent>,
    diff_counts: &std::collections::HashMap<String, DiffCounts>,
    workspace_agents: &crate::shell::left_rail::WorkspaceAgentList,
    expanded_workspaces: &std::collections::HashSet<String>,
    focused_agent: Option<&crate::shell::left_rail::RailAgentTarget>,
    rail: &Entity<LeftRail>,
    weak_root: &WeakEntity<WorkspaceRoot>,
    on_row_menu: &(impl Fn(Workspace, f32, f32, &mut gpui::Window, &mut gpui::App) + Clone + 'static),
    allow_drag: bool,
    locate_glow_seq: u64,
    renaming_id: Option<&str>,
    rename_input: &Option<Entity<InputState>>,
    compact: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    {
        let row_group: SharedString = format!("ws-row-{}", workspace.id).into();
        let is_active = active_workspace_id == Some(workspace.id.as_str());
        // The main worktree lives at the project root; that row is the
        // project's primary (the repo's main worktree). A primary
        // row with no branch is a non-git folder project → "Folder" badge.
        let is_primary = workspace.worktree_path == project.root_path;
        let is_folder = is_primary && workspace.branch.is_empty();
        let is_live = live_worktrees.contains(&workspace.worktree_path);
        // Combine the tracked session status with any live ambient reading
        // from a hand-launched agent in this worktree's terminal: an active
        // tracked session stays authoritative, otherwise the ambient reading
        // overrides a stale/absent one (see `resolve_effective_status`).
        let tracked = latest_status_for(&workspace.id);
        let tracked_present = tracked.is_some();
        let tracked_is_active = matches!(
            tracked.as_ref(),
            Some(AgentStatus::Running)
                | Some(AgentStatus::WaitingForInput)
                | Some(AgentStatus::NeedsApproval(_))
        );
        let ambient = ambient_by_path.get(&workspace.worktree_path);
        let latest = crate::shell::agent_presentation::resolve_effective_status(
            tracked,
            ambient.map(|a| a.status.clone()),
        );
        // Agent display name follows the same precedence as status: an active
        // tracked session stays authoritative; otherwise an ambient terminal's
        // title can surface over stale DB history.
        let agent_name: Option<SharedString> = if tracked_is_active {
            latest_adapter_for(&workspace.id)
        } else {
            ambient.and_then(|a| a.label).or_else(|| {
                tracked_present
                    .then(|| latest_adapter_for(&workspace.id))
                    .flatten()
            })
        }
        .map(SharedString::new_static);
        // Diff counts are looked up from the pushed-down cache; `None` means
        // not yet available — the card renders without the chip.
        let diff = diff_counts.get(&workspace.worktree_path).cloned();
        // A single-agent workspace surfaces the agent's prompt as the card's
        // title (the dot still carries status). This includes a restored,
        // non-live agent: its persisted title shows after a restart instead of
        // decaying to the bare status verb, matching the reference cockpit.
        // Multi-agent workspaces (rows > 1) defer to the disclosure below, so
        // they stay on the `name · verb` summary.
        let agent_title = workspace_agents
            .get(&workspace.id)
            .filter(|rows| rows.len() == 1)
            .and_then(|rows| rows.first())
            .and_then(crate::shell::left_rail::workspace_agent_rows::live_title)
            .map(|t| SharedString::from(t.text));
        let card_plan = build_workspace_card_plan(
            &workspace,
            is_active,
            is_primary,
            is_folder,
            is_live,
            latest.as_ref(),
            agent_name,
            agent_title,
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

        // This row is the one being renamed inline — its title becomes an edit
        // field and its activate/drag handlers are suppressed so typing and
        // clicking inside the field don't switch workspaces or start a drag.
        let is_renaming = renaming_id == Some(workspace.id.as_str());

        // Clicking a card activates the workspace: switch to its project,
        // select it (highlight), and focus the agent tab running in its
        // worktree. `update` + outer `window` — `update_in` returns Err
        // from a mouse-callback context. Suppressed while renaming.
        let weak_root_for_row = weak_root.clone();
        let workspace_for_row = workspace.clone();
        let row_handler =
            move |_ev: &MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
                if is_renaming {
                    return;
                }
                let workspace = workspace_for_row.clone();
                let _ = weak_root_for_row.update(cx, |root, cx| {
                    root.activate_workspace(workspace, window, cx)
                });
            };

        // Inline-rename wiring for non-primary rows: carries the rail entity +
        // this row's workspace (the double-click begin payload) and, when this
        // row is the active one, the shared edit field.
        let rename_config = (!is_primary).then(|| RowRenameConfig {
            rail: rail.clone(),
            workspace: workspace.clone(),
            active_input: if is_renaming {
                rename_input.clone()
            } else {
                None
            },
        });

        // Drag-to-reorder is offered only for non-primary rows while the rail
        // is in Manual sort mode (Smart/Recent are computed orders — dragging
        // there would be immediately overwritten). The pinned primary row is
        // never draggable and never a drop target. A row being renamed is not
        // draggable. Pinned rows are excluded too: they float to the top of
        // their group regardless of rank, so a free-drag would silently no-op
        // on the display.
        let drag_config = (allow_drag
            && !is_primary
            && !workspace.pinned
            && sort_mode == WorkspaceSortMode::Manual
            && !is_renaming)
            .then(|| {
                let weak_root_for_reorder = weak_root.clone();
                let project_id_for_reorder = project.id.clone();
                crate::shell::left_rail::project_drag::WorkspaceDragConfig {
                    workspace_id: workspace.id.clone(),
                    project_id: project.id.clone(),
                    src_index: row_index,
                    accent: theme.selection,
                    ghost_label: workspace.name.clone().into(),
                    on_reorder: std::rc::Rc::new(
                        move |moved_id: String,
                              target_id: String,
                              window: &mut gpui::Window,
                              cx: &mut gpui::App| {
                            let project_id = project_id_for_reorder.clone();
                            let _ = weak_root_for_reorder.update(cx, |root, cx| {
                                root.reorder_workspace(moved_id, target_id, project_id, window, cx);
                            });
                        },
                    ),
                }
            });

        let agent_rows = workspace_agents.get(&workspace.id);
        let multi_agent = agent_rows.is_some_and(|rows| rows.len() > 1);
        let active_agent_wrap = is_active && multi_agent;
        // The active surface fill, reused for the wrap container so a
        // multi-agent active workspace reads as ONE highlighted block (card +
        // its agent rows) — matching the single-agent active card exactly.
        let active_surface_bg = card_plan.row.bg;

        let mut workspace_block = div().flex().flex_col();
        if !active_agent_wrap {
            workspace_block = workspace_block.w_full();
        }
        workspace_block = workspace_block.child(render_workspace_card(
            card_plan,
            row_id,
            row_group,
            !active_agent_wrap,
            multi_agent,
            !is_primary,
            locate_glow_seq,
            drag_config,
            rename_config,
            compact,
            theme,
            density,
            typography,
            row_handler,
            menu_handler,
        ));

        // Multi-agent disclosure: only when a workspace has more than one
        // agent. A single-agent workspace keeps the row's own status dot.
        if let Some(rows) = agent_rows
            && rows.len() > 1
        {
            let is_expanded = expanded_workspaces.contains(&workspace.id);
            workspace_block = workspace_block.child(
                crate::shell::left_rail::workspace_agent_rows::render_workspace_agent_disclosure(
                    &workspace.id,
                    rows,
                    is_expanded,
                    focused_agent,
                    rail.clone(),
                    weak_root.clone(),
                    theme,
                    density,
                    typography,
                ),
            );
        }

        if active_agent_wrap {
            // One highlighted container around the card + its agent rows, like
            // the reference cockpit. `border_active` keeps the box clearly
            // delineated; the fill matches the single-agent active card.
            // `relative` hosts the locate glow so a multi-agent workspace pulses
            // on scroll-to-current exactly like a single-agent card — the card's
            // own glow is suppressed here because its active border moved to this
            // wrapper. No left inset on the ring: the wrapper already carries it.
            let wrap_glow = (locate_glow_seq > 0).then(|| {
                crate::shell::left_rail::workspace_card::locate_glow_overlay(
                    locate_glow_seq,
                    px(density.r_card),
                    px(0.0),
                    theme.focus_ring,
                )
            });
            workspace_block = workspace_block
                .relative()
                .ml(px(density.gap_inline))
                .rounded(px(density.r_card))
                .border_1()
                .border_color(theme.border_active)
                .bg(active_surface_bg)
                .children(wrap_glow);
        }

        workspace_block
    }
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    plan: ProjectGroupPlan,
    project: Project,
    project_index: usize,
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

    // Leading icon slot: the folder icon shows at rest
    // and is replaced in-place by a disclosure chevron on header hover
    // (no layout shift, folder stays flush-left). The chevron is a pure
    // visual indicator — the collapse toggle lives on the whole header row
    // (see the header's `on_click`), so clicking anywhere on the header
    // expands/collapses. Chevron points right when collapsed, down when
    // expanded.
    let chevron_id: SharedString = format!("project-chevron-{}", project.id).into();
    let chevron_path = if is_collapsed {
        "icons/chevron-right.svg"
    } else {
        "icons/chevron-down.svg"
    };
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
                .text_color(theme.fg_muted)
                .group_hover(group_name.clone(), |s| s.visible())
                .child(
                    svg()
                        .path(chevron_path)
                        .size(px(CHEVRON_ICON_SIZE))
                        .text_color(theme.fg_muted),
                ),
        );

    // Per-project identity dot — a small deterministic hue derived from the
    // project id, giving each project a stable color anchor instead of an
    // identical mono folder. Sits just before the title, set apart from the
    // workspace status dots that live on the cards below.
    let identity_dot = div()
        .size(px(IDENTITY_DOT_SIZE))
        .rounded_full()
        .flex_shrink_0()
        .bg(project_identity_hue(&project.id));

    let title = div()
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_semibold)
        .text_color(header_fg)
        .child(plan.project_name);

    let count_chip = div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_overlay)
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
        // Hidden at rest, revealed on header hover (matching the `…` button and
        // the chevron). The count chip + identity dot stay visible — they carry
        // information, not actions.
        .invisible()
        .group_hover(group_name.clone(), |s| s.visible())
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
    // Right-click anywhere on the header opens the same project popover at the
    // cursor (DRY — identical actions to the `…` button).
    let on_project_menu_ctx = on_project_menu.clone();
    let project_for_ctx = project.clone();
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
    // activate this project AND toggle its collapse state via a
    // whole-row disclosure. Activate fires on mouse-down (so a drag-reorder
    // still focuses the project); the collapse toggle fires on `on_click`,
    // which GPUI suppresses after a drag — so reordering a header never
    // collapses it. The "+" / "…" buttons stop-propagate so they stay their
    // own shortcuts, not activate-and-toggle ones.
    let header_id: SharedString = format!("project-header-{}", project.id).into();
    let weak_for_header = weak_root.clone();
    let project_for_header = project.clone();
    let rail_for_toggle = rail.clone();
    let project_id_for_toggle = project.id.clone();
    // Drag payload + ghost: dragging a header reorders the project list. The
    // payload carries the start index; the drop handler recomputes from the
    // live list by id, so a concurrent reorder can't misplace it.
    let drag_payload = ProjectDragPayload {
        project_id: project.id.clone(),
        src_index: project_index,
    };
    let ghost_label = project.name.clone();
    let accent = theme.selection;
    let weak_for_drop = weak_root.clone();
    let project_id_for_drop = project.id.clone();
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
        .hover(|s| s.bg(theme.hover_overlay))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            // `update` + outer `window` — `update_in` does a with_window
            // lookup that returns Err from a mouse callback context.
            let project = project_for_header.clone();
            let _ = weak_for_header.update(cx, |root, cx| {
                root.set_active_project(project, window, cx);
            });
        })
        .on_click(move |_ev, window, cx| {
            let id = project_id_for_toggle.clone();
            rail_for_toggle.update(cx, |r, cx| r.toggle_collapsed(id, window, cx));
        })
        .on_mouse_down(
            MouseButton::Right,
            move |ev: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let pos = ev.position;
                on_project_menu_ctx(
                    project_for_ctx.clone(),
                    f32::from(pos.x),
                    f32::from(pos.y),
                    window,
                    cx,
                );
            },
        )
        .on_drag(drag_payload, move |_p, _offset, _window, cx| {
            cx.new(|_| SidebarDragPreview::new(ghost_label.clone(), theme))
        })
        // Stateless insertion indicator: a 2px accent line on the edge the
        // dragged header would land on. Suppressed on the source row itself
        // (dropping in place is a no-op).
        .drag_over::<ProjectDragPayload>(move |style, payload, _window, _cx| {
            if payload.src_index == project_index {
                return style;
            }
            paint_insertion_line(
                style,
                insertion_side(payload.src_index, project_index),
                accent,
                theme.bg_rail,
            )
        })
        .on_drop::<ProjectDragPayload>(move |payload: &ProjectDragPayload, window, cx| {
            // Drop in place: no write, no flicker.
            if payload.project_id == project_id_for_drop {
                return;
            }
            let moved_id = payload.project_id.clone();
            let _ = weak_for_drop.update(cx, |root, cx| {
                root.reorder_project(moved_id, project_index, window, cx);
            });
        })
        .child(icon_slot)
        .child(identity_dot)
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
            sort_order: 0.0,
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
            tint: None,
            sort_order: 0.0,
            pinned: false,
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

    #[test]
    fn project_identity_hue_uses_fixed_fnv1a() {
        // The dot colour must be a pure function of the id bytes — identical
        // across Rust toolchains. Recompute FNV-1a (32-bit) independently here
        // so a regression to a process/toolchain-seeded hasher (the old
        // `DefaultHasher` behaviour) is caught: this test IS the algorithm spec.
        fn fnv1a_hue(id: &str) -> f32 {
            let h = id
                .bytes()
                .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
            (h % 360) as f32 / 360.0
        }
        for id in ["oximux", "graphify-rs", "abc-123", ""] {
            let c = project_identity_hue(id);
            assert!(
                (c.h - fnv1a_hue(id)).abs() < f32::EPSILON,
                "hue must match fixed FNV-1a for {id:?}"
            );
            // Saturation/lightness are fixed so the dots stay calm; only hue varies.
            assert_eq!(c.s, 0.45);
            assert_eq!(c.l, 0.62);
        }
        // Same id always yields the same colour across calls.
        assert_eq!(
            project_identity_hue("oximux"),
            project_identity_hue("oximux")
        );
    }
}
