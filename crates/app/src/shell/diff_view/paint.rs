//! GPUI element construction for the diff viewer.
//!
//! Sibling to `render.rs`. The data plan lives in `render.rs`
//! (`FilePlan`, `HunkPlan`, `LinePlan`, `build_render_plan`); this file
//! consumes the plan and emits the actual divs / colors / borders. Split
//! out so the data-plan side stays unit-testable without spinning up GPUI
//! and so `render.rs` fits comfortably under the file-size cap.

use crate::shell::diff_view::DiffView;
use crate::shell::diff_view::render::{FilePlan, HunkPlan, LinePlan, RenderCtx};
use crate::shell::diff_view::syntax::HiToken;
use crate::shell::diff_view::word_diff::{TokenSpan, WordOp};
use gpui::{
    ClickEvent, ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Rgba, StatefulInteractiveElement as _, Styled, div, px,
};
use gpui_component::{Icon, Sizable as _, tooltip::Tooltip};
use oximux_core::DiffLineKind;

/// Render the plan into an element. Called from `DiffView::render`.
pub fn render_plan(
    plan: &[FilePlan],
    rctx: &RenderCtx<'_>,
    cx: &mut Context<DiffView>,
) -> impl IntoElement {
    // The body's height MUST be its intrinsic content height, not
    // `h_full()`. The parent in `DiffView::render` wraps this element
    // in an `overflow_y_scroll` container which can only detect
    // overflow when its child reports a height larger than the viewport.
    // Setting `h_full()` here makes the body claim exactly the viewport
    // height and the scroll affordance never fires — long diffs clip
    // silently. Empty / placeholder paths still get `h_full` because
    // they want to center vertically inside the available viewport.
    if plan.is_empty() {
        return div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .child(placeholder("No diff".to_string(), rctx))
            .into_any_element();
    }
    let mut col = div().flex().flex_col().w_full();
    for fp in plan {
        col = col.child(render_file_plan(fp, rctx, cx));
    }
    col.into_any_element()
}

fn render_file_plan(
    fp: &FilePlan,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<DiffView>,
) -> impl IntoElement {
    let block = div().flex().flex_col();
    match fp {
        FilePlan::Hunked {
            path,
            header,
            hunks,
            added,
            removed,
        } => {
            let mut col = block
                .child(interactive_file_header(
                    path.clone(),
                    header.label.clone(),
                    Some((*added, *removed)),
                    rctx,
                    cx,
                ))
                .child(hunks_body(hunks, rctx));
            col = col.font(rctx.typography.mono_font());
            col
        }
        FilePlan::Collapsed {
            path,
            header,
            total_lines,
            hunk_count,
        } => {
            let label =
                format!("Large diff: {hunk_count} hunks, {total_lines} lines — click to expand");
            block
                .child(file_header_strip(
                    format!("{path}  ·  {}", header.label),
                    rctx,
                ))
                .child(expand_row(label, rctx, cx))
        }
        FilePlan::Binary { path, header } => block
            .child(file_header_strip(
                format!("{path}  ·  {}", header.label),
                rctx,
            ))
            .child(body_placeholder(
                "Binary file (body suppressed)".to_string(),
                rctx,
            )),
        FilePlan::ModeOnly {
            path,
            header,
            old_mode,
            new_mode,
        } => {
            let msg = format!("Mode change only: {old_mode:o} → {new_mode:o}");
            block
                .child(file_header_strip(
                    format!("{path}  ·  {}", header.label),
                    rctx,
                ))
                .child(body_placeholder(msg, rctx))
        }
    }
}

/// Header strip for non-Hunked variants (Binary, Collapsed, ModeOnly).
/// No stats chip — those file types don't carry per-row +/-.
fn file_header_strip(text: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_tab))
        .px(px(rctx.density.pad_panel))
        .bg(rctx.theme.bg_panel)
        .border_b_1()
        .border_color(rctx.theme.border_inactive)
        .text_size(px(rctx.typography.t_label_caps))
        .font_weight(rctx.typography.w_semibold)
        .text_color(rctx.theme.fg_base)
        .child(text)
}

