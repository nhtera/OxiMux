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

use crate::shell::diff_view::file_header::file_header_row;
use crate::shell::diff_view::hunk_actions::render_hunk_actions;
use crate::shell::diff_view::render::{FilePlan, LinePlan, RenderCtx};
use crate::shell::diff_view::word_diff::WordOp;
use crate::shell::diff_view::{DiffView, HunkActionSide};
use gpui::{
    App, ClickEvent, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, ParentElement, SharedString,
    StatefulInteractiveElement as _, Styled, StyledText, UniformListScrollHandle, WeakEntity, div,
    px, relative, uniform_list,
};
use oximux_core::DiffLineKind;
use oximux_settings::{Density, Theme, Typography};

/// One row in the flat, virtualization-ready body. Every variant renders
/// at exactly `density.h_row` so `uniform_list` can position rows by index
/// without per-item measurement.
pub enum PreparedRow {
    /// Interactive file header — click folds the file, the copy glyph
    /// copies the path. `stats` carries `+added / -removed` chips for hunked
    /// files; `None` for binary/mode-only/collapsed headers. `folded`
    /// drives the chevron direction and (via `prepare`) body suppression.
    FileHeader {
        file_idx: usize,
        path: SharedString,
        label: String,
        stats: Option<(u32, u32)>,
        folded: bool,
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
    /// A side-by-side body row: original (left) | modified (right). Either
    /// side is `None` (filler) where a block is longer on the opposite side.
    /// Only emitted in split mode; inline mode uses `Line`.
    SplitLine {
        left: Option<SideCell>,
        right: Option<SideCell>,
    },
    /// Large-diff collapse affordance — click expands the whole view.
    Collapsed { label: String },
    /// Inline notice body for binary / mode-only files (and "No diff").
    Special { text: String },
}

/// One column of a side-by-side row — a single line number + content with
/// the same syntax/word highlights and tint the inline view uses, but a
/// single (not dual) gutter.
pub struct SideCell {
    pub gutter: String,
    pub content: SharedString,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub strip: Option<Hsla>,
    /// Overview-ruler classification for this side (`None` for context).
    pub mark: Option<RulerMark>,
}

/// What an overview-ruler tick represents — drives its color. `Mixed` is a
/// side-by-side row whose two columns are a removed/added pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RulerMark {
    Added,
    Removed,
    Mixed,
}

/// A contiguous run of same-kind change rows, as fractions (0..1) down the
/// full body — one painted bar on the overview ruler. Coalescing adjacent
/// changed rows into runs keeps the ruler at ~one element per hunk instead
/// of one per line (so it never reintroduces per-line element blowup).
pub struct OverviewRun {
    pub start: f32,
    pub end: f32,
    pub mark: RulerMark,
}

/// A diff line with every render input pre-resolved. Built once in
/// `prepare`; the render closure only clones the highlight iterator.
pub struct PreparedLine {
    pub content: SharedString,
    /// Layered byte-range highlights over `content`: syntax tokens as
    /// foreground `color`, word-diff Insert/Delete as `background_color`.
    /// The two compose on one shaped run — syntax color stays, changed
    /// words get a brighter background. Empty → inherited row foreground.
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub old_cell: String,
    pub new_cell: String,
    pub fg: Hsla,
    pub row_bg: Option<Hsla>,
    pub strip: Option<Hsla>,
    /// The change region this line belongs to, as `(file_idx, region_idx)`,
    /// or `None` for context outside any region. Drives the hover-widen of
    /// the gutter sliver when its region's header is hovered.
    pub region: Option<(usize, usize)>,
    /// Overview-ruler classification (`None` for context — context rows put
    /// no tick on the ruler).
    pub mark: Option<RulerMark>,
}

