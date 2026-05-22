//! Pure UI helpers for the workspace tab strip. Extracted from `mod.rs`
//! to keep the parent file under the 800-LOC hard cap. Nothing here owns
//! mutable state — every function builds elements that read from the
//! `Entity<WorkspaceTabs>` passed in.

use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px, svg,
};
use oximux_settings::Theme;

use crate::actions::{RequestOpenAdapterPicker, SplitDown, SplitLeft, SplitRight, SplitUp};
use crate::shell::agent_status_badge::render_dot;

use super::{WorkspaceTabKind, WorkspaceTabs};

/// Build the tab-strip element for embedding in `top_bar`'s center zone.
pub fn render_tab_strip(entity: Entity<WorkspaceTabs>, cx: &mut App) -> AnyElement {
    let this = entity.read(cx);
    let theme = this.theme;
    let active = this.active;
    let tab_count = this.tabs.len();

    let mut strip = div()
        .id(SharedString::from(format!(
            "oximux-workspace-tab-strip-{}",
            entity.entity_id()
        )))
        .flex()
        .flex_row()
        .items_stretch()
        .h_full()
        .min_w(px(0.0))
        .overflow_x_scroll()
        .overflow_y_hidden();

    for (ix, tab) in this.tabs.iter().enumerate() {
        strip = strip.child(workspace_tab(
            ix,
            tab.label.clone(),
            ix == active,
            ix < tab_count - 1,
            &tab.kind,
            theme,
            entity.clone(),
        ));
    }

    let mut row = div()
        .flex()
        .flex_row()
        .items_stretch()
        .h_full()
        .min_w(px(0.0))
        .flex_1()
        .child(strip)
        .child(plus_button(theme, entity.clone()))
        .child(div().flex_1().min_w(px(0.0)).h_full());
    if tab_count > 0 {
        row = row.child(pane_actions_button(theme));
    }
    row.into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn workspace_tab(
    ix: usize,
    label: SharedString,
    is_active: bool,
    has_neighbor_right: bool,
    kind: &WorkspaceTabKind,
    theme: Theme,
    entity: Entity<WorkspaceTabs>,
) -> impl IntoElement {
    let group_name = SharedString::from(format!("ws-tab-{ix}"));
    let icon_path = match kind {
        WorkspaceTabKind::Editor { .. } => "icons/file.svg",
        WorkspaceTabKind::Terminal | WorkspaceTabKind::Agent { .. } => "icons/square-terminal.svg",
    };
    let icon = svg().path(icon_path).size(px(11.0)).text_color(if is_active {
        theme.fg_muted
    } else {
        theme.fg_subtle
    });
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
    let separator = if has_neighbor_right {
        theme.border_inactive
    } else {
        gpui::transparent_black()
    };
    let close_btn = close_button(theme, ix, is_active, entity.clone(), group_name.clone());
    let entity_for_click = entity.clone();
    let agent_dot: Option<AnyElement> = if let WorkspaceTabKind::Agent { status_rx, .. } = kind {
        let status = status_rx.borrow().clone();
        let dot_id = SharedString::from(format!("agent-status-dot-{ix}"));
        Some(render_dot(dot_id, &status, theme).into_any_element())
    } else {
        None
    };

    div()
        .id(SharedString::from(format!("ws-tab-{ix}")))
        .group(group_name)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h_full()
        .px(px(8.0))
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
            let entity = entity_for_click.clone();
            entity.update(cx, |this, cx| this.set_active(ix, window, cx));
        })
        .child(icon)
        .when_some(agent_dot, |this, dot| this.child(dot))
        .child(
            div()
                .min_w(px(0.0))
                .max_w(px(110.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(label),
        )
        .child(close_btn)
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(1.0))
                .bg(separator),
        )
}

fn pane_actions_button(theme: Theme) -> impl IntoElement {
    let glyph = svg()
        .path("icons/ellipsis.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id("ws-tab-pane-actions")
        .w(px(28.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            window.dispatch_action(Box::new(crate::actions::OpenPaneActions), cx);
        })
        .child(glyph)
}

pub fn split_icon(action: SplitDirection) -> &'static str {
    match action {
        SplitDirection::Right => "icons/arrow-right.svg",
        SplitDirection::Down => "icons/arrow-down.svg",
        SplitDirection::Left => "icons/arrow-left.svg",
        SplitDirection::Up => "icons/arrow-up.svg",
    }
}

#[derive(Clone, Copy)]
pub enum SplitDirection {
    Right,
    Down,
    Left,
    Up,
}

impl SplitDirection {
    pub fn label(self) -> &'static str {
        match self {
            SplitDirection::Right => "Split Right",
            SplitDirection::Down => "Split Down",
            SplitDirection::Left => "Split Left",
            SplitDirection::Up => "Split Up",
        }
    }

    pub fn dispatch(self, window: &mut Window, cx: &mut App) {
        match self {
            SplitDirection::Right => window.dispatch_action(Box::new(SplitRight), cx),
            SplitDirection::Down => window.dispatch_action(Box::new(SplitDown), cx),
            SplitDirection::Left => window.dispatch_action(Box::new(SplitLeft), cx),
            SplitDirection::Up => window.dispatch_action(Box::new(SplitUp), cx),
        }
    }
}

fn close_button(
    theme: Theme,
    ix: usize,
    is_active: bool,
    entity: Entity<WorkspaceTabs>,
    group_name: SharedString,
) -> impl IntoElement {
    let glyph = svg()
        .path("icons/close.svg")
        .size(px(9.0))
        .text_color(theme.fg_muted);
    let initial_opacity = if is_active { 1.0 } else { 0.0 };
    div()
        .id(SharedString::from(format!("ws-tab-close-{ix}")))
        .w(px(14.0))
        .h(px(14.0))
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

/// Plus-button width in CSS px. Subtracted from the click x so the popover's
/// left edge sits roughly at the button's left edge.
pub(super) const PLUS_BUTTON_WIDTH_PX: f32 = 28.0;

fn plus_button(theme: Theme, entity: Entity<WorkspaceTabs>) -> impl IntoElement {
    let glyph = svg()
        .path("icons/plus.svg")
        .size(px(14.0))
        .text_color(theme.fg_muted);
    div()
        .id("ws-tab-plus")
        .w(px(PLUS_BUTTON_WIDTH_PX))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, cx| {
            let anchor_x = (f32::from(e.position.x) - PLUS_BUTTON_WIDTH_PX / 2.0).max(0.0);
            entity.read(cx).last_plus_click_x.set(Some(anchor_x));
            window.dispatch_action(Box::new(RequestOpenAdapterPicker), cx);
            cx.stop_propagation();
        })
        .child(glyph)
}