/// Interactive header strip for `FilePlan::Hunked`. Click anywhere on the
/// bar copies the file path to the clipboard. Hover state telegraphs the
/// affordance — no hover, no obvious clickability.
///
/// Layout, left to right:
///   `<path>` `· <status label>` `<+added>` `<-removed>`
///
/// The `+N` / `-N` chips use `git.added` / `git.deleted` theme colors so
/// they read as actual diff stats, not generic ok/err semantic colors.
fn interactive_file_header(
    path: String,
    label: String,
    stats: Option<(u32, u32)>,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<DiffView>,
) -> impl IntoElement {
    // The `id` is required for `hover()` to track state across frames.
    // Use the path as the seed — diff views never show the same path twice
    // in one render tree, so collisions are impossible.
    let id = gpui::ElementId::Name(format!("diff-header-{path}").into());
    let copy_path = path.clone();
    let bg = rctx.theme.bg_panel;
    // No dedicated hover-bg token in the theme — step up to `bg_panel_alt`
    // so the strip lifts off the body by exactly one shade, mirroring how
    // tabs / row hovers behave elsewhere in the app.
    let hover_bg = rctx.theme.bg_panel_alt;
    let tooltip_text: gpui::SharedString = "Click to copy path".into();
    let mut row = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .h(px(rctx.density.h_tab))
        .px(px(rctx.density.pad_panel))
        .bg(bg)
        .border_b_1()
        .border_color(rctx.theme.border_inactive)
        .text_size(px(rctx.typography.t_label_caps))
        .font_weight(rctx.typography.w_semibold)
        .text_color(rctx.theme.fg_base)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_path.clone()));
        }))
        .child(div().child(path))
        .child(
            div()
                .text_color(rctx.theme.fg_muted)
                .text_size(px(rctx.typography.t_body_sm))
                .child(format!("· {label}")),
        );
    if let Some((added, removed)) = stats {
        row = row.child(stats_chips(added, removed, rctx));
    }
    // Trailing slot: a copy glyph that reinforces "the whole row is a copy
    // affordance". It's a passive icon — the click handler lives on the
    // row, not on the icon, so users can click anywhere on the bar and
    // still get the copy. Using `div().flex_1()` as a spacer pushes the
    // icon all the way right.
    row = row
        .child(div().flex_1())
        .child(
            div()
                .child(
                    Icon::default()
                        .path("icons/copy.svg")
                        .xsmall()
                        .text_color(rctx.theme.fg_subtle),
                ),
        );
    row
}

/// Render the `+N -N` chip cluster. Stats of zero on one side still show
/// (`+0` is informational — confirms there are no additions, just removals
/// or vice versa) to avoid the header reflowing when stats change.
fn stats_chips(added: u32, removed: u32, rctx: &RenderCtx<'_>) -> impl IntoElement {
    let added_color = rctx.theme.git.added;
    let removed_color = rctx.theme.git.deleted;
    div()
        .flex()
        .flex_row()
        .gap(px(6.0))
        .text_size(px(rctx.typography.t_body_sm))
        .child(
            div()
                .text_color(added_color)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(removed_color)
                .child(format!("-{removed}")),
        )
}

fn hunks_body(hunks: &[HunkPlan], rctx: &RenderCtx<'_>) -> impl IntoElement {
    // Gutter width auto-fits the largest line number across all hunks so
    // the divider between gutter and content stays aligned across the
    // whole file (no per-hunk shift when one hunk ends at line 9 and the
    // next starts at line 1004).
    let max_line: u32 = hunks
        .iter()
        .flat_map(|h| h.rows.iter())
        .map(|r| r.old_line.unwrap_or(0).max(r.new_line.unwrap_or(0)))
        .max()
        .unwrap_or(0);
    let gutter_digits = digit_count(max_line);

    let mut col = div().flex().flex_col();
    for h in hunks {
        // Per-hunk strip color — read at a glance whether the hunk is
        // pure adds (green), pure deletes (red), or both (orange). Acts
        // as a minimap-lite: scroll the diff and the colored bands hint
        // where the next changed region begins.
        let strip = hunk_strip_color(&h.rows, rctx);
        if !h.suppress_header {
            col = col.child(hunk_header(h.header.clone(), rctx, strip));
        }
        for r in &h.rows {
            col = col.child(line_row(r, rctx, gutter_digits, strip));
        }
    }
    col
}

