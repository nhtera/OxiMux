//! GPUI element construction for the diff viewer.
//!
//! Sibling to `render.rs`. The data plan lives in `render.rs`
//! (`FilePlan`, `HunkPlan`, `LinePlan`, `build_render_plan`); this file
//! turns that plan into a FLAT, virtualization-ready row list
//! (`PreparedRow`) and paints only the visible window via `uniform_list`.
//!
//! ## Why flat + virtualized
//!
//! The earlier renderer emitted one `div` per row for every row of every
//! hunk of every file, plus one nested `div` per syntax token — eagerly,
//! every frame. A few-thousand-line diff became tens of thousands of
//! elements laid out and painted each frame (the lag), and the
//! `overflow_y_scroll` flex column couldn't compute a reliable max scroll
//! offset over that dynamic tree (so it never reached the last line).
//!
//! The fix mirrors how GPUI diff viewers do it (Zed / its mobile fork):
//!
//!   1. **Flatten** the nested plan into a single `Vec<PreparedRow>` so the
//!      body lives in one flat index space.
//!   2. **Virtualize** with `uniform_list` (fixed `h_row` height) — only the
//!      visible index range is built per frame, and the list owns scrolling
//!      so content height is exact (`rows × h_row`) → scroll reaches the end.
//!   3. **One `StyledText` per line** — syntax + word-diff colors become
//!      `HighlightStyle` ranges over a single shaped run, never a child
//!      element per colored token.
//!
//! Color resolution, gutter strings, and highlight ranges are all baked
//! once into `PreparedRow` (see `prepare`) so the per-frame render closure
//! does no string or highlight work for off-screen rows. The host
//! (`mod.rs`) caches the prepared vec behind an `Rc` and rebuilds it only
//! when the diff or `expanded` flag changes — never on scroll.

use std::ops::Range;
use std::rc::Rc;

use crate::shell::diff_view::hunk_actions::render_hunk_actions;
use crate::shell::diff_view::render::{FilePlan, LinePlan, RenderCtx};
use crate::shell::diff_view::word_diff::WordOp;
use crate::shell::diff_view::{DiffView, HunkActionSide};
use gpui::{
    App, ClickEvent, ClipboardItem, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement as _,
    Styled, StyledText, UniformListScrollHandle, WeakEntity, div, px, uniform_list,
};
use gpui_component::{Icon, Sizable as _, tooltip::Tooltip};
use oximux_core::DiffLineKind;
use oximux_settings::{Density, Theme, Typography};

/// One row in the flat, virtualization-ready body. Every variant renders
/// at exactly `density.h_row` so `uniform_list` can position rows by index
/// without per-item measurement.
pub enum PreparedRow {
    /// Interactive file header — click copies the path. `stats` carries
    /// `+added / -removed` chips for hunked files; `None` for
    /// binary/mode-only/collapsed headers that have no per-row tally.
    FileHeader {
        path: SharedString,
        label: String,
        stats: Option<(u32, u32)>,
    },
    /// `@@ … @@` hunk marker. Carries the originating (file, hunk) index so
    /// the Stage/Unstage/Discard chips can dispatch against the live diff.
    HunkHeader {
        file_idx: usize,
        hunk_idx: usize,
        header: SharedString,
        strip: Option<Hsla>,
    },
    /// A diff body line, fully resolved (colors, gutter cells, highlight
    /// ranges) so painting it touches no syntect / word-diff machinery.
    Line(PreparedLine),
    /// Large-diff collapse affordance — click expands the whole view.
    Collapsed { label: String },
    /// Inline notice body for binary / mode-only files (and "No diff").
    Special { text: String },
}

/// A diff line with every render input pre-resolved. Built once in
/// `prepare`; the render closure only clones the highlight iterator.
pub struct PreparedLine {
    /// Leading `+` / `-` / ` ` glyph, painted in the row foreground so the
    /// row kind reads even when highlight ranges recolor the content.
    pub prefix: char,
    pub content: SharedString,
    /// Syntax (or word-diff) colors as byte-range highlights over `content`.
    /// Empty → the whole line uses the inherited row foreground.
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub old_cell: String,
    pub new_cell: String,
    pub fg: Hsla,
    pub row_bg: Option<Hsla>,
    pub strip: Option<Hsla>,
    pub strikethrough: bool,
}