/// Index of the row whose laid-out width is largest, used as
/// `uniform_list`'s measurement item so the list's horizontal scroll range
/// spans the longest line (every item then lays out at that width). Approx
/// by character count — gutter width is constant across rows, so the line
/// with the most content chars is the widest. Computed once per prepared
/// rebuild, NOT per frame.
pub fn widest_row_index(rows: &[PreparedRow]) -> usize {
    rows.iter()
        .enumerate()
        .max_by_key(|(_, row)| match row {
            PreparedRow::Line(l) => l.content.chars().count(),
            PreparedRow::SplitLine { left, right } => {
                // Either column can be the wider; the body lays the two side
                // by side so the row's effective width tracks the larger one.
                let w = |c: &Option<SideCell>| c.as_ref().map_or(0, |s| s.content.chars().count());
                w(left).max(w(right))
            }
            PreparedRow::HunkHeader { header, .. } => header.chars().count(),
            PreparedRow::FileHeader { path, label, .. } => {
                path.chars().count() + label.chars().count()
            }
            PreparedRow::Collapsed { label } => label.chars().count(),
            PreparedRow::Special { text } => text.chars().count(),
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Character count of the widest row's content — clamps the split-mode
/// horizontal scroll. For split rows it's the longer of the two columns.
pub fn widest_row_chars(rows: &[PreparedRow]) -> usize {
    rows.iter()
        .map(|row| match row {
            PreparedRow::Line(l) => l.content.chars().count(),
            PreparedRow::SplitLine { left, right } => {
                let w = |c: &Option<SideCell>| c.as_ref().map_or(0, |s| s.content.chars().count());
                w(left).max(w(right))
            }
            PreparedRow::HunkHeader { header, .. } => header.chars().count(),
            PreparedRow::FileHeader { path, label, .. } => {
                path.chars().count() + label.chars().count()
            }
            PreparedRow::Collapsed { label } => label.chars().count(),
            PreparedRow::Special { text } => text.chars().count(),
        })
        .max()
        .unwrap_or(0)
}

/// Overview-ruler classification for one prepared row: the change kind to
/// mark on the scrollbar ruler, or `None` for rows that aren't changed body
/// lines (headers, context, collapsed, special). A split row whose two
/// columns are a removed/added pair reads as `Mixed`.
fn row_mark(row: &PreparedRow) -> Option<RulerMark> {
    match row {
        PreparedRow::Line(l) => l.mark,
        PreparedRow::SplitLine { left, right } => {
            let l = left.as_ref().and_then(|c| c.mark);
            let r = right.as_ref().and_then(|c| c.mark);
            match (l, r) {
                (Some(a), Some(b)) if a == b => Some(a),
                (Some(_), Some(_)) => Some(RulerMark::Mixed),
                (Some(m), None) | (None, Some(m)) => Some(m),
                (None, None) => None,
            }
        }
        _ => None,
    }
}

/// Build the overview-ruler runs: the change rows' positions as 0..1
/// fractions down the body, coalesced into contiguous same-kind runs so the
/// ruler paints one bar per change block (not per line). Computed once per
/// prepared rebuild, NOT per frame.
pub fn overview_runs(rows: &[PreparedRow]) -> Vec<OverviewRun> {
    let total = rows.len();
    if total == 0 {
        return Vec::new();
    }
    let total_f = total as f32;
    let mut runs: Vec<OverviewRun> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(mark) = row_mark(row) else { continue };
        let start = i as f32 / total_f;
        let end = (i + 1) as f32 / total_f;
        // Extend the open run when this row abuts it with the same kind;
        // a context row (or kind change) in between starts a fresh bar.
        if let Some(last) = runs.last_mut()
            && last.mark == mark
            && (last.end - start).abs() < f32::EPSILON
        {
            last.end = end;
            continue;
        }
        runs.push(OverviewRun { start, end, mark });
    }
    runs
}

/// Paint the overview ruler: a thin strip on the body's right edge with a
/// colored bar at the relative position of each change run (green add / red
/// remove / amber mixed) — a glance shows where the diff's changes sit. The
/// host stacks this absolutely over the right edge of the scrollable body
/// (which has no platform scrollbar). Each run is clickable: it scrolls the
/// body to the first row of that change block (`start` fraction → row).
pub fn overview_ruler(
    runs: &[OverviewRun],
    total_rows: usize,
    theme: &Theme,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    const RULER_W: f32 = 5.0;
    let mut ruler = div()
        .absolute()
        .top(px(0.0))
        .right(px(0.0))
        .h_full()
        .w(px(RULER_W))
        .overflow_hidden();
    for (i, run) in runs.iter().enumerate() {
        let base = match run.mark {
            RulerMark::Added => theme.git.added,
            RulerMark::Removed => theme.git.deleted,
            RulerMark::Mixed => theme.status_warn,
        };
        // Target row = run start fraction mapped back onto the row count.
        let target = ((run.start * total_rows as f32) as usize).min(total_rows.saturating_sub(1));
        let weak = weak.clone();
        ruler = ruler.child(
            div()
                .id(gpui::ElementId::Name(format!("diff-ruler-run-{i}").into()))
                .absolute()
                .right(px(0.0))
                .w_full()
                .top(relative(run.start))
                .h(relative((run.end - run.start).max(0.0)))
                // Keep a single-line change visible even when the body is
                // tall enough that its fractional height rounds below a pixel.
                .min_h(px(2.0))
                .cursor_pointer()
                .bg(Hsla { a: 0.85, ..base })
                .on_click(move |_: &ClickEvent, _window, cx: &mut App| {
                    let _ = weak.update(cx, |view, cx| {
                        view.scroll_to_row(target);
                        cx.notify();
                    });
                }),
        );
    }
    ruler
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
    collapsed: &std::collections::HashSet<usize>,
    rctx: &RenderCtx<'_>,
) -> Vec<PreparedRow> {
    let gutter_digits = gutter_digits_for(plan);

    let mut rows = Vec::new();
    for (file_idx, fp) in plan.iter().enumerate() {
        let regions: &[oximux_core::ChangeRegion] = regions_per_file
            .get(file_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // A folded file emits its header only — the body (and the syntect /
        // word-diff work behind it) is skipped entirely until the user
        // unfolds it.
        let folded = collapsed.contains(&file_idx);
        match fp {
            FilePlan::Hunked {
                path,
                header,
                hunks,
                added,
                removed,
            } => {
                rows.push(PreparedRow::FileHeader {
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: Some((*added, *removed)),
                    folded,
                });
                if folded {
                    continue;
                }
                // Walk every line in file order; before the first changed
                // line of each region, dock that region's chip bar. Regions
                // are anchored by line number (robust to hunk merging) and
                // matched sequentially since both lists are in file order.
                let mut next_region = 0usize;
                // The most recently docked region — every line until the next
                // header is tagged with it so its gutter sliver widens when
                // the region's header is hovered. Context lines carry no
                // strip, so a "wrong" tag on surrounding context is inert.
                let mut current_region: Option<usize> = None;
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
                            current_region = Some(next_region);
                            next_region += 1;
                        }
                        let strip = line_change_strip(l.kind, rctx);
                        let region = current_region.map(|r| (file_idx, r));
                        rows.push(PreparedRow::Line(prepare_line(
                            l,
                            strip,
                            gutter_digits,
                            region,
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
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                    folded,
                });
                if folded {
                    continue;
                }
                rows.push(PreparedRow::Collapsed {
                    label: format!(
                        "Large diff: {hunk_count} hunks, {total_lines} lines — click to expand"
                    ),
                });
            }
            FilePlan::Binary { path, header } => {
                rows.push(PreparedRow::FileHeader {
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                    folded,
                });
                if folded {
                    continue;
                }
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
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                    folded,
                });
                if folded {
                    continue;
                }
                rows.push(PreparedRow::Special {
                    text: format!("Mode change only: {old_mode:o} → {new_mode:o}"),
                });
            }
        }
    }
    rows
}

/// Gutter width auto-fits the largest line number across the WHOLE body so
/// the gutter/content divider stays aligned across every file and hunk (no
/// per-hunk horizontal shift). Shared by inline + split builders.
fn gutter_digits_for(plan: &[FilePlan]) -> usize {
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
    digit_count(max_line)
}

/// Side-by-side variant of [`prepare`]. Re-uses the same file/region headers
/// and special bodies, but renders the body as `SplitLine` rows: original
/// (left) | modified (right). Within each change block, removed lines align
/// top-down against added lines, with a `None` filler where one side is
/// longer. Context lines appear on both sides. Region chip headers and the
/// large-diff / binary / mode-only affordances are emitted identically to
/// inline so staging + hover behavior is unchanged.
pub fn prepare_split(
    plan: &[FilePlan],
    regions_per_file: &[Vec<oximux_core::ChangeRegion>],
    collapsed: &std::collections::HashSet<usize>,
    rctx: &RenderCtx<'_>,
) -> Vec<PreparedRow> {
    let gutter_digits = gutter_digits_for(plan);
    let mut rows = Vec::new();
    for (file_idx, fp) in plan.iter().enumerate() {
        let regions: &[oximux_core::ChangeRegion] = regions_per_file
            .get(file_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let folded = collapsed.contains(&file_idx);
        match fp {
            FilePlan::Hunked {
                path,
                header,
                hunks,
                added,
                removed,
            } => {
                rows.push(PreparedRow::FileHeader {
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: Some((*added, *removed)),
                    folded,
                });
                if folded {
                    continue;
                }
                let mut next_region = 0usize;
                // Buffers for the current change block (consecutive non-
                // context lines); flushed — removed aligned against added —
                // at the next context line or hunk end. A region's `@@`
                // header docks at the START of its block (not on the anchor
                // line, which sits between the removed and added halves of a
                // modify and would split the pair across filler rows).
                let mut rem: Vec<&LinePlan> = Vec::new();
                let mut add: Vec<&LinePlan> = Vec::new();
                let mut in_block = false;
                for h in hunks.iter() {
                    for l in &h.rows {
                        match l.kind {
                            DiffLineKind::NoNewlineHint => {}
                            DiffLineKind::Context => {
                                flush_split_block(
                                    &mut rows,
                                    &mut rem,
                                    &mut add,
                                    gutter_digits,
                                    rctx,
                                );
                                in_block = false;
                                rows.push(PreparedRow::SplitLine {
                                    left: Some(side_cell(l, true, gutter_digits, rctx)),
                                    right: Some(side_cell(l, false, gutter_digits, rctx)),
                                });
                            }
                            DiffLineKind::Removed | DiffLineKind::Added => {
                                // First changed line of a block → dock the
                                // region header above the whole block. Blocks
                                // and regions are 1:1 in file order (a region
                                // is a contiguous change run), so the running
                                // region index stays correct for staging.
                                if !in_block {
                                    if let Some(r) = regions.get(next_region) {
                                        rows.push(PreparedRow::HunkHeader {
                                            file_idx,
                                            hunk_idx: next_region,
                                            header: region_label(r).into(),
                                            strip: region_strip(r, rctx),
                                        });
                                        next_region += 1;
                                    }
                                    in_block = true;
                                }
                                if matches!(l.kind, DiffLineKind::Removed) {
                                    rem.push(l);
                                } else {
                                    add.push(l);
                                }
                            }
                        }
                    }
                }
                flush_split_block(&mut rows, &mut rem, &mut add, gutter_digits, rctx);
            }
            FilePlan::Collapsed {
                path,
                header,
                total_lines,
                hunk_count,
            } => {
                rows.push(PreparedRow::FileHeader {
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                    folded,
                });
                if folded {
                    continue;
                }
                rows.push(PreparedRow::Collapsed {
                    label: format!(
                        "Large diff: {hunk_count} hunks, {total_lines} lines — click to expand"
                    ),
                });
            }
            FilePlan::Binary { path, header } => {
                rows.push(PreparedRow::FileHeader {
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                    folded,
                });
                if folded {
                    continue;
                }
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
                    file_idx,
                    path: path.clone().into(),
                    label: header.label.clone(),
                    stats: None,
                    folded,
                });
                if folded {
                    continue;
                }
                rows.push(PreparedRow::Special {
                    text: format!("Mode change only: {old_mode:o} → {new_mode:o}"),
                });
            }
        }
    }
    rows
}