/// Pick the strip color for a hunk based on what kind of rows it carries.
/// Pure context hunks (rare — only when the file rename header is the only
/// content) get no strip.
fn hunk_strip_color(rows: &[LinePlan], rctx: &RenderCtx<'_>) -> Option<gpui::Hsla> {
    let mut has_add = false;
    let mut has_rem = false;
    for r in rows {
        match r.kind {
            DiffLineKind::Added => has_add = true,
            DiffLineKind::Removed => has_rem = true,
            _ => {}
        }
    }
    // Bump alpha so the strip reads against the dark panel background
    // without being so bright it competes with the row tints. 0.7 was
    // tuned against the cockpit theme — feel free to revisit if the
    // theme palette changes.
    let alpha = |c: gpui::Hsla| gpui::Hsla { a: 0.7, ..c };
    match (has_add, has_rem) {
        (true, true) => Some(alpha(rctx.theme.status_warn)),
        (true, false) => Some(alpha(rctx.theme.git.added)),
        (false, true) => Some(alpha(rctx.theme.git.deleted)),
        _ => None,
    }
}

/// 3px-wide colored cell prepended to every row in a hunk. Transparent
/// when `color` is `None` so context-only hunks lay out the same width
/// as colored hunks (no horizontal jitter between rows).
fn strip_cell(color: Option<gpui::Hsla>, h: f32) -> gpui::Div {
    let mut cell = div().w(px(3.0)).h(px(h));
    if let Some(c) = color {
        cell = cell.bg(c);
    }
    cell
}

fn digit_count(n: u32) -> usize {
    // Minimum gutter cell width of 2 digits keeps narrow files (≤ 9 lines)
    // from looking cramped against the divider.
    n.checked_ilog10().map(|d| d as usize + 1).unwrap_or(0).max(2)
}

fn hunk_header(
    header: String,
    rctx: &RenderCtx<'_>,
    strip: Option<gpui::Hsla>,
) -> impl IntoElement {
    // Neutral `fg_subtle` text — the previous `status_warn` (orange)
    // collided with the indicator strip's "mixed hunk" orange, making
    // the `@@` marker look like it belonged to the strip. Muted gray
    // reads as metadata, which is what it is.
    div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_row))
        .bg(rctx.theme.bg_panel_alt)
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(strip_cell(strip, rctx.density.h_row))
        .child(div().px(px(rctx.density.pad_panel)).child(header))
}

fn line_row(
    line: &LinePlan,
    rctx: &RenderCtx<'_>,
    gutter_digits: usize,
    strip: Option<gpui::Hsla>,
) -> impl IntoElement {
    let (prefix, fg, row_bg) = match line.kind {
        DiffLineKind::Context => (' ', rctx.theme.fg_muted, None),
        DiffLineKind::Added => (
            '+',
            rctx.theme.status_ok,
            // Faded green background telegraphs the added range stronger
            // than the `+` glyph alone. `a = 0.22` reads as clearly green
            // against the charcoal cockpit theme without bleaching the
            // foreground text.
            Some(gpui::Hsla {
                a: 0.22,
                ..rctx.theme.git.added
            }),
        ),
        DiffLineKind::Removed => (
            '-',
            rctx.theme.status_error,
            Some(gpui::Hsla {
                a: 0.22,
                ..rctx.theme.git.deleted
            }),
        ),
        DiffLineKind::NoNewlineHint => ('\\', rctx.theme.fg_subtle, None),
    };
    // Each gutter cell packs `<number><sign>` so the eye lands on the
    // number first and the sign second — matches the convention used by
    // GitLens / GitHub / VS Code. The sign is rendered per-cell rather
    // than per-row so a removal in the old cell still reads as removed
    // even when the row also has an addition mirror.
    let old_cell = pack_gutter_cell(line.old_line, line.kind, /*is_new_side=*/ false, gutter_digits);
    let new_cell = pack_gutter_cell(line.new_line, line.kind, /*is_new_side=*/ true, gutter_digits);
    let gutter = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .pl(px(rctx.density.pad_panel))
        .pr(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(div().child(old_cell))
        .child(div().child(new_cell));
    let mut row = div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_row))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(fg);
    if let Some(bg) = row_bg {
        row = row.bg(bg);
    }
    // Strikethrough only on the content cell — gutter stays untouched so
    // the line number remains crisp. Telegraphs "this is gone" stronger
    // than the red color alone.
    let mut content = div()
        .flex()
        .flex_row()
        .flex_1()
        .pr(px(rctx.density.pad_panel));
    // Prefix `+`/`-`/` ` stays in the row foreground so the user reads
    // the row kind from the leading glyph even when token spans paint
    // most of the line.
    content = content.child(div().child(format!("{prefix} ")));
    content = paint_content(content, line, fg, rctx);
    if matches!(line.kind, DiffLineKind::Removed) {
        content = content.line_through();
    }
    row.child(strip_cell(strip, rctx.density.h_row))
        .child(gutter)
        .child(content)
}

