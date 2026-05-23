//! Render for `PaneGroup`: per-leaf tab strip + active content area.
//!
//! Layout: a horizontal tab strip at the top + the active tab's content
//! filling the remainder. Each tab carries an icon (file vs terminal),
//! a label, and an "×" close button that appears on hover or when the
//! tab is active. Click a tab to activate; mouse-down on the × closes
//! the tab without activating it.
//!
//! Action handlers for `NewTab`/`CloseTab`/`NextTab`/`PrevTab`/`NewAgent`
//! live on this group's root container so keystrokes bubbling up from
//! the focused leaf hit the right group's logic (not the entire
//! workspace's).

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder, px, svg,
};
use oximux_settings::Theme;

use super::{PaneGroup, PaneGroupTabKind};
use crate::actions::RequestOpenAdapterPicker;
use crate::shell::agent_status_badge::render_dot;
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::pane_content::PaneContent;

pub const TAB_STRIP_HEIGHT_PX: f32 = 32.0;
const TAB_PAD_X_PX: f32 = 12.0;
const CLOSE_BUTTON_SIZE_PX: f32 = 14.0;
const ICON_SIZE_PX: f32 = 11.0;
const PLUS_BUTTON_WIDTH_PX: f32 = 28.0;
/// Match the workspace chrome (top bar + status bar) so terminal grid
/// math budgets vertical space the same way the legacy host did.
const CHROME_H_PX: f32 = 40.0 + 24.0;

impl Render for PaneGroup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().clone();
        let focus_handle = self.focus_handle_clone();

        dispatch_active_grid(self, window, cx);

        let active_content: Option<AnyElement> = self.active_tab().map(|tab| match &tab.content {
            PaneContent::Terminal(view) => view.clone().into_any_element(),
            PaneContent::Editor(view) => view.clone().into_any_element(),
        });

        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_hidden()
            .when_some(active_content, |s, child| s.child(child));

        div()
            .id(SharedString::from(format!(
                "pane-group-{}",
                entity.entity_id()
            )))
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .on_action(cx.listener(PaneGroup::on_new_tab))
            .on_action(cx.listener(PaneGroup::on_close_tab))
            .on_action(cx.listener(PaneGroup::on_next_tab))
            .on_action(cx.listener(PaneGroup::on_prev_tab))
            .on_action(cx.listener(PaneGroup::on_new_agent))
            .child(body)
    }
}

/// Build a tab strip element for a `PaneGroup` entity. Free function
/// so `ProjectPanes` can render the strip externally — either hoisted
/// into the top bar (for the topmost group) or wrapped above the
/// group's body (for non-topmost groups).
pub fn build_tab_strip_for(entity: Entity<PaneGroup>, theme: Theme, cx: &App) -> AnyElement {
    let group = entity.read(cx);
    let tabs: Vec<PaneGroupTabHeader> = group
        .tabs()
        .iter()
        .map(|t| PaneGroupTabHeader {
            label: t.label.clone(),
            kind_marker: kind_marker(&t.kind),
            agent_status: agent_status_for(&t.kind),
        })
        .collect();
    let active = group.active();
    let _ = group;
    build_tab_strip_from_headers(entity, &tabs, active, theme)
}

/// Lightweight projection of a `PaneGroupTab` that the strip render
/// needs. Avoids holding a borrow of the group while we build elements
/// (the strip itself uses `entity` for click handlers).
struct PaneGroupTabHeader {
    label: SharedString,
    kind_marker: PaneTabKindMarker,
    agent_status: Option<oximux_core::AgentStatus>,
}

#[derive(Clone, Copy)]
enum PaneTabKindMarker {
    Terminal,
    Agent,
    Editor,
}

fn kind_marker(kind: &PaneGroupTabKind) -> PaneTabKindMarker {
    match kind {
        PaneGroupTabKind::Terminal => PaneTabKindMarker::Terminal,
        PaneGroupTabKind::Agent { .. } => PaneTabKindMarker::Agent,
        PaneGroupTabKind::Editor { .. } => PaneTabKindMarker::Editor,
    }
}

fn agent_status_for(kind: &PaneGroupTabKind) -> Option<oximux_core::AgentStatus> {
    if let PaneGroupTabKind::Agent { status_rx, .. } = kind {
        Some(status_rx.borrow().clone())
    } else {
        None
    }
}

fn build_tab_strip_from_headers(
    entity: Entity<PaneGroup>,
    tabs: &[PaneGroupTabHeader],
    active: usize,
    theme: Theme,
) -> AnyElement {
    let entity_id = entity.entity_id();
    let mut strip = div()
        .id(SharedString::from(format!("pane-group-tab-strip-{entity_id}")))
        .flex()
        .flex_row()
        .items_stretch()
        .h(px(TAB_STRIP_HEIGHT_PX))
        .w_full()
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
        .overflow_x_scroll()
        .overflow_y_hidden();
    for (idx, header) in tabs.iter().enumerate() {
        strip = strip.child(render_tab_chip(
            entity_id.as_u64(),
            idx,
            header.label.clone(),
            header.kind_marker,
            header.agent_status.as_ref(),
            idx == active,
            theme,
            entity.clone(),
        ));
    }
    strip = strip.child(plus_button(entity_id.as_u64(), theme));
    strip = strip.child(div().flex_1().min_w(px(0.0)));
    strip.into_any_element()
}