/// Emit the buffered change block as aligned `SplitLine` rows: removed[i] on
/// the left, added[i] on the right, `None` filler where one side is shorter.
/// Clears both buffers. No-op when both are empty.
fn flush_split_block(
    rows: &mut Vec<PreparedRow>,
    rem: &mut Vec<&LinePlan>,
    add: &mut Vec<&LinePlan>,
    gutter_digits: usize,
    rctx: &RenderCtx<'_>,
) {
    let n = rem.len().max(add.len());
    for i in 0..n {
        rows.push(PreparedRow::SplitLine {
            left: rem.get(i).map(|l| side_cell(l, true, gutter_digits, rctx)),
            right: add.get(i).map(|l| side_cell(l, false, gutter_digits, rctx)),
        });
    }
    rem.clear();
    add.clear();
}

/// Build one column of a `SplitLine`. `is_left` selects the old-side gutter
/// (and is otherwise visually identical to the inline line — same tint,
/// sliver, syntax + word-diff highlights).
fn side_cell(l: &LinePlan, is_left: bool, gutter_digits: usize, rctx: &RenderCtx<'_>) -> SideCell {
    let (fg, bg) = line_visuals(l.kind, rctx);
    let line_no = if is_left { l.old_line } else { l.new_line };
    SideCell {
        gutter: pack_gutter_cell(line_no, gutter_digits),
        content: l.content.clone().into(),
        highlights: line_highlights(l, rctx),
        fg,
        bg,
        strip: line_change_strip(l.kind, rctx),
        mark: mark_for_kind(l.kind),
    }
}

