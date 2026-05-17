//! Pure render helpers for the palette modal — composition only, no entity
//! logic. The owning entity (`mod.rs::PaletteModal`) feeds resolved state.

use gpui::{IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::command_palette::entry::{CommandEntry, PaletteMode};

const MODAL_WIDTH: f32 = 600.0;
const MODAL_TOP_OFFSET_RATIO: f32 = 0.15;
const ROW_HEIGHT: f32 = 32.0;

pub struct ModalRenderInput<'a> {
    pub mode: PaletteMode,
    pub query: &'a str,
    pub selected_idx: usize,
    pub command_rows: Vec<&'a CommandEntry>,
    pub file_rows: Vec<&'a str>,
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
}

pub fn build_modal_layout(input: ModalRenderInput<'_>) -> impl IntoElement {
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
        .pt(px(MODAL_TOP_OFFSET_RATIO * 600.0))
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
                .text_size(px(typography.t_body_sm * 0.85))
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
            for (i, entry) in input.command_rows.iter().enumerate() {
                col = col.child(command_row(
                    entry,
                    i == input.selected_idx,
                    input.theme,
                    input.typography,
                ));
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

fn command_row(
    entry: &CommandEntry,
    selected: bool,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
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
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(12.))
        .bg(bg)
        .child(
            div()
                .flex_1()
                .text_size(px(typography.t_body_sm))
                .text_color(fg)
                .child(entry.name),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm * 0.9))
                .text_color(theme.fg_subtle)
                .child(entry.keybinding.unwrap_or("")),
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