/// Flatten the structured plan into the virtualization-ready row list,
/// resolving all colors, gutter strings, and highlight ranges up front.
/// Runs once per (diff, expanded) change — NOT per frame.
///
/// `regions_per_file[i]` are the stageable change regions for `plan[i]`
/// (from `oximux_core::change_regions`). Because diffs are fetched with
/// full-file context, the chip bar (Stage/Unstage/Discard) is docked at
/// the start of EACH region rather than once per file — restoring
/// `git add -p` granularity over a whole-file view. The chip's
/// `hunk_idx` field carries the REGION index, which
/// `DiffView::stage_hunk` maps back through `change_regions` to the
/// matching standalone patch.
pub fn prepare(
    plan: &[FilePlan],
    regions_per_file: &[Vec<oximux_core::ChangeRegion>],
    rctx: &RenderCtx<'_>,
) -> Vec<PreparedRow> {
    // Gutter width auto-fits the largest line number across the WHOLE body
    // so the gutter/content divider stays aligned across every file and
    // hunk (no per-hunk horizontal shift).
    let max_line = plan
        .iter()
        .filter_map(|fp| match fp {
            FilePlan::Hunked { hunks, .. } => Some(hunks),
            _ => None,
        })
        .flat_map(|hunks| hunks.iter())
        .flat_map(|h| h.rows.iter())
        .map(|r| r.old_line.unwrap_or(0).max(r.new_line.unwrap_or(0)))
        .max()
        .unwrap_or(0);
    let gutter_digits = digit_count(max_line);

    let mut rows = Vec::new();
    for (file_idx, fp) in plan.iter().enumerate() {
        let regions: &[oximux_core::ChangeRegion] = regions_per_file
            .get(file_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match fp {
            FilePlan::Hunked {
                path,
                header,
                hunks,
                added,
                removed,
            } => {
                rows.push(PreparedRow::FileHeader {
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: Some((*added, *removed)),
                });
                // Walk every line in file order; before the first changed
                // line of each region, dock that region's chip bar. Regions
                // are anchored by line number (robust to hunk merging) and
                // matched sequentially since both lists are in file order.
                let mut next_region = 0usize;
                for h in hunks.iter() {
                    for l in &h.rows {
                        if next_region < regions.len()
                            && line_matches_anchor(l, &regions[next_region])
                        {
                            let r = &regions[next_region];
                            rows.push(PreparedRow::HunkHeader {
                                file_idx,
                                hunk_idx: next_region,
                                header: region_label(r).into(),
                                strip: region_strip(r, rctx),
                            });
                            next_region += 1;
                        }
                        let strip = line_change_strip(l.kind, rctx);
                        rows.push(PreparedRow::Line(prepare_line(
                            l,
                            strip,
                            gutter_digits,
                            rctx,
                        )));
                    }
                }
            }
            FilePlan::Collapsed {
                path,
                header,
                total_lines,
                hunk_count,
            } => {
                rows.push(PreparedRow::FileHeader {
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                });
                rows.push(PreparedRow::Collapsed {
                    label: format!(
                        "Large diff: {hunk_count} hunks, {total_lines} lines — click to expand"
                    ),
                });
            }
            FilePlan::Binary { path, header } => {
                rows.push(PreparedRow::FileHeader {
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                });
                rows.push(PreparedRow::Special {
                    text: "Binary file (body suppressed)".to_string(),
                });
            }
            FilePlan::ModeOnly {
                path,
                header,
                old_mode,
                new_mode,
            } => {
                rows.push(PreparedRow::FileHeader {
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                });
                rows.push(PreparedRow::Special {
                    text: format!("Mode change only: {old_mode:o} → {new_mode:o}"),
                });
            }
        }
    }
    rows
}

/// Resolve one plan line into a fully-baked `PreparedLine`.
fn prepare_line(
    l: &LinePlan,
    strip: Option<Hsla>,
    gutter_digits: usize,
    rctx: &RenderCtx<'_>,
) -> PreparedLine {
    let (prefix, fg, row_bg) = match l.kind {
        DiffLineKind::Context => (' ', rctx.theme.fg_muted, None),
        DiffLineKind::Added => (
            '+',
            rctx.theme.status_ok,
            // Faded green telegraphs the added range stronger than the `+`
            // glyph alone. a=0.22 reads clearly green on the charcoal
            // theme without bleaching the foreground text.
            Some(Hsla {
                a: 0.22,
                ..rctx.theme.git.added
            }),
        ),
        DiffLineKind::Removed => (
            '-',
            rctx.theme.status_error,
            Some(Hsla {
                a: 0.22,
                ..rctx.theme.git.deleted
            }),
        ),
        DiffLineKind::NoNewlineHint => ('\\', rctx.theme.fg_subtle, None),
    };
    PreparedLine {
        prefix,
        content: l.content.clone().into(),
        highlights: line_highlights(l, rctx),
        old_cell: pack_gutter_cell(l.old_line, l.kind, /*is_new_side=*/ false, gutter_digits),
        new_cell: pack_gutter_cell(l.new_line, l.kind, /*is_new_side=*/ true, gutter_digits),
        fg,
        row_bg,
        strip,
        strikethrough: matches!(l.kind, DiffLineKind::Removed),
    }
}

/// Build the byte-range highlight list for one line. Precedence matches
/// the old per-token renderer:
///
///   1. **Word-diff** (paired Added/Removed with spans): each span colored
///      `Same`=muted, `Insert`=green, `Delete`=red. Loses syntax color on
///      that row — the user is reading "what changed", not "what is this".
///   2. **Syntax tokens** (Context, or unpaired Add/Remove): each syntect
///      token painted with its theme color.
///   3. **Fallback** (Unknown language / blank): empty → inherited row fg.
fn line_highlights(l: &LinePlan, rctx: &RenderCtx<'_>) -> Vec<(Range<usize>, HighlightStyle)> {
    let content = l.content.as_str();
    if let Some(spans) = l.spans.as_ref()
        && matches!(l.kind, DiffLineKind::Added | DiffLineKind::Removed)
    {
        let mut out = Vec::with_capacity(spans.len());
        let mut off = 0usize;
        for span in spans {
            let len = span.text.len();
            let end = (off + len).min(content.len());
            if off < end
                && content.is_char_boundary(off)
                && content.is_char_boundary(end)
            {
                let color = match span.op {
                    WordOp::Same => rctx.theme.fg_muted,
                    WordOp::Insert => rctx.theme.status_ok,
                    WordOp::Delete => rctx.theme.status_error,
                };
                out.push((off..end, color_highlight(color)));
            }
            off += len;
        }
        return out;
    }
    if !l.tokens.is_empty() {
        let max = content.len();
        let mut out = Vec::with_capacity(l.tokens.len());
        for tok in &l.tokens {
            let start = tok.start.min(max);
            let end = tok.end.min(max);
            if start >= end
                || !content.is_char_boundary(start)
                || !content.is_char_boundary(end)
            {
                continue;
            }
            let color = Hsla::from(gpui::Rgba {
                r: tok.r as f32 / 255.0,
                g: tok.g as f32 / 255.0,
                b: tok.b as f32 / 255.0,
                a: 1.0,
            });
            out.push((start..end, color_highlight(color)));
        }
        return out;
    }
    Vec::new()
}

fn color_highlight(color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(color),
        ..Default::default()
    }
}