/// Paint the row's content children, choosing between three strategies:
///
///   1. **Word-diff active (paired Add/Remove with spans)**: paint each
///      word span with `Same`=row-fg, `Insert/Delete`=bright status color.
///      Loses syntax info on that row but the user is reading "what
///      changed here", not "what is this code".
///   2. **Syntax tokens present (Context, or unpaired Add/Remove)**: paint
///      each syntect token with its theme color. Keeps keywords/strings
///      visually distinct so the diff reads as code.
///   3. **Fallback (Unknown language, blank line)**: single child div
///      with the row foreground color.
fn paint_content(
    mut content: gpui::Div,
    line: &LinePlan,
    fg: gpui::Hsla,
    rctx: &RenderCtx<'_>,
) -> gpui::Div {
    if let Some(spans) = line.spans.as_ref()
        && matches!(line.kind, DiffLineKind::Added | DiffLineKind::Removed)
    {
        for span in spans {
            content = content.child(render_token_span(span, line.kind, rctx));
        }
        return content;
    }
    if !line.tokens.is_empty() {
        for tok in slice_tokens(&line.content, &line.tokens) {
            content = content.child(
                div()
                    .text_color(Rgba {
                        r: tok.r as f32 / 255.0,
                        g: tok.g as f32 / 255.0,
                        b: tok.b as f32 / 255.0,
                        a: 1.0,
                    })
                    .child(tok.text),
            );
        }
        return content;
    }
    // Fallback path — row carries no syntax tokens. Use the row's
    // already-applied foreground color via the parent row div, so the
    // child here just inherits.
    let _ = fg;
    content.child(div().child(line.content.clone()))
}

/// Slice the row's content string by the token byte ranges, producing
/// owned `(text, rgb)` pairs the renderer can hand to `div().child(...)`.
/// Defensive against malformed tokens (out-of-range, overlapping): clamps
/// to `content.len()` and skips empty slices. Returns `Vec<HiSliceOwned>`
/// rather than borrowing so the caller can move into GPUI children.
fn slice_tokens(content: &str, tokens: &[HiToken]) -> Vec<HiSliceOwned> {
    let mut out = Vec::with_capacity(tokens.len());
    let max = content.len();
    for tok in tokens {
        let start = tok.start.min(max);
        let end = tok.end.min(max);
        if start >= end {
            continue;
        }
        // `content[start..end]` is a byte slice — syntect tokenizes on
        // bytes too, and the bundled grammars only split at UTF-8 char
        // boundaries, so this slice is always valid UTF-8 for valid
        // inputs. We still guard with `get` to avoid panicking on bad
        // input from a future grammar tweak.
        let Some(slice) = content.get(start..end) else {
            continue;
        };
        out.push(HiSliceOwned {
            text: slice.to_string(),
            r: tok.r,
            g: tok.g,
            b: tok.b,
        });
    }
    out
}

struct HiSliceOwned {
    text: String,
    r: u8,
    g: u8,
    b: u8,
}

/// One token span on a Removed or Added row. `Same` tokens drop back to
/// the muted base color so the bright `+`/`-` highlight reserves itself
/// for the actually-changed words.
fn render_token_span(
    span: &TokenSpan,
    row_kind: DiffLineKind,
    rctx: &RenderCtx<'_>,
) -> impl IntoElement {
    let color = match (span.op, row_kind) {
        (WordOp::Same, _) => rctx.theme.fg_muted,
        (WordOp::Insert, _) => rctx.theme.status_ok,
        (WordOp::Delete, _) => rctx.theme.status_error,
    };
    div().text_color(color).child(span.text.clone())
}

/// Build one gutter cell as `<right-aligned number><sign>`. The sign
/// column is always 1 char wide, so cells align regardless of whether
/// they carry a sign or not.
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

fn expand_row(
    label: String,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<DiffView>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(rctx.density.h_row))
        .px(px(rctx.density.pad_panel))
        .bg(rctx.theme.bg_panel)
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.status_info)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, _: &MouseDownEvent, _window, cx| {
                view.expand();
                cx.notify();
            }),
        )
        .child(label)
}

fn body_placeholder(msg: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(rctx.density.h_tab))
        .px(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(msg)
}

fn placeholder(msg: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .p(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(msg)
}
