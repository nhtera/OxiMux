//! Pane Actions dropdown — split direction picker.
//!
//! A small popup anchored near the workspace tab strip's trailing "..."
//! button. Each item dispatches a Split* action up the focus chain so the
//! focused `MainPane` performs the split.
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

use crate::shell::workspace_tabs::{SplitDirection, split_icon};

/// Width of the dropdown card.
const MENU_WIDTH: f32 = 184.0;
/// Vertical gap below the chrome row before the dropdown starts.
const ANCHOR_TOP_PX: f32 = 42.0;
/// Edge padding around the card content.
const CARD_PADDING: f32 = 6.0;
/// Single menu item height.
const ITEM_HEIGHT: f32 = 30.0;
/// Icon size inside each menu item.
const ICON_SIZE: f32 = 14.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Gap between icon and label.
const ROW_GAP: f32 = 10.0;

pub struct PaneActionsMenu {
    open: bool,
    /// Right-edge offset in CSS pixels — set by WorkspaceRoot at open()
    /// so the dropdown sits beneath the "..." button regardless of
    /// whether the right sidebar is open.
    right_anchor_px: f32,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl PaneActionsMenu {
    pub fn new(theme: Theme, density: Density, typography: Typography) -> Self {
        Self {
            open: false,
            right_anchor_px: 0.0,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, right_anchor_px: f32, cx: &mut Context<Self>) {
        self.right_anchor_px = right_anchor_px;
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
        let right_px = self.right_anchor_px;

        let dirs = [
            SplitDirection::Right,
            SplitDirection::Down,
            SplitDirection::Left,
            SplitDirection::Up,
        ];

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(CARD_PADDING))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
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
                .h(px(ITEM_HEIGHT))
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
            .child(
                div()
                    .absolute()
                    .top(px(ANCHOR_TOP_PX))
                    .right(px(right_px))
                    .w(px(MENU_WIDTH))
                    .child(card),
            )
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
        m.right_anchor_px = 120.0;
        m.open = true;
        assert!(m.is_open());
        assert_eq!(m.right_anchor_px, 120.0);
    }
}
