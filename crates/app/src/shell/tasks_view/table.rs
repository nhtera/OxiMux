//! Column-header row and toolbar rendering for the Tasks table.
//!
//! Both functions are pure layout builders — they receive the state they need
//! via arguments so they can live in their own module without a `self` borrow.
//! The column-header row uses the same `COL_*` constants as `row.rs` so the
//! two always align without any runtime coordination.

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Styled, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Input, InputState};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::{ForgeListFilter, ForgeState};
use crate::shell::tasks_view::row::{
    COL_ACTIONS_W, COL_ASSIGNEES_W, COL_ID_W, COL_STATUS_W, COL_UPDATED_W,
};
use crate::shell::tasks_view::{TaskKind, TasksView};

/// Single-row toolbar: `Issues|PRs` · query box · `Open|Closed|All|Mine` ·
/// `Refresh`. The query box flexes to fill the gap (its `--search` passthrough
/// is what gives the chips composability with GitHub qualifiers). Chip colors
/// are re-leveled for the content canvas (`bg_panel`) surface.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_toolbar(
    kind: TaskKind,
    filter: &ForgeListFilter,
    query_input: &Entity<InputState>,
    has_query: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<TasksView>,
) -> AnyElement {
    let state = filter.state;
    let mine = filter.mine;
    let query_box = div().flex_1().min_w_0().child(
        Input::new(query_input)
            .small()
            .shadow_none()
            .text_size(px(typography.t_body_sm)),
    );
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .px(px(density.pad_panel))
        .py(px(4.0))
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(ctrl_chip("Issues", kind == TaskKind::Issues, theme, density, typography, cx, |tv, cx| {
            tv.set_kind(TaskKind::Issues, cx)
        }))
        .child(ctrl_chip("PRs", kind == TaskKind::Prs, theme, density, typography, cx, |tv, cx| {
            tv.set_kind(TaskKind::Prs, cx)
        }))
        .child(query_box);
    // The clear control only appears while there's text to clear.
    if has_query {
        row = row.child(clear_chip(theme, density, typography, cx));
    }
    row.child(ctrl_chip("Open", state == ForgeState::Open, theme, density, typography, cx, |tv, cx| {
        tv.set_state(ForgeState::Open, cx)
    }))
    .child(ctrl_chip("Closed", state == ForgeState::Closed, theme, density, typography, cx, |tv, cx| {
        tv.set_state(ForgeState::Closed, cx)
    }))
    .child(ctrl_chip("All", state == ForgeState::All, theme, density, typography, cx, |tv, cx| {
        tv.set_state(ForgeState::All, cx)
    }))
    .child(ctrl_chip("Mine", mine, theme, density, typography, cx, |tv, cx| {
        tv.toggle_mine(cx)
    }))
    .child(div().w(px(density.gap_inline as f32 * 2.0)))
    .child(ctrl_chip("Refresh", false, theme, density, typography, cx, |tv, cx| {
        tv.refresh(cx)
    }))
    .into_any_element()
}

/// The query-box clear (`×`) control. A dedicated builder (not [`ctrl_chip`])
/// because clearing needs `&mut Window` to reset + refocus the input, which the
/// chip handler signature doesn't carry.
fn clear_chip(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<TasksView>,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_muted)
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|tv, _ev, window, cx| tv.clear_search(window, cx)),
        )
        .child("Clear".to_string())
        .into_any_element()
}

/// Column-header row — uppercase muted labels using the same `COL_*` widths
/// as `render_task_row` so data columns align exactly with the header.
pub(super) fn render_col_header(theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    let h = |label: &'static str| -> AnyElement {
        div()
            .text_size(px(typography.t_label_xs))
            .text_color(theme.fg_subtle)
            .child(label)
            .into_any_element()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .px(px(density.pad_panel))
        .py(px(3.0))
        .gap(px(density.gap_inline))
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(div().flex_none().w(px(COL_ID_W)).child(h("ID")))
        .child(div().flex_1().min_w_0().child(h("TITLE / CONTEXT")))
        .child(div().flex_none().w(px(COL_ASSIGNEES_W)).child(h("ASSIGNEES")))
        .child(div().flex_none().w(px(COL_STATUS_W)).child(h("STATUS")))
        .child(div().flex_none().w(px(COL_UPDATED_W)).child(h("UPDATED")))
        .child(div().flex_none().w(px(COL_ACTIONS_W)))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Internal: toolbar chip builder
// ---------------------------------------------------------------------------

fn ctrl_chip(
    label: &str,
    active: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<TasksView>,
    handler: impl Fn(&mut TasksView, &mut Context<TasksView>) + 'static,
) -> AnyElement {
    let (fg, bg) = if active {
        (theme.fg_base, theme.bg_overlay)
    } else {
        (theme.fg_muted, theme.bg_panel_alt)
    };
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(density.r_xs))
        .bg(bg)
        .text_size(px(typography.t_label_xs))
        .text_color(fg)
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |tv, _ev, _window, cx| handler(tv, cx)),
        )
        .child(label.to_string())
        .into_any_element()
}
