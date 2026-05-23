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
    AnyElement, Context, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, svg,
};

use super::{PaneGroup, PaneGroupTabKind};
use crate::actions::RequestOpenAdapterPicker;
use crate::shell::agent_status_badge::render_dot;
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::pane_content::PaneContent;

const TAB_STRIP_HEIGHT_PX: f32 = 32.0;
const TAB_PAD_X_PX: f32 = 12.0;
const CLOSE_BUTTON_SIZE_PX: f32 = 14.0;
const ICON_SIZE_PX: f32 = 11.0;
const PLUS_BUTTON_WIDTH_PX: f32 = 28.0;
/// Match the workspace chrome (top bar + status bar) so terminal grid
/// math budgets vertical space the same way the old MainPane did.
const CHROME_H_PX: f32 = 40.0 + 24.0;

impl Render for PaneGroup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active = self.active();
        let entity = cx.entity().clone();
        let focus_handle = self.focus_handle_clone();

        dispatch_active_grid(self, window, cx);

        let mut strip = div()
            .id("pane-group-tab-strip")
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

        for (idx, tab) in self.tabs().iter().enumerate() {
            strip = strip.child(render_tab(
                idx,
                tab.label.clone(),
                &tab.kind,
                idx == active,
                theme,
                entity.clone(),
            ));
        }
        // "+" new-tab/agent button immediately after the last tab, then a
        // flex spacer that pushes nothing (the strip's overflow-scroll
        // already absorbs slack). Matches the per-group "+" pattern in
        // the reference editor.
        strip = strip.child(plus_button(theme));
        strip = strip.child(div().flex_1().min_w(px(0.0)));

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
            .child(strip)
            .child(body)
    }
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

fn render_tab(
    ix: usize,
    label: SharedString,
    kind: &PaneGroupTabKind,
    is_active: bool,
    theme: oximux_settings::Theme,
    entity: Entity<PaneGroup>,
) -> impl IntoElement {
    let icon_path = match kind {
        PaneGroupTabKind::Editor { .. } => "icons/file.svg",
        PaneGroupTabKind::Terminal | PaneGroupTabKind::Agent { .. } => "icons/square-terminal.svg",
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

    let group_name = SharedString::from(format!("pane-group-tab-{ix}"));
    let activate_entity = entity.clone();

    let agent_dot: Option<AnyElement> = if let PaneGroupTabKind::Agent { status_rx, .. } = kind {
        let status = status_rx.borrow().clone();
        let dot_id = SharedString::from(format!("pane-group-agent-status-dot-{ix}"));
        Some(render_dot(dot_id, &status, theme).into_any_element())
    } else {
        None
    };

    div()
        .id(SharedString::from(format!("pane-group-tab-{ix}")))
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
        .child(close_button(ix, is_active, entity, group_name, theme))
}

fn plus_button(theme: oximux_settings::Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/plus.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id("pane-group-plus")
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
    ix: usize,
    is_active: bool,
    entity: Entity<PaneGroup>,
    group_name: SharedString,
    theme: oximux_settings::Theme,
) -> impl IntoElement {
    let glyph = svg()
        .path("icons/close.svg")
        .size(px(9.0))
        .text_color(theme.fg_muted);
    let initial_opacity = if is_active { 1.0 } else { 0.0 };
    div()
        .id(SharedString::from(format!("pane-group-tab-close-{ix}")))
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
