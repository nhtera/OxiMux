//! File-header row + sticky-header overlay for the diff body.
//!
//! Extracted from `paint.rs` so the header — which now carries a fold
//! chevron, a copy affordance, and a sticky-overlay twin — lives in one
//! cohesive place instead of bloating the line-painting module.
//!
//! Two surfaces share one builder (`file_header_row`): the in-list header
//! that scrolls with the body, and the sticky overlay
//! (`sticky_header_overlay`) pinned to the top of the viewport showing the
//! file whose body is currently on screen. Both route a row-click to
//! `DiffView::toggle_file_fold`; a trailing copy glyph copies the path (and
//! stops propagation so it doesn't also fold).

use crate::shell::diff_view::DiffView;
use crate::shell::diff_view::paint::PreparedRow;
use gpui::{
    App, ClickEvent, ClipboardItem, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, WeakEntity, div, px,
};
use gpui_component::{Icon, Sizable as _, tooltip::Tooltip};
use oximux_settings::{Density, Theme, Typography};

/// Per-file header metadata, collected from the prepared row list so the
/// sticky overlay can rebuild any file's header without re-walking the diff.
#[derive(Clone)]
pub struct StickyHeader {
    pub file_idx: usize,
    pub path: SharedString,
    pub label: String,
    pub stats: Option<(u32, u32)>,
    pub folded: bool,
}

/// The file_idx that owns each prepared row (the most recent `FileHeader`
/// above it). Lets the sticky overlay answer "which file is at the top of
/// the viewport" in O(1) from the first-visible row index. Built once per
/// prepared rebuild, off the per-frame path.
pub fn build_row_owner(rows: &[PreparedRow]) -> Vec<usize> {
    let mut owner = Vec::with_capacity(rows.len());
    let mut current = 0usize;
    for row in rows {
        if let PreparedRow::FileHeader { file_idx, .. } = row {
            current = *file_idx;
        }
        owner.push(current);
    }
    owner
}

/// Collect one `StickyHeader` per file, in file order. Built alongside
/// `build_row_owner` during the prepared rebuild.
pub fn collect_headers(rows: &[PreparedRow]) -> Vec<StickyHeader> {
    rows.iter()
        .filter_map(|row| match row {
            PreparedRow::FileHeader {
                file_idx,
                path,
                label,
                stats,
                folded,
            } => Some(StickyHeader {
                file_idx: *file_idx,
                path: path.clone(),
                label: label.clone(),
                stats: *stats,
                folded: *folded,
            }),
            _ => None,
        })
        .collect()
}

/// Interactive file header. Layout, left→right:
/// `<chevron>` `<path>` `· <status>` `+added` `-removed` `<spacer>` `<copy>`.
/// Row-click folds/unfolds the file; the copy glyph copies the path.
/// `sticky` only disambiguates element ids between the in-list row and the
/// pinned overlay twin (GPUI needs unique interactive ids).
#[allow(clippy::too_many_arguments)]
pub fn file_header_row(
    file_idx: usize,
    path: SharedString,
    label: String,
    stats: Option<(u32, u32)>,
    folded: bool,
    sticky: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    let kind = if sticky { "sticky" } else { "list" };
    let id = gpui::ElementId::Name(format!("diff-header-{kind}-{file_idx}").into());
    let copy_id = gpui::ElementId::Name(format!("diff-header-copy-{kind}-{file_idx}").into());
    let copy_path = path.clone();
    let hover_bg = theme.bg_panel_alt;
    let chevron = if folded {
        "icons/chevron-right.svg"
    } else {
        "icons/chevron-down.svg"
    };
    let mut row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .h(px(density.h_row))
        .w_full()
        .px(px(density.pad_panel))
        .bg(theme.bg_panel)
        .border_b_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_label_caps))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .on_click(move |_: &ClickEvent, _window, cx: &mut App| {
            let _ = weak.update(cx, |view, cx| {
                view.toggle_file_fold(file_idx);
                cx.notify();
            });
        })
        .child(
            // Fold indicator — chevron-right when folded, chevron-down open.
            Icon::default()
                .path(chevron)
                .xsmall()
                .text_color(theme.fg_subtle),
        )
        .child(div().child(path))
        .child(
            div()
                .text_color(theme.fg_muted)
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_regular)
                .child(format!("· {label}")),
        );
    if let Some((added, removed)) = stats {
        row = row.child(stats_chips(added, removed, theme, typography));
    }
    let copy_tooltip: SharedString = "Click to copy path".into();
    row.child(div().flex_1()).child(
        // Copy glyph — its own click handler stops propagation so copying
        // the path doesn't also toggle the fold.
        div()
            .id(copy_id)
            .cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(copy_tooltip.clone()).build(window, cx))
            .on_click(move |_: &ClickEvent, _window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_path.to_string()));
                cx.stop_propagation();
            })
            .child(
                Icon::default()
                    .path("icons/copy.svg")
                    .xsmall()
                    .text_color(theme.fg_subtle),
            ),
    )
}

/// `+N -N` chip cluster. Both sides always show (e.g. `+0`) so the header
/// doesn't reflow when one side's count changes. Thin space (U+2009) +
/// figure dash (U+2012) for a tighter, typographically clean tally.
fn stats_chips(added: u32, removed: u32, theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap(px(6.0))
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_regular)
        .child(
            div()
                .text_color(theme.git.added)
                .child(format!("+\u{2009}{added}")),
        )
        .child(
            div()
                .text_color(theme.git.deleted)
                .child(format!("\u{2012}\u{2009}{removed}")),
        )
}

/// The pinned header overlay: an absolutely-positioned twin of the
/// first-visible file's header, stacked at the top of the scroll body so
/// the filename + stats stay on screen while its diff scrolls under it.
/// Gains a shadow only when `stuck` (a real header has scrolled above the
/// viewport); when the file's own header is flush at the top the overlay
/// overlaps it seamlessly with no shadow.
pub fn sticky_header_overlay(
    header: &StickyHeader,
    stuck: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    let mut wrap = div().absolute().top_0().left_0().right_0().child(file_header_row(
        header.file_idx,
        header.path.clone(),
        header.label.clone(),
        header.stats,
        header.folded,
        true,
        theme,
        density,
        typography,
        weak,
    ));
    if stuck {
        wrap = wrap.shadow_md();
    }
    wrap
}
