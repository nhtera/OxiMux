//! Pane Actions dropdown — split direction picker.
//!
//! A small popup anchored near the workspace's trailing "..." button.
//! Each item dispatches a Split* action up the focus chain so the
//! focused pane group performs the split.
//!
//! Positioning: takes a `right_anchor_px` at open time (so the dropdown
//! tracks the current right-sidebar state) and pins to the top-right of
//! the window with that offset. A full-window invisible overlay sits
//! beneath the card to catch click-outside dismiss.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, Window, div, px, svg,
};
use oximux_settings::{Density, Theme, Typography};

use crate::ui::FloatingSurface;

use crate::actions::CloseGroup;
use crate::shell::split_direction::{SplitDirection, split_icon};

/// Width of the dropdown card.
pub const MENU_WIDTH: f32 = 184.0;
/// Vertical gap below the chrome row before the dropdown starts when
/// anchored to the workspace's top-right "..." button.
const TOP_BAR_ANCHOR_TOP_PX: f32 = 42.0;
/// Icon size inside each menu item.
const ICON_SIZE: f32 = 14.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Gap between icon and label.
const ROW_GAP: f32 = 10.0;

/// Where the dropdown anchors. `TopRight` is the workspace's trailing
/// "..." button — fixed top offset, right-edge inset. `Chip` is a
/// per-pane "..." click — the menu opens at the cursor's absolute
/// window position, shifted left so it doesn't fall off the right edge.
#[derive(Clone, Copy)]
pub enum PaneActionsAnchor {
    TopRight { right_px: f32 },
    Chip { x_px: f32, y_px: f32 },
}

pub struct PaneActionsMenu {
    open: bool,
    anchor: PaneActionsAnchor,
    /// Show the Close Group row. Owner passes `in_order_groups().len() > 1`
    /// so the action only appears when another group would survive.
    has_siblings: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl PaneActionsMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            anchor: PaneActionsAnchor::TopRight { right_px: 0.0 },
            has_siblings: false,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, anchor: PaneActionsAnchor, has_siblings: bool, cx: &mut Context<Self>) {
        self.anchor = anchor;
        self.has_siblings = has_siblings;
        self.open = true;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }
}

impl Render for PaneActionsMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let anchor = self.anchor;

        let dirs = [
            SplitDirection::Right,
            SplitDirection::Down,
            SplitDirection::Left,
            SplitDirection::Up,
        ];

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .floating_chrome(&theme, &density)
            .shadow_lg();
        for (ix, dir) in dirs.iter().copied().enumerate() {
            let icon = svg()
                .path(split_icon(dir))
                .size(px(ICON_SIZE))
                .text_color(theme.fg_muted);
            let row = div()
                .id(("pane-action-row", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(ROW_GAP))
                .h(px(density.h_overlay_item))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_base)
                .child(icon)
                .child(div().flex_1().child(dir.label()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        dir.dispatch(window, cx);
                        this.close(cx);
                    }),
                );
            card = card.child(row);
        }

        if self.has_siblings {
            let separator = div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive);
            // Destructive row — icon + label painted with `status_error` so
            // the user reads "this is the dangerous one" at a glance. Hover
            // bg stays `bg_panel_alt` (consistent feedback); we don't tint
            // the hover red because that conflicts with the focus cursor.
            let close_icon = svg()
                .path("icons/close.svg")
                .size(px(ICON_SIZE))
                .text_color(theme.status_error);
            let close_row = div()
                .id("pane-action-row-close-group")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(ROW_GAP))
                .h(px(density.h_overlay_item))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_panel_alt))
                .text_size(px(typography.t_body_md))
                .text_color(theme.status_error)
                .child(close_icon)
                .child(div().flex_1().child("Close Group"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        window.dispatch_action(Box::new(CloseGroup), cx);
                        this.close(cx);
                    }),
                );
            card = card.child(separator).child(close_row);
        }

        // Anchored card: shared between the top-bar "..." (right-edge
        // pin) and the per-pane "..." chip (cursor-pin). For the chip
        // path, shift left by MENU_WIDTH so the card stays inside the
        // window edge when the click lands near the right side.
        let card_container = match anchor {
            PaneActionsAnchor::TopRight { right_px } => div()
                .absolute()
                .top(px(TOP_BAR_ANCHOR_TOP_PX))
                .right(px(right_px))
                .w(px(MENU_WIDTH))
                .child(card),
            PaneActionsAnchor::Chip { x_px, y_px } => {
                let left_px = (x_px - MENU_WIDTH).max(0.0);
                div()
                    .absolute()
                    .top(px(y_px))
                    .left(px(left_px))
                    .w(px(MENU_WIDTH))
                    .child(card)
            }
        };

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
            .child(card_container)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_menu() -> PaneActionsMenu {
        PaneActionsMenu::new(Theme::charcoal(), Density::cockpit(), Typography::cockpit())
    }

    #[test]
    fn new_menu_is_closed() {
        let m = test_menu();
        assert!(!m.is_open());
    }

    #[test]
    fn open_sets_anchor_and_visibility() {
        let mut m = test_menu();
        // Mirror open() body inline (no Context<Self> in unit tests).
        m.anchor = PaneActionsAnchor::TopRight { right_px: 120.0 };
        m.has_siblings = true;
        m.open = true;
        assert!(m.is_open());
        assert!(matches!(
            m.anchor,
            PaneActionsAnchor::TopRight { right_px } if right_px == 120.0
        ));
        assert!(m.has_siblings);
    }

    #[test]
    fn chip_anchor_carries_xy() {
        let mut m = test_menu();
        m.anchor = PaneActionsAnchor::Chip {
            x_px: 800.0,
            y_px: 200.0,
        };
        m.open = true;
        assert!(matches!(
            m.anchor,
            PaneActionsAnchor::Chip { x_px, y_px } if x_px == 800.0 && y_px == 200.0
        ));
    }
}
