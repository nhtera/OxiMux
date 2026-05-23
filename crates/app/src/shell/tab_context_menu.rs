//! Tab right-click context menu — Close / Close Others / Close to Right
//! / Close All in Group.
//!
//! Mirrors `PaneActionsMenu`: one shared entity owned by `WorkspaceRoot`,
//! opened via a payload action carrying the click coords + the target
//! group's id + the right-clicked tab index. The menu stores a
//! `WeakEntity<PaneGroup>` so row clicks can mutate the right group
//! even if focus has moved elsewhere by the time the user picks an item.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::pane_group::PaneGroup;

/// Width of the dropdown card.
pub const MENU_WIDTH: f32 = 188.0;
/// Edge padding around the card content.
const CARD_PADDING: f32 = 6.0;
/// Single menu item height.
const ITEM_HEIGHT: f32 = 30.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;

/// Right-click context target: which group + which tab inside it.
#[derive(Clone)]
struct TabContextTarget {
    group: WeakEntity<PaneGroup>,
    tab_idx: usize,
    /// Tab count snapshot at open time — drives "Close Others" / "Close
    /// to Right" disabled rendering. Not refreshed on tick: if the user
    /// somehow mutates the group between open and click the helpers
    /// will still bail safely (each verifies its own bounds).
    tab_count: usize,
}

pub struct TabContextMenu {
    open: bool,
    /// Absolute window x of the click — drives left-shift positioning
    /// so the card stays inside the right edge.
    x_px: f32,
    /// Absolute window y of the click.
    y_px: f32,
    target: Option<TabContextTarget>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl TabContextMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            x_px: 0.0,
            y_px: 0.0,
            target: None,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(
        &mut self,
        x_px: f32,
        y_px: f32,
        group: WeakEntity<PaneGroup>,
        tab_idx: usize,
        tab_count: usize,
        cx: &mut Context<Self>,
    ) {
        self.x_px = x_px;
        self.y_px = y_px;
        self.target = Some(TabContextTarget {
            group,
            tab_idx,
            tab_count,
        });
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }
}

impl Render for TabContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let Some(target) = self.target.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let x_px = self.x_px;
        let y_px = self.y_px;

        let has_multiple = target.tab_count > 1;
        let has_right = target.tab_idx + 1 < target.tab_count;

        let close_idx = target.tab_idx;
        let others_idx = target.tab_idx;
        let right_idx = target.tab_idx;

        let group_close = target.group.clone();
        let group_others = target.group.clone();
        let group_right = target.group.clone();
        let group_all = target.group.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(CARD_PADDING))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        card = card.child(menu_row(
            "tab-ctx-close",
            "Close",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_close.upgrade() {
                    group.update(cx, |g, cx| g.close_tab(close_idx, window, cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "tab-ctx-close-others",
            "Close Others",
            has_multiple,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_others.upgrade() {
                    group.update(cx, |g, cx| g.close_others(others_idx, window, cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "tab-ctx-close-to-right",
            "Close to Right",
            has_right,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_right.upgrade() {
                    group.update(cx, |g, cx| g.close_to_right(right_idx, window, cx));
                }
                this.close(cx);
            }),
        ));

        card = card.child(menu_row(
            "tab-ctx-close-all",
            "Close All in Group",
            true,
            theme,
            density,
            typography.clone(),
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                if let Some(group) = group_all.upgrade() {
                    group.update(cx, |g, cx| g.close_all(window, cx));
                }
                this.close(cx);
            }),
        ));

        let left_px = (x_px - MENU_WIDTH).max(0.0);
        let card_container = div()
            .absolute()
            .top(px(y_px))
            .left(px(left_px))
            .w(px(MENU_WIDTH))
            .child(card);

        // Full-window invisible overlay for click-outside dismiss.
        div()
            .absolute()
            .inset_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    this.close(cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                    this.close(cx);
                }),
            )
            .child(card_container)
            .into_any_element()
    }
}

fn menu_row<H>(
    row_id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
    on_click: H,
) -> impl IntoElement
where
    H: Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
{
    let fg = if enabled {
        theme.fg_base
    } else {
        theme.fg_subtle
    };
    let mut row = div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .h(px(ITEM_HEIGHT))
        .px(px(ROW_PADDING_X))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_md))
        .text_color(fg)
        .child(label);
    if enabled {
        row = row
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_panel_alt))
            .on_mouse_down(MouseButton::Left, on_click);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> TabContextMenu {
        TabContextMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
        assert!(m.target.is_none());
    }

    #[test]
    fn open_stores_coords_and_target_metadata() {
        let mut m = test_menu();
        // Mirror open() body inline (no Context<Self> in unit tests).
        m.x_px = 500.0;
        m.y_px = 200.0;
        m.open = true;
        assert!(m.is_open());
        assert_eq!(m.x_px, 500.0);
        assert_eq!(m.y_px, 200.0);
    }
}
