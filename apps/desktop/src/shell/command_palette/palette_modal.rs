//! Layout composition for the palette modal — backdrop, card, header,
//! scrollable result list, and footer. Pure: the owning entity
//! (`mod.rs::PaletteModal`) feeds resolved state and the activate/dismiss
//! callbacks. Per-row rendering lives in `row_render.rs`.

use std::rc::Rc;

use gpui::{
    Animation, AnimationExt, App, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, hsla, prelude::FluentBuilder,
    px,
};
use gpui_component::{Icon, IconName};
use oximux_settings::{Density, Motion, Theme, Typography};

use crate::ui::FloatingSurface;

use crate::shell::command_palette::entry::{PaletteGroup, PaletteItem, PaletteMode};
use crate::shell::command_palette::row_render::{
    ActivateFn, ROW_HEIGHT, file_row, group_label, palette_row,
};

/// Backdrop dismiss callback — closes the modal (same path as Esc).
pub type DismissFn = Rc<dyn Fn(&mut Window, &mut App)>;

const MODAL_WIDTH: f32 = 620.0;
const MODAL_TOP_OFFSET_PX: f32 = 96.0;
const HEADER_HEIGHT: f32 = 44.0;
const FOOTER_HEIGHT: f32 = 30.0;
/// Cap the scrollable result area at ~13 rows so the card never runs off a
/// short window; longer catalogs (built-ins + custom commands, or a big file
/// index) scroll within this height.
const LIST_MAX_HEIGHT: f32 = ROW_HEIGHT * 13.0;
/// Backdrop scrim alpha — dark enough to lift the card off the workspace and
/// signal "click outside to dismiss", light enough to keep context visible.
const SCRIM_ALPHA: f32 = 0.20;

pub struct ModalRenderInput<'a> {
    pub mode: PaletteMode,
    pub query: &'a str,
    pub selected_idx: usize,
    /// Resolved palette items (Commands mode). Empty for QuickOpen mode.
    pub palette_items: &'a [PaletteItem],
    /// Quick-open file paths (QuickOpen mode). Empty for Commands mode.
    pub file_rows: Vec<&'a str>,
    /// Number of ACTIONABLE rows. In QuickOpen this is 0 when `file_rows`
    /// holds a single non-actionable status/hint line — so the hint renders
    /// without a hover/click affordance.
    pub row_count: usize,
    /// Blinking-caret phase for the query field (true = caret drawn).
    pub caret_on: bool,
    /// Activates the row at the given filtered-list index — dispatches the
    /// action AND closes the modal. Shared by click and keyboard.
    pub on_activate: ActivateFn,
    /// Dismisses the modal (backdrop click-outside). Same close path as Esc.
    pub on_dismiss: DismissFn,
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
    pub motion: Motion,
}

/// Build the full modal: a dimmed backdrop that dismisses on click-outside,
/// centering a floating card. The caller chains `.track_focus` and
/// `.on_key_down` on the returned element.
pub fn build_modal_layout(input: ModalRenderInput<'_>) -> gpui::Div {
    let dismiss = input.on_dismiss.clone();
    let motion = input.motion;

    let card = card_container(input.theme, input.density)
        // Stop presses inside the card from reaching the backdrop's
        // click-outside dismiss handler — an empty closure does NOT swallow,
        // so without `stop_propagation` clicking the header/search box (which
        // has no handler of its own) bubbles up and dismisses the palette.
        .on_mouse_down(MouseButton::Left, |_event, _window, cx| cx.stop_propagation())
        .child(header_row(
            input.mode,
            input.query,
            input.caret_on,
            input.theme,
            input.typography,
        ))
        .child(divider(input.theme))
        .child(result_list(&input))
        .child(divider(input.theme))
        .child(footer_hints(input.theme, input.typography));

    div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(MODAL_TOP_OFFSET_PX))
        .bg(hsla(0.0, 0.0, 0.0, SCRIM_ALPHA))
        .on_mouse_down(
            MouseButton::Left,
            move |_event, window, cx| dismiss(window, cx),
        )
        // Enter animation: the card fades in and rises 6px to rest. GPUI's
        // `div` exposes `opacity` but no transform-scale (that lives on
        // `svg`/`img` only), so the reference "0.98→1.0 scale" pop is
        // approximated with an opacity + vertical-offset settle — same
        // perceptual beat, only div-supported props. Keyed on a stable id so
        // it plays once on mount (the modal is unmounted while closed, so every
        // open re-mounts and replays) and settles across the in-flight
        // re-renders from typing / caret-blink rather than restarting. Reduced
        // motion collapses `m_overlay` to ~instant, so the card simply appears.
        .child(card.with_animation(
            "palette-enter",
            Animation::new(motion.m_overlay).with_easing(oximux_settings::ease_out_spring()),
            |el, delta| el.opacity(delta).mt(px(6.0 * (1.0 - delta))),
        ))
}

