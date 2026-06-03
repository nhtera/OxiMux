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
//! ## Continuous flow + foldable context
//!
//! The body reads as one continuous numbered listing — no `@@` marker rows
//! at rest. Each changed line tints inline; the per-region Stage/Discard
//! card reveals on hover, anchored on the region's first changed line. Long
//! runs of unchanged context collapse behind a `ContextFold` expander
//! (3 lines kept on each side of a change; a run longer than the collapse
//! threshold folds its middle). Folding happens at this prepared-row layer
//! only — `change_regions` still anchors over the full diff, so per-region
//! `git add -p` staging is unaffected.
//!
//! Color resolution, gutter strings, and highlight ranges are all baked
//! once into `PreparedRow` (see `prepare`) so the per-frame render closure
//! does no string or highlight work for off-screen rows. The host
//! (`mod.rs`) caches the prepared vec behind an `Rc` and rebuilds it only
//! when the diff, fold, or `expanded` flag changes — never on scroll.

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

/// Number of unchanged context lines kept on EACH side of a change before a
/// long run is folded.
const CONTEXT_KEEP: usize = 3;
/// An unchanged-context run longer than this collapses its middle behind a
/// single `ContextFold` expander. Runs at or under it render in full.
const COLLAPSE_THRESHOLD: usize = 10;

