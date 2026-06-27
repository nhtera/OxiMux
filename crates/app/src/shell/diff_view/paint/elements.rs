use super::*;

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
    split: bool,
    h_offset: f32,
    state: ListState,
    rctx: &RenderCtx<'_>,
    weak: WeakEntity<DiffView>,
    copied_file: Option<usize>,
) -> impl IntoElement {
    if rows.is_empty() {
        return placeholder("No diff".to_string(), rctx).into_any_element();
    }
    let theme = rctx.theme;
    let density = rctx.density;
    let typography = rctx.typography.clone();
    // `gpui::list` measures each item's height, so rows may differ in height
    // (wrapped lines, image previews). The host owns the `ListState`; it calls
    // `reset(len)` on a prepared-row rebuild and `remeasure()` on a font-zoom /
    // wrap toggle so the cached heights stay in step with what's painted.
    let body = list(state, move |i, _window, _cx| {
        rows.get(i)
            .map(|row| {
                build_prepared_row(
                    i,
                    row,
                    hovered_region,
                    h_offset,
                    theme,
                    density,
                    &typography,
                    weak.clone(),
                    copied_file,
                )
            })
            .unwrap_or_else(|| div().into_any_element())
    });
    if split {
        // Side-by-side: columns stay inside the viewport and clip per half (no
        // per-row horizontal scroll). Default `Auto` sizing fills the body
        // width; the list owns its own vertical scroll + virtualization.
        body.size_full().into_any_element()
    } else {
        // Inline: long lines scroll horizontally. `Infer` lays each row at its
        // intrinsic (nowrap) width and sizes the list to the widest row, and
        // the outer `overflow_x_scroll` container scrolls across that width.
        // The list still virtualizes + owns vertical scroll internally — the
        // two scroll axes are orthogonal.
        div()
            .id("diff-body-hscroll")
            .size_full()
            .overflow_x_scroll()
            .child(body.with_sizing_behavior(ListSizingBehavior::Infer).h_full())
            .into_any_element()
    }
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
    copied_file: Option<usize>,
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
            copied_file == Some(*file_idx),
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

    // Packed (file, plan-row) id — same treatment as the split row: as
    // stable as the old gutter-text pair (both only change when the diff
    // changes) without a format! per visible row per frame.
    let row_id = gpui::ElementId::NamedInteger(
        "diff-line".into(),
        ((line.file_idx as u64) << 32) | (row_index as u64 & 0xffff_ffff),
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
    // Gutter note marker — click opens the compose popover. Built before
    // `weak` is moved into `on_hover` below (it needs its own clone).
    let marker = note_marker_cell(
        line.note_anchor.clone(),
        line.has_note,
        weak.clone(),
        theme,
        density,
        typography,
    );
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
    row.child(marker)
        .child(strip_cell(line.strip, line.hollow, hovered, density.h_row))
        .child(gutter)
        .child(sign)
        .child(content)
}

/// A fixed-width gutter cell carrying the review-note marker. Reserved on
/// every line (so columns stay aligned) but only painted on annotatable
/// lines: a filled glyph when a note exists, an otherwise-invisible glyph
/// that reveals faintly on hover so the affordance is discoverable without
/// peppering every line with a dot. Click opens the compose popover anchored
/// to this line.
fn note_marker_cell(
    anchor: Option<NoteAnchor>,
    has_note: bool,
    weak: WeakEntity<DiffView>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> gpui::AnyElement {
    let cell = div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(14.0))
        .h(px(density.h_row));
    let Some(anchor) = anchor else {
        // Non-annotatable line (no-newline hint): reserved blank space.
        return cell.into_any_element();
    };
    let id = gpui::ElementId::Name(
        format!(
            "diff-note-marker-{}-{}-{}",
            anchor.line,
            anchor.side.as_str(),
            anchor.path
        )
        .into(),
    );
    let (glyph, base_color, hover_color) = if has_note {
        ("●", theme.status_info, theme.status_info)
    } else {
        // Invisible until the marker cell is hovered, then a faint hint.
        let transparent = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 0.0,
        };
        ("○", transparent, theme.fg_muted)
    };
    cell.id(id)
        .cursor_pointer()
        .font(typography.mono_font())
        .text_size(px(typography.t_body_sm))
        .text_color(base_color)
        .hover(move |s| s.text_color(hover_color))
        .child(glyph)
        .on_mouse_down(MouseButton::Left, move |_ev, window, cx: &mut App| {
            // Don't let the click also arm the region / staging card.
            cx.stop_propagation();
            let anchor = anchor.clone();
            let _ = weak.update(cx, |view, cx| view.open_note_popover(anchor, window, cx));
        })
        .into_any_element()
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
            // Packed (file, region) id — no per-frame string allocation.
            .id(gpui::ElementId::NamedInteger(
                "diff-staging-card".into(),
                ((file_idx as u64) << 32) | (region_idx as u64 & 0xffff_ffff),
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
    // Packed (file, plan-row) id — identifies the row as stably as the old
    // gutter-text pair (both change only when the diff itself changes) but
    // without two String builds + a format! per visible row per frame.
    let row_id = gpui::ElementId::NamedInteger(
        "diff-split".into(),
        ((file_idx as u64) << 32) | (row_index as u64 & 0xffff_ffff),
    );
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
    let id = gpui::ElementId::NamedInteger(
        "diff-fold".into(),
        ((file_idx as u64) << 32) | (fold_id.1 as u64 & 0xffff_ffff),
    );
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
pub(super) fn line_change_strip(kind: DiffLineKind, rctx: &RenderCtx<'_>) -> Option<Hsla> {
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
pub(super) fn line_matches_anchor(l: &LinePlan, region: &oximux_core::ChangeRegion) -> bool {
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

pub(super) fn digit_count(n: u32) -> usize {
    // Minimum 2 digits keeps narrow files (≤ 9 lines) from looking cramped.
    n.checked_ilog10()
        .map(|d| d as usize + 1)
        .unwrap_or(0)
        .max(2)
}

/// Build one gutter cell as a right-aligned line number (or blanks when the
/// side has no number, e.g. an addition's old side). No `+`/`-` sign — the
/// row tint + gutter sliver carry the add/remove cue.
pub(super) fn pack_gutter_cell(line_no: Option<u32>, digits: usize) -> String {
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

