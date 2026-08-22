//! Agents-page status-filter dropdown — the popover that opens under the
//! header's status trigger and lets the user pick which status bucket the
//! dashboard is narrowed to (All / Needs attention / Working / Done / Idle /
//! Ended). The active bucket carries a trailing check, like the reference
//! cockpit's grouping dropdown.
//!
//! Mirrors the `project_menu` / `row_menu` contract: a full-window occluding
//! backdrop (closes on outside click) with the menu card pinned at the click
//! anchor. Closed state is `open_for: None`. The selection is applied to the
//! owning `LeftRail` through a `WeakEntity` so the rail's filter state stays the
//! single source of truth.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::agents_dashboard::filter::StatusFilter;
use crate::shell::left_rail::LeftRail;

/// Width of the menu card — wide enough for the longest label
/// ("Needs attention") plus the trailing check.
const MENU_WIDTH: f32 = 190.0;
/// One row height — matches the other rail popovers for visual parity.
const ROW_MENU_ITEM_H: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Y offset below the trigger so the menu doesn't overlap it.
const ANCHOR_Y_OFFSET: f32 = 4.0;

pub struct DashboardStatusFilterMenu {
    /// `None` when closed; `Some` carries the currently-active filter (for the
    /// check) plus the screen-pixel anchor for the popover.
    open_for: Option<(StatusFilter, f32, f32)>,
    /// The rail whose `dashboard_status_filter` this menu drives.
    rail: WeakEntity<LeftRail>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl DashboardStatusFilterMenu {
    /// `true` while the menu is pinned open.
    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        rail: WeakEntity<LeftRail>,
    ) -> Self {
        Self {
            open_for: None,
            rail,
            theme,
            density,
            typography,
        }
    }

    /// Open the menu anchored at (x, y), with `active` shown as checked.
    pub fn open(&mut self, active: StatusFilter, x: f32, y: f32, cx: &mut Context<Self>) {
        self.open_for = Some((active, x, y + ANCHOR_Y_OFFSET));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        cx.notify();
    }

    /// Apply `choice` to the rail and close.
    fn select(&mut self, choice: StatusFilter, cx: &mut Context<Self>) {
        let _ = self
            .rail
            .update(cx, |rail, cx| rail.set_dashboard_status_filter(choice, cx));
        self.close(cx);
    }
}

impl Render for DashboardStatusFilterMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let Some((active, x, y)) = self.open_for else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        for (ix, &choice) in StatusFilter::ALL.iter().enumerate() {
            let is_active = choice == active;
            let row = div()
                .id(("status-filter-item", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(density.gap_inline))
                .h(px(ROW_MENU_ITEM_H))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.hover_overlay))
                .text_size(px(typography.t_body_md))
                .text_color(theme.fg_base)
                .child(
                    div()
                        .flex_1()
                        .whitespace_nowrap()
                        .child(choice.label().to_string()),
                )
                .child(
                    // Trailing check on the active bucket; a fixed-width slot so
                    // the labels stay aligned whether checked or not.
                    div()
                        .w(px(12.0))
                        .flex_shrink_0()
                        .text_color(theme.fg_base)
                        .child(if is_active { "✓" } else { "" }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.select(choice, cx);
                    }),
                );
            card = card.child(row);
        }

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(MENU_WIDTH))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
            .into_any_element()
    }
}