/// Render the prepared rows into the virtualized body. Called from
/// `DiffView::render` with the cached `Rc<Vec<PreparedRow>>`.
///
/// `side` gates the per-hunk action chips (Stage/Unstage/Discard) — `None`
/// on commit-detail / read-only views. `weak` routes chip + expand + copy
/// clicks back into the view from the `uniform_list` closure's App scope.
pub fn render_rows(
    rows: Rc<Vec<PreparedRow>>,
    side: Option<HunkActionSide>,
    scroll: &UniformListScrollHandle,
    rctx: &RenderCtx<'_>,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    if rows.is_empty() {
        return placeholder("No diff".to_string(), rctx).into_any_element();
    }
    let theme = rctx.theme;
    let density = rctx.density;
    let typography = rctx.typography.clone();
    uniform_list(
        "diff-view-rows",
        rows.len(),
        move |range, _window, _cx| {
            range
                .filter_map(|i| rows.get(i))
                .map(|row| {
                    build_prepared_row(row, side, theme, density, &typography, weak.clone())
                })
                .collect()
        },
    )
    // `h_full` (NOT `flex_1`) is load-bearing: `uniform_list` implements its
    // own Element and needs a DEFINITE height to lay rows against + compute
    // its scroll range. With `flex_1` it infers content height, so its
    // viewport ≈ content and the scroll max collapses → can't reach the end.
    // The DiffView root is the `flex_col().h_full()` wrapper that bounds it.
    // (Same rule as every other uniform_list in the app — see file_explorer.)
    .track_scroll(scroll)
    .h_full()
    .w_full()
    .into_any_element()
}