/// Forward grid target to the active terminal tab so its PTY resizes
/// when the window resizes or chrome width changes. No-op for editor /
/// empty tabs. Mirrors the old `MainPane::dispatch_grids` budget math.
fn dispatch_active_grid(
    group: &PaneGroup,
    window: &Window,
    cx: &mut Context<PaneGroup>,
) {
    let Some(tab) = group.active_tab() else {
        return;
    };
    let PaneContent::Terminal(view) = &tab.content else {
        return;
    };
    let metrics = CellMetrics::measure(&group.typography, window);
    let v = window.viewport_size();
    let pad = group.density.pad_panel;
    let w =
        (f32::from(v.width) - group.chrome_w_px() - pad * 2.0).max(metrics.cell_width);
    let h = (f32::from(v.height) - CHROME_H_PX - TAB_STRIP_HEIGHT_PX - pad * 2.0)
        .max(metrics.line_height);
    let cols = metrics.cols_in(w);
    let rows = metrics.rows_in(h);
    let view = view.clone();
    view.update(cx, |v, _| v.set_target_grid(cols, rows));
}

#[allow(clippy::too_many_arguments)]
fn render_tab_chip(
    entity_id_raw: u64,
    ix: usize,
    label: SharedString,
    marker: PaneTabKindMarker,
    agent_status: Option<&oximux_core::AgentStatus>,
    is_active: bool,
    theme: Theme,
    entity: Entity<PaneGroup>,
) -> impl IntoElement {
    let icon_path = match marker {
        PaneTabKindMarker::Editor => "icons/file.svg",
        PaneTabKindMarker::Terminal | PaneTabKindMarker::Agent => "icons/square-terminal.svg",
    };
    let icon_color = if is_active {
        theme.fg_muted
    } else {
        theme.fg_subtle
    };
    let text_color = if is_active {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    let top_accent = if is_active {
        theme.focus_ring
    } else {
        gpui::transparent_black()
    };

    let group_name = SharedString::from(format!("pane-group-tab-{entity_id_raw}-{ix}"));
    let activate_entity = entity.clone();

    let agent_dot: Option<AnyElement> = agent_status.map(|status| {
        let dot_id =
            SharedString::from(format!("pane-group-agent-status-dot-{entity_id_raw}-{ix}"));
        render_dot(dot_id, status, theme).into_any_element()
    });

    div()
        .id(SharedString::from(format!(
            "pane-group-tab-{entity_id_raw}-{ix}"
        )))
        .group(group_name.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h_full()
        .px(px(TAB_PAD_X_PX))
        .border_t_2()
        .border_color(top_accent)
        .text_size(px(11.0))
        .text_color(text_color)
        .flex_shrink_0()
        .cursor_pointer()
        .when(!is_active, |s| {
            s.hover(|s| s.text_color(theme.fg_base).bg(theme.bg_panel_alt))
        })
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = activate_entity.clone();
            entity.update(cx, |this, cx| this.set_active(ix, window, cx));
        })
        .child(svg().path(icon_path).size(px(ICON_SIZE_PX)).text_color(icon_color))
        .when_some(agent_dot, |s, dot| s.child(dot))
        .child(div().child(label))
        .child(close_button(entity_id_raw, ix, is_active, entity, group_name, theme))
}

fn plus_button(entity_id_raw: u64, theme: Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/plus.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id(SharedString::from(format!("pane-group-plus-{entity_id_raw}")))
        .w(px(PLUS_BUTTON_WIDTH_PX))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(RequestOpenAdapterPicker), cx);
        })
        .child(glyph)
}

fn close_button(
    entity_id_raw: u64,
    ix: usize,
    is_active: bool,
    entity: Entity<PaneGroup>,
    group_name: SharedString,
    theme: Theme,
) -> impl IntoElement {
    let glyph = svg()
        .path("icons/close.svg")
        .size(px(9.0))
        .text_color(theme.fg_muted);
    let initial_opacity = if is_active { 1.0 } else { 0.0 };
    div()
        .id(SharedString::from(format!(
            "pane-group-tab-close-{entity_id_raw}-{ix}"
        )))
        .w(px(CLOSE_BUTTON_SIZE_PX))
        .h(px(CLOSE_BUTTON_SIZE_PX))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .opacity(initial_opacity)
        .group_hover(group_name, |s| s.opacity(1.0))
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let entity = entity.clone();
            entity.update(cx, |this, cx| this.close_tab(ix, window, cx));
            cx.stop_propagation();
        })
        .child(glyph)
}
