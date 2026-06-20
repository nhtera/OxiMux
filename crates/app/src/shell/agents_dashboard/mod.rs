//! Agents dashboard — the body rendered when the Agents nav item is active.
//!
//! Entry point: `render_agents_dashboard`. Accepts the same snapshot fields
//! already held by `LeftRail` (pushed down by `WorkspaceRoot::refresh_left_rail`
//! before each render) and returns a `uniform_list`-backed scrollable body.
//!
//! All data must be fully assembled BEFORE the `uniform_list` closure to avoid
//! per-frame work inside the virtualization layer.

pub mod model;
pub mod row_builder;
pub mod row_render;

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, InteractiveElement, IntoElement, ListHorizontalSizingBehavior, MouseButton,
    MouseDownEvent, ParentElement, Styled, UniformListScrollHandle, WeakEntity, Window, div, px,
    uniform_list,
};
use oximux_core::{Project, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agents_dashboard::model::AgentRow;
use crate::shell::agents_dashboard::row_builder::{build_agent_rows, widest_row_index};
use crate::shell::agents_dashboard::row_render::render_agent_row;
use crate::shell::left_rail::LatestStatusMap;
use crate::shell::left_rail::workspace_row::DiffCounts;
use crate::workspace_root::WorkspaceRoot;

/// Render the full agents dashboard body. Returns an `AnyElement` sized to
/// fill the flex-1 slot in `LeftRail::render`.
///
/// `scroll` is a `UniformListScrollHandle` held on `LeftRail` so the scroll
/// position persists between frames.
#[allow(clippy::too_many_arguments)]
pub fn render_agents_dashboard(
    projects: &[Project],
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    latest_status: &LatestStatusMap,
    live_worktrees: &HashSet<String>,
    diff_counts: &HashMap<String, DiffCounts>,
    agent_activity: &HashMap<String, String>,
    latest_adapter: &HashMap<String, String>,
    last_active: &HashMap<String, String>,
    weak_root: WeakEntity<WorkspaceRoot>,
    scroll: &UniformListScrollHandle,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    // Build + sort once before the closure — no per-frame work inside the list.
    let rows: Vec<AgentRow> = build_agent_rows(
        projects,
        workspaces_by_project,
        latest_status,
        live_worktrees,
        diff_counts,
        agent_activity,
        latest_adapter,
        last_active,
        theme,
    );

    if rows.is_empty() {
        return render_empty_state(theme, density, typography).into_any_element();
    }

    let row_count = rows.len();
    let typography_for_list = typography.clone();
    // Computed before `rows` is moved into the closure. Points the list's width
    // measurement at the widest row so long labels scroll horizontally rather
    // than clipping in the narrow rail (`Unconstrained` enables x-overflow).
    let widest = widest_row_index(&rows);

    // All row data is moved into the closure; the closure captures a
    // pre-sorted Vec so no sorting or map lookups happen per frame inside
    // the `uniform_list` virtualization layer.
    let list = uniform_list(
        "agents-dashboard-rows",
        row_count,
        move |range: std::ops::Range<usize>, _window: &mut Window, _cx: &mut App| {
            let typography = typography_for_list.clone();
            range
                .filter_map(|i| rows.get(i).cloned())
                .map(|row| {
                    build_dashboard_row(row, weak_root.clone(), theme, density, &typography)
                        .into_any_element()
                })
                .collect::<Vec<AnyElement>>()
        },
    )
    .track_scroll(scroll)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .with_width_from_item(widest)
    .h_full();

    div()
        .flex()
        .flex_col()
        .h_full()
        .w_full()
        .child(list)
        .into_any_element()
}

/// Build one interactive row element from an `AgentRow`. Wires the click
/// handler to `weak_root.update → activate_workspace` using `update` (not
/// `update_in`) — safe from a mouse-callback context with the outer `window`.
fn build_dashboard_row(
    row: AgentRow,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let workspace = row.workspace.clone();
    let row_id: gpui::SharedString = format!("agent-row-{}", row.workspace.id).into();

    render_agent_row(&row, theme, density, typography)
        .id(row_id)
        .on_mouse_down(
            MouseButton::Left,
            move |_ev: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                let ws = workspace.clone();
                // `update` + outer `window` — matches the pattern used in
                // project_group.rs for workspace card click handlers.
                let _ = weak_root.update(cx, |root, cx| {
                    root.activate_workspace(ws, window, cx);
                });
            },
        )
}

/// Quiet on-brand empty state when no agents are live across any project.
fn render_empty_state(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .h_full()
        .w_full()
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline * 2.0))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child("No agents running"),
        )
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Start an agent from any workspace to see it here."),
        )
}