/// Stable identity for a folded context run: `(file_idx, first-hidden-line)`.
/// Anchored by the new-side (or old-side) line number of the first folded
/// line so the id survives a prepared-row rebuild (row indices shift, line
/// numbers don't). The host tracks which fold ids the user has expanded.
pub type FoldId = (usize, u32);

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
    /// A diff body line, fully resolved (colors, gutter cells, highlight
    /// ranges) so painting it touches no syntect / word-diff machinery.
    Line(PreparedLine),
    /// A side-by-side body row: original (left) | modified (right). Either
    /// side is `None` (filler) where a block is longer on the opposite side.
    /// Only emitted in split mode; inline mode uses `Line`.
    SplitLine {
        file_idx: usize,
        left: Option<SideCell>,
        right: Option<SideCell>,
        /// The change region this row belongs to, as `(file_idx, region_idx)`,
        /// or `None` for context. Drives hover (sliver-widen + staging card).
        region: Option<(usize, usize)>,
        /// True on the first row of a change block — where the floating
        /// Stage/Discard card anchors on hover.
        region_anchor: bool,
    },
    /// Collapsed unchanged-context run. Click expands just this run
    /// (`DiffView::expand_fold`); the body otherwise reads continuously.
    ContextFold {
        file_idx: usize,
        fold_id: FoldId,
        /// Hidden line count, shown as "⋯ N unchanged lines".
        count: usize,
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
    /// True when this cell's file is already staged — the sliver renders
    /// hollow (outlined) instead of solid. Only meaningful in the combined
    /// view where staged + unstaged files coexist; single-file diffs are
    /// uniformly one state so this stays `false`.
    pub hollow: bool,
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
    /// True when this line's file is already staged — the sliver renders
    /// hollow (outlined) instead of solid. Set in a per-file post-pass from
    /// the combined view's group tag; `false` for single-file diffs.
    pub hollow: bool,
    /// The file this line belongs to — disambiguates the row's interactive
    /// id across files and owns the row in the sticky-overlay map.
    pub file_idx: usize,
    /// The change region this line belongs to, as `(file_idx, region_idx)`,
    /// or `None` for context outside any region. Drives the hover-widen of
    /// the gutter sliver + reveals the region's staging card.
    pub region: Option<(usize, usize)>,
    /// True on the region's first changed line — where the floating
    /// Stage/Discard card anchors when the region is hovered.
    pub region_anchor: bool,
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
        .max_by_key(|(_, row)| row_width_chars(row))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Character count of the widest row's content — clamps the split-mode
/// horizontal scroll. For split rows it's the longer of the two columns.
pub fn widest_row_chars(rows: &[PreparedRow]) -> usize {
    rows.iter().map(row_width_chars).max().unwrap_or(0)
}

/// Approximate laid-out width of one prepared row, in content characters.
/// Shared by the widest-index + widest-chars passes.
fn row_width_chars(row: &PreparedRow) -> usize {
    match row {
        PreparedRow::Line(l) => l.content.chars().count(),
        PreparedRow::SplitLine { left, right, .. } => {
            // Either column can be the wider; the body lays the two side
            // by side so the row's effective width tracks the larger one.
            let w = |c: &Option<SideCell>| c.as_ref().map_or(0, |s| s.content.chars().count());
            w(left).max(w(right))
        }
        PreparedRow::FileHeader { path, label, .. } => path.chars().count() + label.chars().count(),
        PreparedRow::ContextFold { count, .. } => count_fold_label(*count).chars().count(),
        PreparedRow::Collapsed { label } => label.chars().count(),
        PreparedRow::Special { text } => text.chars().count(),
    }
}

/// Overview-ruler classification for one prepared row: the change kind to
/// mark on the scrollbar ruler, or `None` for rows that aren't changed body
/// lines (headers, context, fold, collapsed, special). A split row whose two
/// columns are a removed/added pair reads as `Mixed`.
fn row_mark(row: &PreparedRow) -> Option<RulerMark> {
    match row {
        PreparedRow::Line(l) => l.mark,
        PreparedRow::SplitLine { left, right, .. } => {
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
/// Runs once per (diff, fold, expanded) change — NOT per frame.
///
/// `regions_per_file[i]` are the stageable change regions for `plan[i]`
/// (from `oximux_core::change_regions`). Because diffs are fetched with
/// full-file context, the staging card (Stage/Unstage/Discard) anchors at
/// the FIRST changed line of EACH region rather than once per file —
/// restoring `git add -p` granularity over a whole-file view. The card's
/// `region_idx` (carried on the anchor line) is what `DiffView::stage_hunk`
/// maps back through `change_regions` to the matching standalone patch.
///
/// `expanded_folds` are the context runs the user has clicked open; a run
/// whose `FoldId` is present renders in full instead of behind an expander.
///
/// `staged_per_file[i]` (when present) marks `plan[i]` as already staged, so
/// its gutter slivers render hollow instead of solid — the combined view's
/// staged-vs-unstaged cue. An empty slice (single-file diffs) renders every
/// sliver solid.
pub fn prepare(
    plan: &[FilePlan],
    regions_per_file: &[Vec<oximux_core::ChangeRegion>],
    collapsed: &std::collections::HashSet<usize>,
    expanded_folds: &std::collections::HashSet<FoldId>,
    staged_per_file: &[bool],
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
                // Walk every line in file order. Unchanged context is
                // buffered so a long run can fold; the first changed line of
                // each region is tagged as that region's staging anchor.
                // Regions are matched sequentially by line-number anchor
                // (robust to hunk merging) since both lists are in file order.
                let mut next_region = 0usize;
                let mut current_region: Option<usize> = None;
                let mut ctx_buf: Vec<&LinePlan> = Vec::new();
                for h in hunks.iter() {
                    for l in &h.rows {
                        if matches!(l.kind, DiffLineKind::Context) {
                            ctx_buf.push(l);
                            continue;
                        }
                        // A changed (or no-newline) line ends the context run.
                        flush_context_inline(
                            &mut rows,
                            &mut ctx_buf,
                            file_idx,
                            gutter_digits,
                            expanded_folds,
                            rctx,
                        );
                        let is_anchor = next_region < regions.len()
                            && line_matches_anchor(l, &regions[next_region]);
                        if is_anchor {
                            current_region = Some(next_region);
                            next_region += 1;
                        }
                        // Only changed lines carry the region tag (context
                        // between regions clears it, so hovering context
                        // doesn't keep a stale staging card open).
                        let region = if matches!(l.kind, DiffLineKind::Added | DiffLineKind::Removed)
                        {
                            current_region.map(|r| (file_idx, r))
                        } else {
                            None
                        };
                        let strip = line_change_strip(l.kind, rctx);
                        rows.push(PreparedRow::Line(prepare_line(
                            l,
                            strip,
                            gutter_digits,
                            file_idx,
                            region,
                            is_anchor,
                            rctx,
                        )));
                    }
                    // Don't carry a context run across a hunk boundary.
                    flush_context_inline(
                        &mut rows,
                        &mut ctx_buf,
                        file_idx,
                        gutter_digits,
                        expanded_folds,
                        rctx,
                    );
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
    apply_staged_slivers(&mut rows, staged_per_file);
    rows
}

/// Flip every changed line's sliver to hollow for files the combined view
/// has tagged staged. Runs once after the row list is built — the staged
/// flag is uniform per file, so this avoids threading it through every
/// per-line/per-block builder. No-op when nothing is staged (single-file
/// diffs pass an empty slice).
fn apply_staged_slivers(rows: &mut [PreparedRow], staged_per_file: &[bool]) {
    if !staged_per_file.iter().any(|&s| s) {
        return;
    }
    // An empty slice (single-file views) skips this whole pass; a non-empty
    // one must run parallel to the plan or trailing files silently fall back
    // to solid. Caught in dev, free in release.
    debug_assert!(
        staged_per_file.len()
            >= rows
                .iter()
                .filter_map(|r| match r {
                    PreparedRow::Line(l) => Some(l.file_idx),
                    PreparedRow::SplitLine { file_idx, .. } => Some(*file_idx),
                    _ => None,
                })
                .max()
                .map_or(0, |m| m + 1),
        "staged_per_file must span every file index in the row list",
    );
    let staged = |idx: usize| staged_per_file.get(idx).copied().unwrap_or(false);
    for row in rows.iter_mut() {
        match row {
            PreparedRow::Line(line) if staged(line.file_idx) => line.hollow = true,
            PreparedRow::SplitLine {
                file_idx,
                left,
                right,
                ..
            } if staged(*file_idx) => {
                if let Some(cell) = left {
                    cell.hollow = true;
                }
                if let Some(cell) = right {
                    cell.hollow = true;
                }
            }
            _ => {}
        }
    }
}

/// Emit the buffered run of unchanged context as `Line` rows, folding the
/// middle behind a `ContextFold` when the run is longer than the threshold
/// and the user hasn't expanded it. Clears the buffer.
fn flush_context_inline(
    rows: &mut Vec<PreparedRow>,
    buf: &mut Vec<&LinePlan>,
    file_idx: usize,
    gutter_digits: usize,
    expanded_folds: &std::collections::HashSet<FoldId>,
    rctx: &RenderCtx<'_>,
) {
    let n = buf.len();
    if n == 0 {
        return;
    }
    if n > COLLAPSE_THRESHOLD {
        let fold_id = context_fold_id(file_idx, buf[CONTEXT_KEEP]);
        if !expanded_folds.contains(&fold_id) {
            for l in &buf[..CONTEXT_KEEP] {
                rows.push(context_line(l, gutter_digits, file_idx, rctx));
            }
            rows.push(PreparedRow::ContextFold {
                file_idx,
                fold_id,
                count: n - 2 * CONTEXT_KEEP,
            });
            for l in &buf[n - CONTEXT_KEEP..] {
                rows.push(context_line(l, gutter_digits, file_idx, rctx));
            }
            buf.clear();
            return;
        }
    }
    for l in buf.iter() {
        rows.push(context_line(l, gutter_digits, file_idx, rctx));
    }
    buf.clear();
}

/// One unchanged-context inline row (no strip, no region tag).
fn context_line(
    l: &LinePlan,
    gutter_digits: usize,
    file_idx: usize,
    rctx: &RenderCtx<'_>,
) -> PreparedRow {
    PreparedRow::Line(prepare_line(
        l,
        None,
        gutter_digits,
        file_idx,
        None,
        false,
        rctx,
    ))
}

/// `FoldId` for a context run, anchored by the new-side (or old-side) line
/// number of its first hidden line so the id is stable across rebuilds.
fn context_fold_id(file_idx: usize, first_hidden: &LinePlan) -> FoldId {
    let line = first_hidden
        .new_line
        .or(first_hidden.old_line)
        .unwrap_or(0);
    (file_idx, line)
}

fn count_fold_label(count: usize) -> String {
    format!("⋯ {count} unchanged lines")
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

/// Side-by-side variant of [`prepare`]. Re-uses the same file headers, fold
/// expanders, and special bodies, but renders the body as `SplitLine` rows:
/// original (left) | modified (right). Within each change block, removed
/// lines align top-down against added lines, with a `None` filler where one
/// side is longer. Context lines appear on both sides and fold identically
/// to inline. The change block's first row carries the region's staging
/// anchor so Stage/Unstage/Discard works unchanged.
pub fn prepare_split(
    plan: &[FilePlan],
    regions_per_file: &[Vec<oximux_core::ChangeRegion>],
    collapsed: &std::collections::HashSet<usize>,
    expanded_folds: &std::collections::HashSet<FoldId>,
    staged_per_file: &[bool],
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
                // Buffers for the current change block (consecutive non-
                // context lines); flushed — removed aligned against added —
                // at the next context line or hunk end. Context runs buffer
                // separately so a long run can fold. Region assignment mirrors
                // the inline walk EXACTLY (anchor-matched + carried), so split
                // region indices stay aligned with `change_regions` even when
                // two change clusters within `2*HUNK_CONTEXT` context lines
                // merge into one region — a block-counting scheme would drift
                // and strand later blocks with no region (no staging card).
                let mut next_region = 0usize;
                let mut current_region: Option<usize> = None;
                let mut rem: Vec<&LinePlan> = Vec::new();
                let mut add: Vec<&LinePlan> = Vec::new();
                let mut in_block = false;
                let mut block_region: Option<usize> = None;
                // True when the open block is its region's FIRST cluster (the
                // card anchors here); false for a later cluster of a merged
                // region — which still carries the tag for slivers + hover.
                let mut block_anchor = false;
                let mut ctx_buf: Vec<&LinePlan> = Vec::new();
                for h in hunks.iter() {
                    for l in &h.rows {
                        match l.kind {
                            // Stripped upstream by `collapse_no_newline_eof`
                            // (no body row); handled defensively in case one
                            // survives — it neither folds nor anchors.
                            DiffLineKind::NoNewlineHint => {}
                            DiffLineKind::Context => {
                                if in_block {
                                    flush_split_block(
                                        &mut rows,
                                        &mut rem,
                                        &mut add,
                                        file_idx,
                                        block_region.map(|r| (file_idx, r)),
                                        block_anchor,
                                        gutter_digits,
                                        rctx,
                                    );
                                    in_block = false;
                                }
                                ctx_buf.push(l);
                            }
                            DiffLineKind::Removed | DiffLineKind::Added => {
                                flush_context_split(
                                    &mut rows,
                                    &mut ctx_buf,
                                    file_idx,
                                    gutter_digits,
                                    expanded_folds,
                                    rctx,
                                );
                                // A changed line matching the next region's
                                // anchor opens that region; otherwise it belongs
                                // to the current (possibly merged) region.
                                let is_region_start = next_region < regions.len()
                                    && line_matches_anchor(l, &regions[next_region]);
                                if is_region_start {
                                    current_region = Some(next_region);
                                    next_region += 1;
                                }
                                if !in_block {
                                    block_region = current_region;
                                    block_anchor = is_region_start;
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
                    // Don't carry a change block or context run across a hunk
                    // boundary (mirrors the inline walk). At most one buffer
                    // is non-empty here.
                    flush_split_block(
                        &mut rows,
                        &mut rem,
                        &mut add,
                        file_idx,
                        block_region.map(|r| (file_idx, r)),
                        block_anchor,
                        gutter_digits,
                        rctx,
                    );
                    in_block = false;
                    flush_context_split(
                        &mut rows,
                        &mut ctx_buf,
                        file_idx,
                        gutter_digits,
                        expanded_folds,
                        rctx,
                    );
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
    apply_staged_slivers(&mut rows, staged_per_file);
    rows
}

/// Emit the buffered change block as aligned `SplitLine` rows: removed[i] on
/// the left, added[i] on the right, `None` filler where one side is shorter.
/// `region_anchor` marks the block's first row as the region's staging anchor
/// — but ONLY when this block actually starts its region (`block_anchor`);
/// a block that is a later cluster of a merged region carries the region tag
/// (so its slivers + hover resolve) without re-anchoring the card. Clears
/// both buffers. No-op when both are empty.
#[allow(clippy::too_many_arguments)]
fn flush_split_block(
    rows: &mut Vec<PreparedRow>,
    rem: &mut Vec<&LinePlan>,
    add: &mut Vec<&LinePlan>,
    file_idx: usize,
    region: Option<(usize, usize)>,
    block_anchor: bool,
    gutter_digits: usize,
    rctx: &RenderCtx<'_>,
) {
    let n = rem.len().max(add.len());
    for i in 0..n {
        rows.push(PreparedRow::SplitLine {
            file_idx,
            left: rem.get(i).map(|l| side_cell(l, true, gutter_digits, rctx)),
            right: add.get(i).map(|l| side_cell(l, false, gutter_digits, rctx)),
            region,
            region_anchor: block_anchor && i == 0,
        });
    }
    rem.clear();
    add.clear();
}

/// Emit the buffered context run as `SplitLine` rows (both columns filled),
/// folding the middle behind a `ContextFold` when long + not expanded.
fn flush_context_split(
    rows: &mut Vec<PreparedRow>,
    buf: &mut Vec<&LinePlan>,
    file_idx: usize,
    gutter_digits: usize,
    expanded_folds: &std::collections::HashSet<FoldId>,
    rctx: &RenderCtx<'_>,
) {
    let n = buf.len();
    if n == 0 {
        return;
    }
    if n > COLLAPSE_THRESHOLD {
        let fold_id = context_fold_id(file_idx, buf[CONTEXT_KEEP]);
        if !expanded_folds.contains(&fold_id) {
            for l in &buf[..CONTEXT_KEEP] {
                rows.push(context_split_row(l, file_idx, gutter_digits, rctx));
            }
            rows.push(PreparedRow::ContextFold {
                file_idx,
                fold_id,
                count: n - 2 * CONTEXT_KEEP,
            });
            for l in &buf[n - CONTEXT_KEEP..] {
                rows.push(context_split_row(l, file_idx, gutter_digits, rctx));
            }
            buf.clear();
            return;
        }
    }
    for l in buf.iter() {
        rows.push(context_split_row(l, file_idx, gutter_digits, rctx));
    }
    buf.clear();
}

/// One unchanged-context split row (both columns the same line, no region).
fn context_split_row(
    l: &LinePlan,
    file_idx: usize,
    gutter_digits: usize,
    rctx: &RenderCtx<'_>,
) -> PreparedRow {
    PreparedRow::SplitLine {
        file_idx,
        left: Some(side_cell(l, true, gutter_digits, rctx)),
        right: Some(side_cell(l, false, gutter_digits, rctx)),
        region: None,
        region_anchor: false,
    }
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
        hollow: false,
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
#[allow(clippy::too_many_arguments)]
fn prepare_line(
    l: &LinePlan,
    strip: Option<Hsla>,
    gutter_digits: usize,
    file_idx: usize,
    region: Option<(usize, usize)>,
    region_anchor: bool,
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
        hollow: false,
        file_idx,
        region,
        region_anchor,
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

    // Effective row background for a tinted (Added/Removed) row — the
    // translucent tint composited over the base — so syntax tokens that
    // would wash out against it can be nudged back to legible contrast.
    let eff_bg = match l.kind {
        DiffLineKind::Added => Some(composite_over(rctx.theme.diff_added_bg(), rctx.theme.bg_base)),
        DiffLineKind::Removed => {
            Some(composite_over(rctx.theme.diff_removed_bg(), rctx.theme.bg_base))
        }
        _ => None,
    };

    // 1. Syntax foreground tokens (always). On tinted rows, contrast-protect
    //    each token so it stays legible over the wash.
    for tok in &l.tokens {
        let start = tok.start.min(max);
        let end = tok.end.min(max);
        if start >= end || !content.is_char_boundary(start) || !content.is_char_boundary(end) {
            continue;
        }
        let mut color = Hsla::from(gpui::Rgba {
            r: tok.r as f32 / 255.0,
            g: tok.g as f32 / 255.0,
            b: tok.b as f32 / 255.0,
            a: 1.0,
        });
        if let Some(bg) = eff_bg {
            color = ensure_minimum_contrast(color, bg, MIN_TOKEN_CONTRAST);
        }
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

/// Minimum WCAG contrast ratio a syntax token must keep against a tinted
/// row background before its lightness is nudged. ~3:1 keeps colored tokens
/// legible without flattening them to white.
const MIN_TOKEN_CONTRAST: f32 = 3.0;

/// Alpha-composite a translucent `top` over an opaque `base`, returning the
/// resulting opaque color — used to resolve a diff row's effective on-screen
/// background (translucent tint over the panel base) for contrast checks.
fn composite_over(top: Hsla, base: Hsla) -> Hsla {
    let t = gpui::Rgba::from(top);
    let b = gpui::Rgba::from(base);
    // Callers pass the opaque panel/base color; the blend below assumes it.
    debug_assert!((b.a - 1.0).abs() < 1e-4, "composite_over base must be opaque");
    let a = t.a;
    Hsla::from(gpui::Rgba {
        r: t.r * a + b.r * (1.0 - a),
        g: t.g * a + b.g * (1.0 - a),
        b: t.b * a + b.b * (1.0 - a),
        a: 1.0,
    })
}

/// Linearize one sRGB channel (gamma-expand) for relative-luminance, per the
/// WCAG 2.x definition (normative `0.04045` knee).
fn channel_lin(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance of a color.
fn relative_luminance(c: Hsla) -> f32 {
    let r = gpui::Rgba::from(c);
    0.2126 * channel_lin(r.r) + 0.7152 * channel_lin(r.g) + 0.0722 * channel_lin(r.b)
}

/// WCAG contrast ratio between two colors (≥ 1.0).
fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Nudge a syntax token's lightness until it clears `min` contrast against
/// the (composited) row background, preserving hue + saturation so the token
/// keeps its syntactic color. Pushes toward white on dark backgrounds, black
/// on light ones; stops at the first passing step or a lightness bound.
fn ensure_minimum_contrast(fg: Hsla, bg: Hsla, min: f32) -> Hsla {
    if contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let step = if relative_luminance(bg) < 0.5 { 0.04 } else { -0.04 };
    let mut out = fg;
    // 26 steps of 0.04 span the full [0,1] lightness range, so the loop can
    // always reach the brightest/darkest end before giving up.
    for _ in 0..26 {
        let next = (out.l + step).clamp(0.0, 1.0);
        if (next - out.l).abs() < f32::EPSILON {
            break; // hit a clamp bound — can't push further
        }
        out.l = next;
        if contrast_ratio(out, bg) >= min {
            break;
        }
    }
    out
}

/// Render the prepared rows into the virtualized body. Called from
/// `DiffView::render` with the cached `Rc<Vec<PreparedRow>>`.
///
/// The per-region staging card is NOT rendered here — it floats at the
/// body-wrap level (see `staging_card_overlay`) so it stays pinned to the
/// viewport regardless of line length / horizontal scroll. `weak` routes
/// hover + fold + expand + copy clicks back into the view from the
/// `uniform_list` closure's App scope.
#[allow(clippy::too_many_arguments)]
pub fn render_rows(
    rows: Rc<Vec<PreparedRow>>,
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
                .filter_map(|i| rows.get(i).map(|row| (i, row)))
                .map(|(i, row)| {
                    build_prepared_row(
                        i,
                        row,
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
    row_index: usize,
    row: &PreparedRow,
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
        PreparedRow::Line(line) => {
            let hovered = line.region.is_some() && line.region == hovered_region;
            line_row_el(row_index, line, hovered, theme, density, typography, weak)
                .into_any_element()
        }
        PreparedRow::SplitLine {
            file_idx,
            left,
            right,
            region,
            region_anchor: _,
        } => {
            let hovered = region.is_some() && *region == hovered_region;
            split_line_el(
                row_index,
                *file_idx,
                left.as_ref(),
                right.as_ref(),
                *region,
                hovered,
                h_offset,
                theme,
                density,
                typography,
                weak,
            )
            .into_any_element()
        }
        PreparedRow::ContextFold {
            file_idx,
            fold_id,
            count,
        } => context_fold_row(*file_idx, *fold_id, *count, theme, density, typography, weak)
            .into_any_element(),
        PreparedRow::Collapsed { label } => {
            collapsed_row(label.clone(), theme, density, typography, weak).into_any_element()
        }
        PreparedRow::Special { text } => {
            special_row(text.clone(), theme, density, typography).into_any_element()
        }
    }
}

/// One diff body line: strip cell + dual gutter + a SINGLE `StyledText`
/// for the content (colors via highlight ranges, not child elements).
/// Clean line numbers + tint carry the add/remove cue. The row is
/// `min_w_full` so it always fills the viewport (tint spans full width) but
/// grows past it for long lines, which the list's `Unconstrained` horizontal
/// scroll then reveals.
///
/// Every body row carries enter-driven hover wiring: entering a changed line
/// arms its region (`set_hovered_region`), widening every sliver in the
/// region and floating the Stage/Discard card (rendered separately at the
/// body-wrap level — see `staging_card_overlay`); entering context disarms.
/// Leave events are ignored so the pointer can travel onto the floating card
/// without it vanishing.
#[allow(clippy::too_many_arguments)]
fn line_row_el(
    row_index: usize,
    line: &PreparedLine,
    hovered: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
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

    // Narrow `+`/`−` column between the numbers and content: `+` (green) on
    // additions, `−` (red) on deletions, blank on context. Fixed mono width
    // so content stays column-aligned across all rows.
    let sign = sign_cell(line.mark, typography, theme);

    let content = div()
        .flex()
        .flex_row()
        // Small left pad separates the sign column from the content.
        .pl(px(density.gap_inline))
        .pr(px(density.pad_panel))
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(line.fg)
        .child(StyledText::new(line.content.clone()).with_highlights(line.highlights.iter().cloned()));

    let row_id = gpui::ElementId::Name(
        format!(
            "diff-line-{}-{}-{}",
            line.file_idx,
            line.old_cell.trim(),
            line.new_cell.trim()
        )
        .into(),
    );
    // Hover wash: on a tinted row, deepen the same hue a touch so the cue
    // survives (the add/remove color "wins" under the pointer); on context,
    // a neutral panel highlight.
    let hover_bg = match line.row_bg {
        Some(bg) => Hsla {
            a: (bg.a + 0.10).min(0.9),
            ..bg
        },
        None => theme.bg_panel_alt,
    };
    // Enter-driven hover: entering a changed line arms its region (for the
    // sliver widen) + this row (for the card's Y); entering context disarms.
    // Leave is ignored so the pointer can reach the floating staging card
    // without it vanishing.
    let target_region = line.region;
    let target_row = line.region.map(|_| row_index);
    let mut row = div()
        .id(row_id)
        .relative()
        .flex()
        .items_center()
        .h(px(density.h_row))
        // `min_w_full` (not `w_full`): fill the viewport so the tint spans
        // edge-to-edge, but allow growth past it so long lines extend into
        // the list's horizontal scroll range instead of clipping.
        .min_w_full()
        .text_size(px(typography.t_body_sm))
        .hover(move |s| s.bg(hover_bg))
        .on_hover(move |hovered, _window, cx: &mut App| {
            if *hovered {
                let _ = weak.update(cx, |view, cx| view.set_hover(target_region, target_row, cx));
            }
        });
    if let Some(bg) = line.row_bg {
        row = row.bg(bg);
    }
    row.child(strip_cell(line.strip, line.hollow, hovered, density.h_row))
        .child(gutter)
        .child(sign)
        .child(content)
}

/// The elevated Stage/Discard card — a raised surface + crisp border + soft
/// shadow so it reads as a floating control docked over the diff.
fn action_card(actions: gpui::Div, theme: Theme, density: Density) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_active)
        .shadow_md()
        .child(actions)
}

/// Map each change region to the prepared-row index of its anchor (first
/// changed) line, so the host can float the staging card at that row's
/// on-screen Y. Built once per prepared rebuild, off the per-frame path.
pub fn region_anchor_rows(rows: &[PreparedRow]) -> std::collections::HashMap<(usize, usize), usize> {
    let mut map = std::collections::HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let (region, anchor) = match row {
            PreparedRow::Line(l) => (l.region, l.region_anchor),
            PreparedRow::SplitLine {
                region,
                region_anchor,
                ..
            } => (*region, *region_anchor),
            _ => (None, false),
        };
        if anchor && let Some(r) = region {
            map.entry(r).or_insert(i);
        }
    }
    map
}

/// The floating Stage/Discard card for the hovered region. Lives at the
/// body-wrap level (NOT inside the scrollable list row) and is absolutely
/// positioned at `top` (the hovered row's on-screen Y) on the viewport's
/// right edge, so it is always visible regardless of the changed line's
/// length or the horizontal scroll. It re-arms the hover (region + the
/// `anchor_row` it sits at) on its own hover so the pointer can travel onto
/// it without it vanishing. Returns `None` on read-only / untracked sides.
#[allow(clippy::too_many_arguments)]
pub fn staging_card_overlay(
    top: f32,
    file_idx: usize,
    region_idx: usize,
    anchor_row: usize,
    side: HunkActionSide,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> Option<impl IntoElement> {
    let actions = render_hunk_actions(side, file_idx, region_idx, theme, density, typography, &weak)?;
    let hover_weak = weak.clone();
    Some(
        div()
            .id(gpui::ElementId::Name(
                format!("diff-staging-card-{file_idx}-{region_idx}").into(),
            ))
            .absolute()
            .top(px(top))
            // Clear the overview ruler (right edge) so the card never paints
            // over the change-map strip.
            .right(px(density.pad_panel + 6.0))
            .on_hover(move |hovered, _window, cx: &mut App| {
                if *hovered {
                    let _ = hover_weak.update(cx, |view, cx| {
                        view.set_hover(Some((file_idx, region_idx)), Some(anchor_row), cx)
                    });
                }
            })
            .child(action_card(actions, theme, density)),
    )
}

/// One side-by-side row: original (left) | 1px divider | modified (right).
/// Each half is a 50% column with its own sliver + single (sticky) gutter +
/// content; `None` halves render as a faint filler so unequal blocks still
/// align. `h_offset` shifts each column's CONTENT left (synced horizontal
/// scroll) while the gutter stays pinned. Enter-driven hover arms the row's
/// region; the staging card floats separately at the body-wrap level.
#[allow(clippy::too_many_arguments)]
fn split_line_el(
    row_index: usize,
    file_idx: usize,
    left: Option<&SideCell>,
    right: Option<&SideCell>,
    region: Option<(usize, usize)>,
    hovered: bool,
    h_offset: f32,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    let lid = left.map(|c| c.gutter.trim().to_string()).unwrap_or_default();
    let rid = right.map(|c| c.gutter.trim().to_string()).unwrap_or_default();
    let row_id = gpui::ElementId::Name(format!("diff-split-{file_idx}-{lid}-{rid}").into());
    let target_region = region;
    let target_row = region.map(|_| row_index);
    div()
        .id(row_id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .h(px(density.h_row))
        .w_full()
        .text_size(px(typography.t_body_sm))
        .on_hover(move |hovered, _window, cx: &mut App| {
            if *hovered {
                let _ = weak.update(cx, |view, cx| view.set_hover(target_region, target_row, cx));
            }
        })
        .child(split_half(left, hovered, h_offset, theme, density, typography))
        .child(div().w(px(1.0)).h(px(density.h_row)).bg(theme.border_inactive))
        .child(split_half(right, hovered, h_offset, theme, density, typography))
}

/// One 50%-width column of a `SplitLine`. `None` → filler (faint wash, no
/// number); `Some` → sliver + sticky gutter + highlighted content shifted by
/// `h_offset` inside an `overflow_hidden` viewport (so long lines scroll
/// rather than clip, with the gutter staying fixed).
fn split_half(
    cell: Option<&SideCell>,
    hovered: bool,
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
        // Filler: a faint neutral wash signals "no line on this side" — a
        // touch stronger than the body so the absence reads clearly.
        return half.bg(Hsla {
            a: 0.05,
            ..theme.fg_subtle
        });
    };
    if let Some(bg) = cell.bg {
        half = half.bg(bg);
    }
    // Per-half hover wash (parity with inline): deepen the tint on a changed
    // half, neutral highlight on context. Each half owns its own bg, so the
    // hover style attaches here, not on the whole split row.
    let hover_bg = match cell.bg {
        Some(bg) => Hsla {
            a: (bg.a + 0.10).min(0.9),
            ..bg
        },
        None => theme.bg_panel_alt,
    };
    half = half.hover(move |s| s.bg(hover_bg));
    // Sliver + gutter stay pinned (do not scroll horizontally).
    let gutter = div()
        .flex_shrink_0()
        .px(px(density.pad_panel))
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(cell.gutter.clone());
    // Per-side `+`/`−` sign (the left half shows deletions, the right half
    // additions) — pinned beside the gutter so it doesn't scroll.
    let sign = sign_cell(cell.mark, typography, theme);
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
    half.child(strip_cell(cell.strip, cell.hollow, hovered, density.h_row))
        .child(gutter)
        .child(sign)
        .child(content_viewport)
}

/// Collapsed-context expander row — a centered muted "⋯ N unchanged lines"
/// between two hairline rules. Click expands just this run.
fn context_fold_row(
    file_idx: usize,
    fold_id: FoldId,
    count: usize,
    theme: Theme,
    density: Density,
    typography: &Typography,
    weak: WeakEntity<DiffView>,
) -> impl IntoElement {
    let id = gpui::ElementId::Name(format!("diff-fold-{file_idx}-{}", fold_id.1).into());
    let hover_bg = theme.bg_panel;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_row))
        .w_full()
        .px(px(density.pad_panel))
        .bg(theme.bg_panel_alt)
        .border_t_1()
        .border_b_1()
        .border_color(theme.border_inactive)
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, _window, cx: &mut App| {
                let _ = weak.update(cx, |view, cx| {
                    view.expand_fold(fold_id);
                    cx.notify();
                });
            },
        )
        .child(count_fold_label(count))
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
///
/// `hollow` draws the bar outlined (border + faint fill) instead of solid —
/// the combined view's cue that the file is already staged. Solid = unstaged
/// / untracked / single-file.
fn strip_cell(color: Option<Hsla>, hollow: bool, hovered: bool, h: f32) -> gpui::Div {
    const COL_W: f32 = 7.0;
    let bar_w = if hovered { COL_W } else { 3.0 };
    let mut bar = div().w(px(bar_w)).h(px(h));
    if let Some(c) = color {
        if hollow {
            bar = bar.bg(Hsla { a: 0.16, ..c }).border_1().border_color(c);
        } else {
            bar = bar.bg(c);
        }
    }
    div().w(px(COL_W)).h(px(h)).child(bar)
}

/// Fixed-width `+`/`−` sign column derived from a row's ruler mark: `+`
/// (green) for an addition, `−` (red) for a deletion, blank otherwise. Mono
/// + fixed width so the content column stays aligned across all row kinds.
fn sign_cell(mark: Option<RulerMark>, typography: &Typography, theme: Theme) -> gpui::Div {
    let (glyph, color) = match mark {
        Some(RulerMark::Added) => ("+", theme.git.added),
        Some(RulerMark::Removed) => ("−", theme.git.deleted),
        // Non-breaking space holds the fixed column even if a layout pass
        // would collapse a plain space.
        _ => ("\u{00A0}", theme.fg_subtle),
    };
    div()
        .flex_shrink_0()
        .w(px(10.0))
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(color)
        .child(glyph)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(l: f32) -> Hsla {
        Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        }
    }

    #[test]
    fn contrast_ratio_white_over_black_is_maximal() {
        let r = contrast_ratio(gray(1.0), gray(0.0));
        assert!((r - 21.0).abs() < 0.5, "white/black ≈ 21:1, got {r}");
    }

    #[test]
    fn ensure_contrast_lifts_low_contrast_token_on_dark_bg() {
        let bg = gray(0.12); // dark tinted row
        let fg = gray(0.18); // nearly invisible over it
        assert!(contrast_ratio(fg, bg) < MIN_TOKEN_CONTRAST);
        let fixed = ensure_minimum_contrast(fg, bg, MIN_TOKEN_CONTRAST);
        assert!(fixed.l > fg.l, "pushed lighter on a dark background");
        // Small slack absorbs f32 rounding + the 0.04 lightness step; it is
        // NOT a tolerance for under-delivering contrast.
        assert!(
            contrast_ratio(fixed, bg) >= MIN_TOKEN_CONTRAST - 0.05,
            "reaches (≈) the minimum contrast"
        );
    }

    #[test]
    fn ensure_contrast_preserves_already_legible_token() {
        let bg = gray(0.12);
        let fg = gray(0.9);
        let same = ensure_minimum_contrast(fg, bg, MIN_TOKEN_CONTRAST);
        assert_eq!(same.l, fg.l, "legible token is returned unchanged");
    }

    #[test]
    fn composite_over_respects_alpha_bounds() {
        let base = gray(0.1);
        let clear = Hsla { a: 0.0, ..gray(0.9) };
        let solid = Hsla { a: 1.0, ..gray(0.9) };
        // a=0 → base luminance; a=1 → top luminance.
        assert!(
            (relative_luminance(composite_over(clear, base)) - relative_luminance(base)).abs()
                < 1e-4
        );
        assert!(
            (relative_luminance(composite_over(solid, base)) - relative_luminance(gray(0.9))).abs()
                < 1e-3
        );
    }
}
