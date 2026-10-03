//! Project filter: the checklist popover under the Filter button and the chip
//! row shown beneath the input while a filter is active.

use gpui::{
    InteractiveElement, IntoElement, MouseButton, ParentElement, Styled,
    div, prelude::FluentBuilder, px, svg,
};

use crate::shell::search_palette::render_chrome::ChromeInput;

const POPOVER_W: f32 = 280.0;
const POPOVER_TOP: f32 = 44.0;
const ITEM_H: f32 = 30.0;

pub fn filter_popover(input: &ChromeInput<'_>) -> impl IntoElement {
    let theme = input.theme;
    let state = input.state;
    let mut list = div().flex().flex_col().py(px(4.));
    for (i, (id, name, count)) in state.project_counts().into_iter().enumerate() {
        let checked = state.filter.project_ids.contains(&id);
        let entity = input.entity.clone();
        list = list.child(
            div()
                .id(("search-palette-filter-row", i))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .h(px(ITEM_H))
                .px(px(10.))
                .rounded(px(input.density.r_xs))
                .cursor_pointer()
                .when(i == input.filter_cursor, |d| d.bg(theme.selection))
                .hover(|s| s.bg(theme.hover_overlay))
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                    cx.stop_propagation();
                    entity.update(cx, |p, cx| p.toggle_project(&id, cx));
                })
                .child(
                    div()
                        .size(px(14.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        // Half the smallest radius token: a 14px box.
                        .rounded(px(input.density.r_xs * 0.5))
                        .border_1()
                        .border_color(if checked { theme.border_active } else { theme.border_inactive })
                        .when(checked, |d| {
                            d.child(svg().path("icons/check.svg").size(px(10.)).text_color(theme.fg_base))
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(input.typography.t_body_sm))
                        .text_color(theme.fg_base)
                        .child(name),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(input.typography.t_sub_label))
                        .text_color(theme.fg_subtle)
                        .child(count.to_string()),
                ),
        );
    }
    div()
        .absolute()
        .top(px(POPOVER_TOP))
        .right(px(12.))
        .w(px(POPOVER_W))
        .px(px(4.))
        .rounded(px(input.density.r_card))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_overlay)
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
        .child(list)
}

pub fn filter_chips(input: &ChromeInput<'_>) -> impl IntoElement {
    let theme = input.theme;
    let state = input.state;
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px(6.))
        .px(px(14.))
        .pb(px(8.));
    for p in state.snapshot.projects.iter().filter(|p| state.filter.project_ids.contains(&p.project_id)) {
        let entity = input.entity.clone();
        let id = p.project_id.clone();
        row = row.child(
            div()
                .id(gpui::SharedString::from(format!("search-palette-chip-{id}")))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .px(px(6.))
                .py(px(1.))
                .rounded(px(input.density.r_chip))
                .bg(theme.bg_panel_alt)
                .text_size(px(input.typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(p.name.clone())
                .child(
                    div()
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                            cx.stop_propagation();
                            entity.update(cx, |pal, cx| pal.toggle_project(&id, cx));
                        })
                        .child(svg().path("icons/x.svg").size(px(10.)).text_color(theme.fg_subtle)),
                ),
        );
    }
    let entity = input.entity.clone();
    row.child(
        div()
            .id("search-palette-chip-clear")
            .cursor_pointer()
            .text_size(px(input.typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .hover(|s| s.text_color(theme.fg_base))
            .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                cx.stop_propagation();
                entity.update(cx, |p, cx| p.clear_filter(cx));
            })
            .child("Clear all"),
    )
}
