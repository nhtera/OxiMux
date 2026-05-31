//! Row painters for the search result list.
//!
//! Pure rendering glue between `SearchRow` plans and GPUI elements. Logic
//! that affects content (truncation, summary text) lives in sibling pure
//! modules; this file only assembles divs, click handlers, and colors.

use crate::shell::file_explorer::file_icon::icon_for_name;
use crate::shell::search_panel::SearchPanel;
use crate::shell::search_panel::match_render::truncate_before;
use crate::shell::search_panel::rg_runner::SearchResults;
use gpui::{
    AnyElement, Context, ElementId, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, px,
};
use gpui_component::{Icon, IconName, Sizable as _};
use oximux_settings::{Theme, Typography};

/// Constants shared by file + match rows. Both heights are intentionally
/// tighter than the global `density.h_row = 24` because search results
/// are scan-heavy: file rows pack 2px tighter and per-line match rows
/// pack another 4px tighter so a typical 10-match file fits in the same
/// vertical space as a single SCM file row. Documented locally instead
/// of adding `h_search_file` / `h_search_match` tokens until a second
/// surface needs them.
const FILE_ROW_H: f32 = 22.0;
const MATCH_ROW_H: f32 = 18.0;
const ROW_INDENT: f32 = 14.0;

/// Per-render context. Cloned cheaply into closures.
pub(super) struct RowPaintCtx {
    pub theme: Theme,
    pub typography: Typography,
}

/// Paint a file header row: chevron + file-type icon + filename + dim parent + count pill.
pub(super) fn paint_file_row(
    results: Option<&SearchResults>,
    file_index: usize,
    match_count: usize,
    collapsed: bool,
    ctx: &RowPaintCtx,
    cx: &mut Context<SearchPanel>,
) -> AnyElement {
    let Some(file) = results.and_then(|r| r.files.get(file_index)) else {
        return div().into_any_element();
    };
    let theme = ctx.theme;
    let typography = &ctx.typography;

    let (parent_dim, file_name) = split_relative(&file.relative_path);
    let row_id = ElementId::Name(format!("sp-file-{}", file.file_path.display()).into());
    let click_path = file.file_path.clone();

    let chevron = if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };

    let file_icon_path = icon_for_name(&file_name);

    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .w_full()
        .h(px(FILE_ROW_H))
        .pl(px(6.0))
        .pr(px(8.0))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_base)
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |me, _: &MouseDownEvent, _window, cx| {
                me.toggle_file_collapse(click_path.clone(), cx);
            }),
        )
        .child(Icon::new(chevron).xsmall().text_color(theme.fg_subtle))
        .child(file_type_icon_el(file_icon_path, theme))
        .child(
            div()
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child(file_name),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_color(theme.fg_subtle)
                .child(parent_dim),
        )
        .child(count_pill(match_count, theme, typography))
        .into_any_element()
}

fn file_type_icon_el(icon_path: Option<&'static str>, theme: Theme) -> AnyElement {
    match icon_path {
        Some(path) => Icon::default()
            .path(path)
            .xsmall()
            .text_color(theme.fg_muted)
            .into_any_element(),
        None => Icon::new(IconName::File)
            .xsmall()
            .text_color(theme.fg_muted)
            .into_any_element(),
    }
}

fn count_pill(count: usize, theme: Theme, typography: &Typography) -> AnyElement {
    div()
        .flex_shrink_0()
        .ml_auto()
        .px(px(6.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme.bg_overlay)
        .text_size(px(typography.t_label_caps))
        .text_color(theme.fg_muted)
        .child(count.to_string())
        .into_any_element()
}

/// Paint one match row: line number + truncated content with amber highlight span.
pub(super) fn paint_match_row(
    results: Option<&SearchResults>,
    file_index: usize,
    match_index: usize,
    ctx: &RowPaintCtx,
    cx: &mut Context<SearchPanel>,
) -> AnyElement {
    let Some(file) = results.and_then(|r| r.files.get(file_index)) else {
        return div().into_any_element();
    };
    let Some(m) = file.matches.get(match_index) else {
        return div().into_any_element();
    };
    let theme = ctx.theme;
    let typography = &ctx.typography;

    // rg gives 1-based byte column; truncate_before is 0-indexed.
    let col0 = m.column.saturating_sub(1) as usize;
    let truncated = truncate_before(&m.line_content, col0, m.match_length);

    let row_id = ElementId::Name(format!("sp-match-{file_index}-{match_index}").into());
    let click_path = file.file_path.clone();

    div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .w_full()
        .h(px(MATCH_ROW_H))
        .pl(px(ROW_INDENT + 14.0))
        .pr(px(8.0))
        .text_size(px(typography.t_body_sm))
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |me, _: &MouseDownEvent, _window, _cx| {
                me.open_match(&click_path);
            }),
        )
        .child(
            div()
                .w(px(28.0))
                .flex_shrink_0()
                .text_color(theme.fg_subtle)
                .text_size(px(typography.t_body_sm))
                .child(m.line.to_string()),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .flex()
                .flex_row()
                .items_center()
                .text_color(theme.fg_muted)
                .child(div().child(truncated.before))
                .child(
                    div()
                        .px(px(2.0))
                        .rounded(px(2.0))
                        .bg(amber_highlight_bg(theme))
                        .text_color(theme.fg_base)
                        .child(truncated.matched),
                )
                .child(div().text_color(theme.fg_muted).child(truncated.after)),
        )
        .into_any_element()
}

/// Match highlight color — amber with a soft alpha so the row reads but
/// doesn't shout. Uses the same hue as `match_bg_current` (the cycled
/// terminal-search highlight) for cross-feature consistency.
fn amber_highlight_bg(theme: Theme) -> gpui::Hsla {
    let mut bg = theme.match_bg_current;
    bg.a = 0.32;
    bg
}

/// Split `relative_path` into ("parent/dir", "leaf.rs") for header display.
fn split_relative(rel: &std::path::Path) -> (String, String) {
    let leaf = rel
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.display().to_string());
    let parent = rel
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    (parent, leaf)
}