fn card_container(theme: Theme, density: Density) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .w(px(MODAL_WIDTH))
        .floating_chrome(&theme, &density)
        // Rounded corners must clip the scrollable list; shadow lifts the card
        // off the workspace to match the other floating overlays.
        .overflow_hidden()
        .shadow_lg()
}

fn header_row(
    mode: PaletteMode,
    query: &str,
    caret_on: bool,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    let mode_label = match mode {
        PaletteMode::QuickOpen => "Files",
        PaletteMode::Commands => "Commands",
        PaletteMode::WorkspaceJump => "Workspaces",
    };
    let placeholder = match mode {
        PaletteMode::QuickOpen => "Search files…",
        PaletteMode::Commands => "Search commands…",
        PaletteMode::WorkspaceJump => "Jump to workspace…",
    };

    // Blinking text caret. Fixed width whether on or off so the toggle never
    // nudges the text; transparent (alpha 0) on the off phase.
    let caret_color = if caret_on {
        theme.fg_base
    } else {
        hsla(0.0, 0.0, 0.0, 0.0)
    };
    let caret = div().w(px(1.5)).h(px(16.)).rounded(px(1.)).bg(caret_color);

    // Query area reads as a live input: typed text followed by the caret, or
    // the caret followed by greyed placeholder text when empty.
    let query_area = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .gap(px(1.))
        .text_size(px(typography.t_body_md))
        .when(!query.is_empty(), |d| {
            d.child(div().text_color(theme.fg_base).child(query.to_string()))
        })
        .child(caret)
        .when(query.is_empty(), |d| {
            d.child(
                div()
                    .text_color(theme.fg_subtle)
                    .child(placeholder.to_string()),
            )
        });

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .h(px(HEADER_HEIGHT))
        .child(
            Icon::new(IconName::Search)
                .size(px(14.))
                .text_color(theme.fg_subtle),
        )
        .child(
            div()
                .px(px(6.))
                .py(px(2.))
                .bg(theme.bg_panel_alt)
                .rounded(px(6.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_muted)
                .child(mode_label),
        )
        .child(query_area)
}

fn divider(theme: Theme) -> impl IntoElement {
    div().w_full().h(px(1.)).bg(theme.border_inactive)
}

/// Scrollable result column. Capped at [`LIST_MAX_HEIGHT`]; rows are inset by
/// the overlay padding so the rounded selection fill has a margin.
fn result_list(input: &ModalRenderInput<'_>) -> gpui::AnyElement {
    // `.id(...)` makes the column a stateful scroll container so the capped
    // height actually scrolls (the wheel/trackpad path needs the id).
    let mut col = div()
        .id("palette-results")
        .flex()
        .flex_col()
        .w_full()
        .px(px(input.density.pad_overlay))
        .py(px(4.))
        .max_h(px(LIST_MAX_HEIGHT))
        .overflow_y_scroll();

    match input.mode {
        PaletteMode::Commands => {
            let mut last_group: Option<PaletteGroup> = None;
            for (i, item) in input.palette_items.iter().enumerate() {
                if last_group != Some(item.display_group) {
                    if item.display_group == PaletteGroup::Custom {
                        col = col.child(group_label("Custom", input.theme, input.typography));
                    }
                    last_group = Some(item.display_group);
                }
                col = col.child(palette_row(
                    item,
                    i == input.selected_idx,
                    i,
                    input.query,
                    input.theme,
                    input.typography,
                    input.on_activate.clone(),
                ));
            }
        }
        // QuickOpen and WorkspaceJump both render plain string rows from
        // `file_rows` via `file_row`; only the row source + activation differ
        // (resolved by the caller before this runs).
        PaletteMode::QuickOpen | PaletteMode::WorkspaceJump => {
            // `row_count == 0` ⇒ the single row is a non-actionable hint.
            let actionable = input.row_count > 0;
            for (i, path) in input.file_rows.iter().enumerate() {
                col = col.child(file_row(
                    path,
                    actionable && i == input.selected_idx,
                    i,
                    input.query,
                    actionable,
                    input.theme,
                    input.typography,
                    input.on_activate.clone(),
                ));
            }
        }
    }
    col.into_any_element()
}

/// Footer keyboard hints — discoverability strip mirroring the nav model.
fn footer_hints(theme: Theme, typography: &Typography) -> impl IntoElement {
    let hint = |label: &str| -> gpui::Div {
        div()
            .text_size(px(typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child(label.to_string())
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.))
        .h(px(FOOTER_HEIGHT))
        .px(px(12.))
        .child(hint("↑↓ navigate"))
        .child(hint("↵ run"))
        .child(hint("esc dismiss"))
}