/// Overview-ruler classification for a diff line kind — only adds/removes
/// put a tick on the ruler; context and the no-newline hint do not.
fn mark_for_kind(kind: DiffLineKind) -> Option<RulerMark> {
    match kind {
        DiffLineKind::Added => Some(RulerMark::Added),
        DiffLineKind::Removed => Some(RulerMark::Removed),
        _ => None,
    }
}

/// Per-kind (foreground, tint) for a diff body line. The row tint + gutter
/// sliver carry "added/removed"; the text keeps a normal foreground so syntax
/// reads through and plain-text rows don't wash to solid green/red. Only
/// context is dimmed to push the eye toward changed lines. Shared by the
/// inline (`prepare_line`) and split (`side_cell`) builders.
fn line_visuals(kind: DiffLineKind, rctx: &RenderCtx<'_>) -> (Hsla, Option<Hsla>) {
    match kind {
        DiffLineKind::Context => (rctx.theme.fg_muted, None),
        DiffLineKind::Added => (rctx.theme.fg_base, Some(rctx.theme.diff_added_bg())),
        DiffLineKind::Removed => (rctx.theme.fg_base, Some(rctx.theme.diff_removed_bg())),
        DiffLineKind::NoNewlineHint => (rctx.theme.fg_subtle, None),
    }
}

