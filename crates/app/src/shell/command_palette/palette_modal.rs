//! Pure render helpers for the palette modal — composition only, no entity
//! logic. The owning entity (`mod.rs::PaletteModal`) feeds resolved state.
//!
//! Rows are clickable: `on_click_idx` is a closure supplied by the caller
//! that activates the row at the given index in the filtered list.

use gpui::{InteractiveElement, IntoElement, MouseButton, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::command_palette::entry::{PaletteGroup, PaletteItem, PaletteMode};

const MODAL_WIDTH: f32 = 600.0;
const MODAL_TOP_OFFSET_PX: f32 = 90.0;
const ROW_HEIGHT: f32 = 32.0;
const GROUP_LABEL_HEIGHT: f32 = 22.0;

pub struct ModalRenderInput<'a> {
    pub mode: PaletteMode,
    pub query: &'a str,
    pub selected_idx: usize,
    /// Resolved palette items (Commands mode). Empty for QuickOpen mode.
    pub palette_items: &'a [PaletteItem],
    /// Quick-open file paths (QuickOpen mode). Empty for Commands mode.
    pub file_rows: Vec<&'a str>,
    /// Activates the row at the given filtered-list index — dispatches the
    /// action AND closes the modal (the entity's `activate_item`). Shared by
    /// click and keyboard so a click can't leave the palette open.
    pub on_activate: std::rc::Rc<dyn Fn(usize, &mut gpui::Window, &mut gpui::App)>,
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
}

/// Build the full modal layout div. The caller chains `.track_focus` and
/// `.on_key_down` on the returned element.
pub fn build_modal_layout(input: ModalRenderInput<'_>) -> gpui::Div {
    let card = card_container(input.theme, input.density)
        .child(header_row(
            input.mode,
            input.query,
            input.theme,
            input.typography,
        ))
        .child(divider(input.theme))
        .child(result_list(&input));

    div()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(MODAL_TOP_OFFSET_PX))
        .child(card)
}

fn card_container(theme: Theme, density: Density) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .w(px(MODAL_WIDTH))
        .max_h(px(420.))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_active)
        .rounded(px(density.r_card))
}

fn header_row(
    mode: PaletteMode,
    query: &str,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    let mode_label = match mode {
        PaletteMode::QuickOpen => "Files",
        PaletteMode::Commands => "Commands",
    };
    let placeholder = if query.is_empty() {
        match mode {
            PaletteMode::QuickOpen => "Search files…",
            PaletteMode::Commands => "Search commands…",
        }
    } else {
        query
    };
    let placeholder_color = if query.is_empty() {
        theme.fg_subtle
    } else {
        theme.fg_base
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .h(px(36.))
        .child(
            div()
                .px(px(6.))
                .py(px(2.))
                .bg(theme.bg_panel_alt)
                .rounded(px(4.))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(mode_label),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_md))
                .text_color(placeholder_color)
                .child(placeholder.to_string()),
        )
}

fn divider(theme: Theme) -> impl IntoElement {
    div().w_full().h(px(1.)).bg(theme.border_inactive)
}

fn result_list(input: &ModalRenderInput<'_>) -> gpui::AnyElement {
    let mut col = div().flex().flex_col().w_full();
    match input.mode {
        PaletteMode::Commands => {
            let mut last_group: Option<PaletteGroup> = None;
            for (i, item) in input.palette_items.iter().enumerate() {
                // Insert group separator label when the group changes.
                if last_group != Some(item.display_group) {
                    if item.display_group == PaletteGroup::Custom {
                        col = col.child(group_label("Custom", input.theme, input.typography));
                    }
                    last_group = Some(item.display_group);
                }
                col = col.child(palette_row(item, i == input.selected_idx, i, input));
            }
        }
        PaletteMode::QuickOpen => {
            for (i, path) in input.file_rows.iter().enumerate() {
                col = col.child(file_row(
                    path,
                    i == input.selected_idx,
                    input.theme,
                    input.typography,
                ));
            }
        }
    }
    col.into_any_element()
}

/// A thin label row that separates custom commands from built-ins.
fn group_label(label: &str, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(GROUP_LABEL_HEIGHT))
        .px(px(12.))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_subtle)
        .child(label.to_string())
}

/// A single palette row. Click activates through the entity's `activate_item`
/// (via the shared `on_activate` callback) so the row dispatches AND closes the
/// modal — identical to the keyboard Enter path, no duplicated dispatch logic.
fn palette_row(
    item: &PaletteItem,
    selected: bool,
    row_idx: usize,
    input: &ModalRenderInput<'_>,
) -> impl IntoElement {
    let bg = if selected {
        input.theme.bg_panel_alt
    } else {
        input.theme.bg_overlay
    };
    let fg = if selected {
        input.theme.fg_base
    } else {
        input.theme.fg_muted
    };

    let kb = item.keybinding.unwrap_or("");
    let activate = input.on_activate.clone();

    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(12.))
        .bg(bg)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
            activate(row_idx, window, cx);
        })
        .child(
            div()
                .flex_1()
                .text_size(px(input.typography.t_body_sm))
                .text_color(fg)
                .child(item.name.clone()),
        )
        .child(
            div()
                .text_size(px(input.typography.t_body_sm * 0.9))
                .text_color(input.theme.fg_subtle)
                .child(kb.to_string()),
        )
}

fn file_row(path: &str, selected: bool, theme: Theme, typography: &Typography) -> impl IntoElement {
    let bg = if selected {
        theme.bg_panel_alt
    } else {
        theme.bg_overlay
    };
    let fg = if selected {
        theme.fg_base
    } else {
        theme.fg_muted
    };
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(12.))
        .bg(bg)
        .text_size(px(typography.t_body_sm))
        .text_color(fg)
        .child(path.to_string())
}