/// Build a single visible row. Every branch pins the height to `h_row` so
/// the list's uniform-height assumption holds.
fn build_prepared_row(
    row: &PreparedRow,
    side: Option<HunkActionSide>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> gpui::AnyElement {
    match row {
        PreparedRow::FileHeader { path, label, stats } => {
            file_header_row(path.clone(), label.clone(), *stats, theme, density, typography)
                .into_any_element()
        }
        PreparedRow::HunkHeader {
            file_idx,
            hunk_idx,
            header,
            strip,
        } => hunk_header_row(
            *file_idx, *hunk_idx, header.clone(), *strip, side, theme, density, typography, weak,
        )
        .into_any_element(),
        PreparedRow::Line(line) => {
            line_row_el(line, theme, density, typography).into_any_element()
        }
        PreparedRow::Collapsed { label } => {
            collapsed_row(label.clone(), theme, density, typography, weak).into_any_element()
        }
        PreparedRow::Special { text } => {
            special_row(text.clone(), theme, density, typography).into_any_element()
        }
    }
}

/// Interactive file header — click copies the path. Layout, left→right:
/// `<path>` `· <status>` `+added` `-removed` `<spacer>` `<copy glyph>`.
fn file_header_row(
    path: SharedString,
    label: String,
    stats: Option<(u32, u32)>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let id = gpui::ElementId::Name(format!("diff-header-{path}").into());
    let copy_path = path.clone();
    let hover_bg = theme.bg_panel_alt;
    let tooltip_text: SharedString = "Click to copy path".into();
    let mut row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
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
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .on_click(move |_: &ClickEvent, _window, cx: &mut App| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_path.to_string()));
        })
        .child(div().child(path))
        .child(
            div()
                .text_color(theme.fg_muted)
                .text_size(px(typography.t_body_sm))
                .child(format!("· {label}")),
        );
    if let Some((added, removed)) = stats {
        row = row.child(stats_chips(added, removed, theme, typography));
    }
    row.child(div().flex_1()).child(
        div().child(
            Icon::default()
                .path("icons/copy.svg")
                .xsmall()
                .text_color(theme.fg_subtle),
        ),
    )
}

/// `+N -N` chip cluster. Both sides always show (e.g. `+0`) so the header
/// doesn't reflow when one side's count changes.
fn stats_chips(
    added: u32,
    removed: u32,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .gap(px(6.0))
        .text_size(px(typography.t_body_sm))
        .child(div().text_color(theme.git.added).child(format!("+{added}")))
        .child(
            div()
                .text_color(theme.git.deleted)
                .child(format!("-{removed}")),
        )
}

#[allow(clippy::too_many_arguments)]
fn hunk_header_row(
    file_idx: usize,
    hunk_idx: usize,
    header: SharedString,
    strip: Option<Hsla>,
    side: Option<HunkActionSide>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    let actions = side.and_then(|s| {
        render_hunk_actions(s, file_idx, hunk_idx, theme, density, typography, &weak)
    });
    let mut row = div()
        .flex()
        .items_center()
        .h(px(density.h_row))
        .w_full()
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(strip_cell(strip, density.h_row))
        .child(div().px(px(density.pad_panel)).child(header));
    if let Some(actions) = actions {
        row = row.child(div().flex_1()).child(actions);
    }
    row
}

/// One diff body line: strip cell + dual gutter + a SINGLE `StyledText`
/// for the content (colors via highlight ranges, not child elements).
/// `overflow_hidden` clips long lines horizontally so the row height stays
/// fixed — horizontal scrolling of long lines is out of scope.
fn line_row_el(
    line: &PreparedLine,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let gutter = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .px(px(density.pad_panel))
        // Monospace the gutter so right-aligned line numbers stay column-true.
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(div().child(line.old_cell.clone()))
        .child(div().child(line.new_cell.clone()));

    let mut content = div()
        .flex()
        .flex_row()
        .pr(px(density.pad_panel))
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(line.fg)
        // Prefix stays in the row foreground so the kind reads even when
        // highlight ranges recolor the body.
        .child(div().child(format!("{} ", line.prefix)))
        .child(
            StyledText::new(line.content.clone())
                .with_highlights(line.highlights.iter().cloned()),
        );
    if line.strikethrough {
        // Strikethrough on the content cell only — the gutter stays crisp.
        // Inherited by the `StyledText` default style via `window.text_style()`.
        content = content.line_through();
    }

    let mut row = div()
        .flex()
        .items_center()
        .h(px(density.h_row))
        .w_full()
        .overflow_hidden()
        .text_size(px(typography.t_body_sm));
    if let Some(bg) = line.row_bg {
        row = row.bg(bg);
    }
    row.child(strip_cell(line.strip, density.h_row))
        .child(gutter)
        .child(content)
}

