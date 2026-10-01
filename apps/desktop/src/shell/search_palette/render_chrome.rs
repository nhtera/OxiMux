//! Overlay chrome for the search palette: scrim, card, input row with the
//! Filter button, filter chips, the sectioned result list, empty states and
//! the footer key hints. Rows themselves live in `render_rows.rs`.

use gpui::{
    Animation, AnimationExt, AnyElement, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, ScrollHandle, Size, StatefulInteractiveElement, Styled, div, hsla,
    prelude::FluentBuilder, px, svg,
};
use gpui_component::input::{Input, InputState};
use oximux_settings::{Density, Motion, Theme, Typography};

use crate::shell::search_palette::filter_popover::{filter_chips, filter_popover};
use crate::shell::search_palette::recency::ms_to_rfc3339;
use crate::shell::search_palette::render_rows::{ActivateFn, RowCtx, render_entry};
use crate::shell::search_palette::sections::Entry;
use crate::shell::search_palette::state::PaletteState;
use crate::shell::search_palette::view::SearchPalette;
use crate::ui::FloatingSurface;

const CARD_MAX_W: f32 = 900.0;
const INPUT_ROW_H: f32 = 48.0;
const FOOTER_H: f32 = 32.0;
const SCRIM_ALPHA: f32 = 0.20;

pub struct ChromeInput<'a> {
    pub state: &'a PaletteState,
    pub query_input: Option<&'a Entity<InputState>>,
    pub filter_open: bool,
    pub filter_cursor: usize,
    pub scroll: &'a ScrollHandle,
    pub entity: Entity<SearchPalette>,
    pub on_activate: ActivateFn,
    pub viewport: Size<gpui::Pixels>,
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
    pub motion: Motion,
}

pub fn build_overlay(input: ChromeInput<'_>) -> gpui::Div {
    let theme = input.theme;
    let vw = f32::from(input.viewport.width);
    let vh = f32::from(input.viewport.height);
    let card_w = CARD_MAX_W.min(vw * 0.96);
    let top = (vh * 0.10).min(64.0);
    let list_max_h = (vh - top - INPUT_ROW_H - FOOTER_H - 48.0).clamp(120.0, 560.0);
    let filter_active = !input.state.filter.project_ids.is_empty();

    let mut card = div()
        .relative()
        .flex()
        .flex_col()
        .w(px(card_w))
        .floating_chrome(&theme, &input.density)
        .shadow_lg()
        // Presses inside the card must not reach the scrim's dismiss handler.
        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
        .child(input_row(&input))
        .when(filter_active, |c| c.child(filter_chips(&input)))
        .child(divider(theme))
        .child(result_list(&input, list_max_h))
        .child(divider(theme))
        .child(footer(theme, input.density, input.typography));
    if input.filter_open {
        card = card.child(filter_popover(&input));
    }

    let dismiss = input.entity.clone();
    div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(top))
        .bg(hsla(0.0, 0.0, 0.0, SCRIM_ALPHA))
        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
            dismiss.update(cx, |p, cx| p.close(cx));
        })
        // Same enter beat as the Command Palette: fade + 6px settle, replayed
        // on every open because the overlay unmounts while closed.
        .child(card.with_animation(
            "search-palette-enter",
            Animation::new(input.motion.m_overlay).with_easing(oximux_settings::ease_out_spring()),
            |el, delta| el.opacity(delta).mt(px(6.0 * (1.0 - delta))),
        ))
}

fn input_row(input: &ChromeInput<'_>) -> impl IntoElement {
    let theme = input.theme;
    let field: AnyElement = match input.query_input {
        Some(state) => Input::new(state)
            .appearance(false)
            .text_size(px(input.typography.t_body_md))
            .into_any_element(),
        None => div().into_any_element(),
    };
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .h(px(INPUT_ROW_H))
        .child(svg().path("icons/search.svg").size(px(16.)).text_color(theme.fg_subtle))
        .child(div().flex_1().min_w_0().child(field));
    if input.state.snapshot.project_count() > 1 {
        row = row.child(filter_button(input));
    }
    row
}

fn filter_button(input: &ChromeInput<'_>) -> impl IntoElement {
    let theme = input.theme;
    let count = input.state.filter.project_ids.len();
    let entity = input.entity.clone();
    div()
        .id("search-palette-filter")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .flex_shrink_0()
        .px(px(8.))
        .py(px(3.))
        .rounded(px(input.density.r_xs))
        .border_1()
        .border_color(if input.filter_open { theme.border_active } else { theme.border_inactive })
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .text_size(px(input.typography.t_sub_label))
        .text_color(theme.fg_muted)
        .on_mouse_down(MouseButton::Left, move |_e, window, cx| {
            cx.stop_propagation();
            entity.update(cx, |p, cx| p.toggle_filter_popover(window, cx));
        })
        .child("Filter")
        .when(count > 0, |d| {
            d.child(
                div()
                    .px(px(5.))
                    .rounded(px(input.density.r_chip))
                    .bg(theme.bg_overlay)
                    .text_color(theme.fg_base)
                    .child(count.to_string()),
            )
        })
}

fn result_list(input: &ChromeInput<'_>, max_h: f32) -> impl IntoElement {
    let state = input.state;
    let ctx = RowCtx {
        snapshot: &state.snapshot,
        now: ms_to_rfc3339(state.snapshot.now_ms),
        theme: input.theme,
        density: input.density,
        typography: input.typography,
        on_activate: input.on_activate.clone(),
    };
    // One child per entry so `ScrollHandle::scroll_to_item(entry index)`
    // keeps the selection in view.
    let mut col = div()
        .id("search-palette-results")
        .flex()
        .flex_col()
        .w_full()
        .px(px(input.density.pad_overlay))
        .py(px(4.))
        .max_h(px(max_h))
        .overflow_y_scroll()
        .track_scroll(input.scroll);
    for (i, entry) in state.entries.iter().enumerate() {
        col = col.child(render_entry(i, entry, state.selected == Some(i), &ctx));
    }
    let has_items = state.entries.iter().any(|e| matches!(e, Entry::Item { .. }));
    let empty = if has_items {
        None
    } else if !state.filter.project_ids.is_empty() {
        Some("Nothing in the selected projects")
    } else if !state.query().trim().is_empty() {
        Some("No matches")
    } else {
        Some("Nothing open yet")
    };
    div().w_full().when_some(empty, |d, msg| {
        d.child(
            div()
                .px(px(18.))
                .py(px(10.))
                .text_size(px(input.typography.t_body_sm))
                .text_color(input.theme.fg_subtle)
                .child(msg),
        )
    })
    .child(col)
}

fn footer(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    let hint = |key: &str, label: &str| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.))
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child(
                div()
                    .px(px(5.))
                    .rounded(px(density.r_xs))
                    .bg(theme.bg_panel_alt)
                    .text_color(theme.fg_muted)
                    .child(key.to_string()),
            )
            .child(label.to_string())
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(14.))
        .h(px(FOOTER_H))
        .px(px(14.))
        .child(hint("Enter", "Open"))
        .child(hint("Esc", "Close"))
        .child(hint("\u{2191}\u{2193}", "Move"))
        .child(hint("Tab", "Filter"))
}

fn divider(theme: Theme) -> impl IntoElement {
    div().w_full().h(px(1.)).flex_shrink_0().bg(theme.border_inactive)
}
