//! Agents dashboard — the body rendered when the Agents nav item is active.
//!
//! Entry point: `render_agents_dashboard`. Accepts the same snapshot fields
//! already held by `LeftRail` (pushed down by `WorkspaceRoot::refresh_left_rail`
//! before each render) and returns a `uniform_list`-backed scrollable body.
//!
//! All data must be fully assembled BEFORE the `uniform_list` closure to avoid
//! per-frame work inside the virtualization layer.

pub mod filter;
pub mod model;
pub mod row_builder;
pub mod row_render;
pub mod sections;

use std::collections::HashMap;

use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, ScrollHandle, StatefulInteractiveElement, Styled, WeakEntity, Window, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Escape as InputEscape, Input, InputState};
use oximux_core::{Project, Workspace};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agents_dashboard::filter::{StatusFilter, apply_filter};
use crate::shell::agents_dashboard::model::AgentRow;
use crate::shell::agents_dashboard::row_builder::build_agent_rows;
use crate::shell::agents_dashboard::row_render::{render_agent_row, render_section_header};
use crate::shell::agents_dashboard::sections::{DashboardItem, group_into_sections};
use crate::shell::left_rail::{LeftRail, RailAgentTarget, WorkspaceAgentList};
use crate::workspace_root::WorkspaceRoot;

/// Render the full agents dashboard body (a sticky filter header + a sectioned
/// scroll column). Returns an `AnyElement` sized to fill the flex-1 slot in
/// `LeftRail::render`.
///
/// `scroll` is a `ScrollHandle` held on `LeftRail` so the scroll position
/// persists between frames; `rail` is the `LeftRail` entity the header chip
/// updates; `filter_input` is its lazily-built text-filter widget.
#[allow(clippy::too_many_arguments)]
pub fn render_agents_dashboard(
    workspace_agents: &WorkspaceAgentList,
    projects: &[Project],
    workspaces_by_project: &HashMap<String, Vec<Workspace>>,
    now: &str,
    focused_agent: Option<&RailAgentTarget>,
    status_filter: StatusFilter,
    filter_text: &str,
    filter_input: Entity<InputState>,
    rail: Entity<LeftRail>,
    weak_root: WeakEntity<WorkspaceRoot>,
    scroll: &ScrollHandle,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    // Build + sort once, narrow by the filter, then group into status sections.
    let all_rows: Vec<AgentRow> =
        build_agent_rows(workspace_agents, projects, workspaces_by_project, now);
    let had_any = !all_rows.is_empty();
    let rows = apply_filter(all_rows, filter_text, status_filter);

    let header = render_dashboard_header(filter_input, status_filter, rail, theme, density, typography);

    let body: AnyElement = if rows.is_empty() {
        // Distinguish "nothing running" from "filtered to nothing" so the user
        // knows to relax the filter rather than thinking no agents exist.
        let (title, subtitle) = if had_any {
            ("No matching agents", "Adjust the filter or status above.")
        } else {
            (
                "No agents running",
                "Start an agent from any workspace to see it here.",
            )
        };
        render_empty_state(title, subtitle, theme, density, typography).into_any_element()
    } else {
        // Plain scroll column (not `uniform_list`): section headers and cards
        // have different heights, which a uniform list can't virtualize. The
        // list is bounded by the per-workspace history cap, so this is fine.
        let mut col = div()
            .id("agents-dashboard-list")
            .flex()
            .flex_col()
            .w_full()
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(scroll);
        for item in group_into_sections(rows) {
            col = match item {
                DashboardItem::Header { kind, count } => {
                    col.child(render_section_header(kind, count, theme, density, typography))
                }
                DashboardItem::Row(row) => col.child(build_dashboard_row(
                    *row,
                    focused_agent,
                    weak_root.clone(),
                    theme,
                    density,
                    typography,
                )),
            };
        }
        col.into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .h_full()
        .w_full()
        .child(header)
        .child(body)
        .into_any_element()
}

/// The filter header: a text `Filter…` input (Escape clears it) plus a status
/// chip that cycles the section filter on click — the reference cockpit's
/// `Filter` + `Status` controls.
fn render_dashboard_header(
    filter_input: Entity<InputState>,
    status_filter: StatusFilter,
    rail: Entity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let rail_for_clear = rail.clone();
    // At rest the chip reads "Status" (the field, like the reference cockpit);
    // an active filter shows the chosen bucket so the narrowing is visible.
    let chip_label = if status_filter == StatusFilter::All {
        "Status".to_string()
    } else {
        status_filter.label().to_string()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .flex_shrink_0()
        .gap(px(density.gap_inline))
        .px(px(density.pad_panel))
        .py(px(density.gap_inline))
        .child(
            // A compact (.small() = 30px) input with a clear button, so it
            // matches the status control's height. Escape is captured before
            // the Input's own handling so it clears the filter.
            div()
                .flex_1()
                .min_w_0()
                .capture_action(move |_: &InputEscape, window, cx| {
                    rail_for_clear.update(cx, |r, cx| r.clear_dashboard_filter(window, cx));
                })
                .child(Input::new(&filter_input).small().cleanable(true)),
        )
        .child(
            // A bordered control the SAME height as the input, so the two read
            // as a matched pair (the reference cockpit's filter + status row).
            div()
                .id("agents-status-filter")
                .flex_shrink_0()
                .h(px(30.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .px(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border_inactive)
                .bg(theme.bg_panel)
                .cursor_pointer()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .hover(|s| s.bg(theme.hover_overlay).text_color(theme.fg_base))
                .child(format!("{chip_label} ▾"))
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    rail.update(cx, |r, cx| r.cycle_dashboard_status_filter(cx));
                }),
        )
}

/// Build one interactive card from an `AgentRow`. Clicking focuses THIS
/// agent's tab (tracked) or terminal (ambient) — like the rail sub-rows — via
/// `update` (not `update_in`), safe from a mouse-callback context with the
/// outer `window`. The focused row keeps a resting highlight.
fn build_dashboard_row(
    row: AgentRow,
    focused_agent: Option<&RailAgentTarget>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let row_id: gpui::SharedString = format!("agent-row-{}", row.db_id).into();
    let is_focused = focused_agent == Some(&row.target);
    let target = row.target.clone();

    let mut card = render_agent_row(&row, theme, density, typography).id(row_id);
    if is_focused {
        card = card.bg(theme.hover_overlay);
    }
    card.on_mouse_down(
        MouseButton::Left,
        move |_ev: &MouseDownEvent, window: &mut Window, cx: &mut App| {
            let _ = weak_root.update(cx, |root, cx| match &target {
                RailAgentTarget::AgentSession { db_id } => {
                    root.focus_agent_by_db_id(db_id, window, cx);
                }
                RailAgentTarget::AmbientTerminal { pty_id } => {
                    root.focus_ambient_agent_terminal(pty_id, window, cx);
                }
            });
        },
    )
}

/// Quiet on-brand empty state when no agents are live across any project.
fn render_empty_state(
    title: &str,
    subtitle: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .flex_1()
        .w_full()
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline * 2.0))
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(subtitle.to_string()),
        )
}