fn collapsed_row(
    label: String,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    div()
        .id("diff-view-expand")
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_row))
        .w_full()
        .px(px(density.pad_panel))
        .bg(theme.bg_panel)
        .text_size(px(typography.t_body_sm))
        .text_color(theme.status_info)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _window, cx: &mut App| {
            let _ = weak.update(cx, |view, cx| {
                view.expand();
                cx.notify();
            });
        })
        .child(label)
}

fn special_row(
    text: String,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(density.h_row))
        .w_full()
        .px(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(text)
}

/// Per-line change bar in the strip column: green for additions, red for
/// deletions, none for context. In a full-file view this reads as a gutter
/// change indicator that pinpoints exactly which lines changed.
fn line_change_strip(kind: DiffLineKind, rctx: &RenderCtx<'_>) -> Option<Hsla> {
    let alpha = |c: Hsla| Hsla { a: 0.7, ..c };
    match kind {
        DiffLineKind::Added => Some(alpha(rctx.theme.git.added)),
        DiffLineKind::Removed => Some(alpha(rctx.theme.git.deleted)),
        _ => None,
    }
}

/// Strip color for a region's chip-bar header: orange when the region
/// mixes adds + deletes, green for pure adds, red for pure deletes.
fn region_strip(region: &oximux_core::ChangeRegion, rctx: &RenderCtx<'_>) -> Option<Hsla> {
    let mut has_add = false;
    let mut has_rem = false;
    for l in &region.stage_hunk.lines {
        match l.kind {
            DiffLineKind::Added => has_add = true,
            DiffLineKind::Removed => has_rem = true,
            _ => {}
        }
    }
    let alpha = |c: Hsla| Hsla { a: 0.7, ..c };
    match (has_add, has_rem) {
        (true, true) => Some(alpha(rctx.theme.status_warn)),
        (true, false) => Some(alpha(rctx.theme.git.added)),
        (false, true) => Some(alpha(rctx.theme.git.deleted)),
        _ => None,
    }
}

/// `@@ -a,b +c,d @@` location label for a region's chip bar — tells the
/// reviewer where in the file the change sits, the way `git add -p` does.
fn region_label(region: &oximux_core::ChangeRegion) -> String {
    let h = &region.stage_hunk;
    format!(
        "@@ -{},{} +{},{} @@",
        h.old_start, h.old_lines, h.new_start, h.new_lines
    )
}

/// Does this display line start `region`? Regions are anchored by the
/// line number of their first changed line (addition → new side, deletion
/// → old side), which survives hunk merging from full-file context.
fn line_matches_anchor(l: &LinePlan, region: &oximux_core::ChangeRegion) -> bool {
    if let Some(n) = region.anchor_new {
        matches!(l.kind, DiffLineKind::Added) && l.new_line == Some(n)
    } else if let Some(o) = region.anchor_old {
        matches!(l.kind, DiffLineKind::Removed) && l.old_line == Some(o)
    } else {
        false
    }
}

/// 3px colored cell prepended to every row in a hunk. Transparent when
/// `color` is `None` so context-only rows lay out the same width.
fn strip_cell(color: Option<Hsla>, h: f32) -> gpui::Div {
    let mut cell = div().w(px(3.0)).h(px(h));
    if let Some(c) = color {
        cell = cell.bg(c);
    }
    cell
}

fn digit_count(n: u32) -> usize {
    // Minimum 2 digits keeps narrow files (≤ 9 lines) from looking cramped.
    n.checked_ilog10()
        .map(|d| d as usize + 1)
        .unwrap_or(0)
        .max(2)
}

/// Build one gutter cell as `<right-aligned number><sign>`. The sign
/// column is always 1 char wide so cells align with or without a sign.
fn pack_gutter_cell(
    line_no: Option<u32>,
    kind: DiffLineKind,
    is_new_side: bool,
    digits: usize,
) -> String {
    let n = match line_no {
        Some(n) => format!("{n:>width$}", width = digits),
        None => " ".repeat(digits),
    };
    let sign = match (kind, is_new_side, line_no.is_some()) {
        (DiffLineKind::Added, true, true) => '+',
        (DiffLineKind::Removed, false, true) => '-',
        _ => ' ',
    };
    format!("{n}{sign}")
}

fn placeholder(msg: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .w_full()
        .p(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(msg)
}