/// Resolve one plan line into a fully-baked `PreparedLine`.
fn prepare_line(
    l: &LinePlan,
    strip: Option<Hsla>,
    gutter_digits: usize,
    region: Option<(usize, usize)>,
    rctx: &RenderCtx<'_>,
) -> PreparedLine {
    let (fg, row_bg) = line_visuals(l.kind, rctx);
    PreparedLine {
        content: l.content.clone().into(),
        highlights: line_highlights(l, rctx),
        old_cell: pack_gutter_cell(l.old_line, gutter_digits),
        new_cell: pack_gutter_cell(l.new_line, gutter_digits),
        fg,
        row_bg,
        strip,
        region,
        mark: mark_for_kind(l.kind),
    }
}

/// Build the layered byte-range highlights for one line:
///
///   1. **Syntax foreground** — every syntect token paints its `color`, on
///      ALL line kinds. A changed line keeps full syntax coloring.
///   2. **Word-diff background** — on paired Added/Removed rows, each
///      `Insert`/`Delete` span gets a brighter `background_color` (over the
///      preserved syntax fg) so the exact changed words pop. `Same` spans
///      add nothing — the line tint already covers them.
///   3. **Fallback** (Unknown language / blank): empty → inherited row fg.
///
/// Foreground and background ranges compose on a single shaped run because
/// GPUI applies `color` and `background_color` independently per range.
fn line_highlights(l: &LinePlan, rctx: &RenderCtx<'_>) -> Vec<(Range<usize>, HighlightStyle)> {
    let content = l.content.as_str();
    let max = content.len();
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();

    // 1. Syntax foreground tokens (always).
    for tok in &l.tokens {
        let start = tok.start.min(max);
        let end = tok.end.min(max);
        if start >= end || !content.is_char_boundary(start) || !content.is_char_boundary(end) {
            continue;
        }
        let color = Hsla::from(gpui::Rgba {
            r: tok.r as f32 / 255.0,
            g: tok.g as f32 / 255.0,
            b: tok.b as f32 / 255.0,
            a: 1.0,
        });
        out.push((start..end, fg_highlight(color)));
    }

    // 2. Word-diff backgrounds on paired Added/Removed rows.
    if let Some(spans) = l.spans.as_ref()
        && matches!(l.kind, DiffLineKind::Added | DiffLineKind::Removed)
    {
        let mut off = 0usize;
        for span in spans {
            let len = span.text.len();
            let end = (off + len).min(max);
            let bg = match span.op {
                WordOp::Insert => Some(rctx.theme.diff_word_added_bg()),
                WordOp::Delete => Some(rctx.theme.diff_word_removed_bg()),
                WordOp::Same => None,
            };
            if let Some(bg) = bg
                && off < end
                && content.is_char_boundary(off)
                && content.is_char_boundary(end)
            {
                out.push((off..end, bg_highlight(bg)));
            }
            off += len;
        }
    }

    out
}

fn fg_highlight(color: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(color),
        ..Default::default()
    }
}

fn bg_highlight(color: Hsla) -> HighlightStyle {
    HighlightStyle {
        background_color: Some(color),
        ..Default::default()
    }
}

/// Render the prepared rows into the virtualized body. Called from
/// `DiffView::render` with the cached `Rc<Vec<PreparedRow>>`.
///
/// `side` gates the per-hunk action chips (Stage/Unstage/Discard) — `None`
/// on commit-detail / read-only views. `weak` routes chip + expand + copy
/// clicks back into the view from the `uniform_list` closure's App scope.
#[allow(clippy::too_many_arguments)]
pub fn render_rows(
    rows: Rc<Vec<PreparedRow>>,
    side: Option<HunkActionSide>,
    hovered_region: Option<(usize, usize)>,
    widest_idx: usize,
    split: bool,
    h_offset: f32,
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
    let list = uniform_list(
        "diff-view-rows",
        rows.len(),
        move |range, _window, _cx| {
            range
                .filter_map(|i| rows.get(i))
                .map(|row| {
                    build_prepared_row(
                        row,
                        side,
                        hovered_region,
                        h_offset,
                        theme,
                        density,
                        &typography,
                        weak.clone(),
                    )
                })
                .collect()
        },
    );
    // Inline mode: long lines scroll horizontally instead of clipping —
    // `Unconstrained` lays every row at the widest item's width and turns on
    // x-overflow scroll; `with_width_from_item` points the measurement at
    // that widest row so the scroll range is exact. Split mode keeps the two
    // columns inside the viewport (`FitList`, the default) and clips per half.
    let list = if split {
        list
    } else {
        list.with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(Some(widest_idx))
    };
    list
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
#[allow(clippy::too_many_arguments)]
fn build_prepared_row(
    row: &PreparedRow,
    side: Option<HunkActionSide>,
    hovered_region: Option<(usize, usize)>,
    h_offset: f32,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> gpui::AnyElement {
    match row {
        PreparedRow::FileHeader {
            file_idx,
            path,
            label,
            stats,
            folded,
        } => file_header_row(
            *file_idx,
            path.clone(),
            label.clone(),
            *stats,
            *folded,
            false,
            theme,
            density,
            typography,
            weak,
        )
        .into_any_element(),
        PreparedRow::HunkHeader {
            file_idx,
            hunk_idx,
            header,
            strip,
        } => {
            let hovered = hovered_region == Some((*file_idx, *hunk_idx));
            hunk_header_row(
                *file_idx, *hunk_idx, header.clone(), *strip, hovered, side, theme, density,
                typography, weak,
            )
            .into_any_element()
        }
        PreparedRow::Line(line) => {
            let hovered = line.region.is_some() && line.region == hovered_region;
            line_row_el(line, hovered, theme, density, typography).into_any_element()
        }
        PreparedRow::SplitLine { left, right } => {
            split_line_el(left.as_ref(), right.as_ref(), h_offset, theme, density, typography)
                .into_any_element()
        }
        PreparedRow::Collapsed { label } => {
            collapsed_row(label.clone(), theme, density, typography, weak).into_any_element()
        }
        PreparedRow::Special { text } => {
            special_row(text.clone(), theme, density, typography).into_any_element()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn hunk_header_row(
    file_idx: usize,
    hunk_idx: usize,
    header: SharedString,
    strip: Option<Hsla>,
    hovered: bool,
    side: Option<HunkActionSide>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    // The action chips are revealed only while the pointer is over this
    // region's header — at rest the header shows just the `@@` label, so a
    // multi-region diff stays calm instead of docking a chip bar per region.
    let actions = side
        .filter(|_| hovered)
        .and_then(|s| render_hunk_actions(s, file_idx, hunk_idx, theme, density, typography, &weak));
    // Drive the hover state from the header row. `on_hover` fires true on
    // enter, false on leave; the card + sliver-widen both key off it. The
    // chips card is a child of this row, so moving onto it stays "hovered".
    let hover_weak = weak.clone();
    let mut row = div()
        .id(gpui::ElementId::Name(
            format!("diff-hunk-header-{file_idx}-{hunk_idx}").into(),
        ))
        .flex()
        .items_center()
        .h(px(density.h_row))
        .w_full()
        .bg(theme.bg_panel_alt)
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .on_hover(move |hovered, _window, cx: &mut App| {
            let region = hovered.then_some((file_idx, hunk_idx));
            let _ = hover_weak.update(cx, |view, cx| view.set_hovered_region(region, cx));
        })
        .child(strip_cell(strip, hovered, density.h_row))
        .child(div().px(px(density.pad_panel)).child(header));
    if let Some(actions) = actions {
        // Float the chips as an elevated card at the right of the header —
        // raised surface + crisp border + soft shadow so it reads as a
        // floating control docked over the diff, not a flat inline chip.
        let card = div()
            .flex()
            .flex_row()
            .items_center()
            .mr(px(density.pad_panel))
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(density.r_xs))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .shadow_md()
            .child(actions);
        row = row.child(div().flex_1()).child(card);
    }
    row
}

/// One diff body line: strip cell + dual gutter + a SINGLE `StyledText`
/// for the content (colors via highlight ranges, not child elements).
/// Clean line numbers + tint carry the add/remove cue — no `+`/`-` glyph in
/// the body. The row is `min_w_full` so it always fills the viewport (tint
/// spans full width) but grows past it for long lines, which the list's
/// `Unconstrained` horizontal scroll then reveals.
fn line_row_el(
    line: &PreparedLine,
    hovered: bool,
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

    let content = div()
        .flex()
        .flex_row()
        // Small left pad replaces the old glyph column so content doesn't
        // butt against the gutter.
        .pl(px(density.gap_inline))
        .pr(px(density.pad_panel))
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(line.fg)
        .child(
            StyledText::new(line.content.clone())
                .with_highlights(line.highlights.iter().cloned()),
        );

    let mut row = div()
        .flex()
        .items_center()
        .h(px(density.h_row))
        // `min_w_full` (not `w_full`): fill the viewport so the tint spans
        // edge-to-edge, but allow growth past it so long lines extend into
        // the list's horizontal scroll range instead of clipping.
        .min_w_full()
        .text_size(px(typography.t_body_sm));
    if let Some(bg) = line.row_bg {
        row = row.bg(bg);
    }
    row.child(strip_cell(line.strip, hovered, density.h_row))
        .child(gutter)
        .child(content)
}

/// One side-by-side row: original (left) | 1px divider | modified (right).
/// Each half is a 50% column with its own sliver + single (sticky) gutter +
/// content; `None` halves render as a faint filler so unequal blocks still
/// align. `h_offset` shifts each column's CONTENT left (synced horizontal
/// scroll) while the gutter stays pinned.
fn split_line_el(
    left: Option<&SideCell>,
    right: Option<&SideCell>,
    h_offset: f32,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(density.h_row))
        .w_full()
        .text_size(px(typography.t_body_sm))
        .child(split_half(left, h_offset, theme, density, typography))
        .child(div().w(px(1.0)).h(px(density.h_row)).bg(theme.border_inactive))
        .child(split_half(right, h_offset, theme, density, typography))
}

/// One 50%-width column of a `SplitLine`. `None` → filler (faint wash, no
/// number); `Some` → sliver + sticky gutter + highlighted content shifted by
/// `h_offset` inside an `overflow_hidden` viewport (so long lines scroll
/// rather than clip, with the gutter staying fixed).
fn split_half(
    cell: Option<&SideCell>,
    h_offset: f32,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> gpui::Div {
    let mut half = div()
        .flex()
        .flex_row()
        .items_center()
        .w_1_2()
        .h(px(density.h_row))
        .overflow_hidden();
    let Some(cell) = cell else {
        // Filler: a faint neutral wash signals "no line on this side".
        return half.bg(Hsla {
            a: 0.03,
            ..theme.fg_subtle
        });
    };
    if let Some(bg) = cell.bg {
        half = half.bg(bg);
    }
    // Sliver + gutter stay pinned (do not scroll horizontally).
    let gutter = div()
        .flex_shrink_0()
        .px(px(density.pad_panel))
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(cell.gutter.clone());
    // Content viewport fills the rest of the half and clips; the inner line
    // is absolutely positioned and shifted left by the shared offset to
    // reveal long lines (proven pattern — `relative` clip box + `absolute`
    // negative-`left` content, avoids relying on negative margins).
    let content_viewport = div()
        .relative()
        .flex_1()
        .h(px(density.h_row))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .left(px(density.gap_inline - h_offset))
                .flex()
                .flex_row()
                .items_center()
                .h(px(density.h_row))
                .pr(px(density.pad_panel))
                .font(typography.mono_font())
                .text_size(px(typography.t_body_sm))
                .text_color(cell.fg)
                .child(
                    StyledText::new(cell.content.clone())
                        .with_highlights(cell.highlights.iter().cloned()),
                ),
        );
    half.child(strip_cell(cell.strip, false, density.h_row))
        .child(gutter)
        .child(content_viewport)
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
    // Near-opaque so the sliver stays crisp now that it's the primary
    // (glyph-free) add/remove indicator against the softer line tint.
    let alpha = |c: Hsla| Hsla { a: 0.9, ..c };
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

/// Colored sliver prepended to every row in a hunk. The cell is a fixed
/// 7px-wide column (so the gutter never reflows) with the bar drawn left-
/// aligned inside it: 3px at rest, widening to the full 7px when the row's
/// region is hovered. Transparent bar when `color` is `None` (context rows).
fn strip_cell(color: Option<Hsla>, hovered: bool, h: f32) -> gpui::Div {
    const COL_W: f32 = 7.0;
    let bar_w = if hovered { COL_W } else { 3.0 };
    let mut bar = div().w(px(bar_w)).h(px(h));
    if let Some(c) = color {
        bar = bar.bg(c);
    }
    div().w(px(COL_W)).h(px(h)).child(bar)
}

fn digit_count(n: u32) -> usize {
    // Minimum 2 digits keeps narrow files (≤ 9 lines) from looking cramped.
    n.checked_ilog10()
        .map(|d| d as usize + 1)
        .unwrap_or(0)
        .max(2)
}

/// Build one gutter cell as a right-aligned line number (or blanks when the
/// side has no number, e.g. an addition's old side). No `+`/`-` sign — the
/// row tint + gutter sliver carry the add/remove cue.
fn pack_gutter_cell(line_no: Option<u32>, digits: usize) -> String {
    match line_no {
        Some(n) => format!("{n:>width$}", width = digits),
        None => " ".repeat(digits),
    }
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
