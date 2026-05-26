//! Pure data plan + render helpers for the DiffView. Splitting the data plan
//! (`build_render_plan`) from `IntoElement` construction lets tests assert on
//! the plan without spinning up GPUI.
//!
//! `build_render_plan` walks `&[FileDiff]` and produces a `RenderPlan`
//! summarising what each file contributes — collapsed marker, hunks, special
//! body (binary, mode-only, rename header). The IntoElement renderer in
//! `mod.rs` consumes the plan and only deals with layout + colors.

use crate::shell::diff_view::DiffView;
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    div, px,
};
use oximux_core::{DiffLineKind, DiffStatus, FileDiff};
use oximux_settings::{Density, Theme, Typography};

/// Bundle of styling threaded through the render layer. Same trick as
/// `git_panel::changed_files::RenderCtx` — keeps argument counts under the
/// clippy ceiling.
pub struct RenderCtx<'a> {
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
}

/// Per-file rendering decision computed from `FileDiff` + the `expanded` flag.
/// Tests assert on these variants directly so the visual renderer doesn't
/// need to be exercised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePlan {
    /// Standard hunked body. Always emitted when `large == false`, or when
    /// `large == true && expanded == true`.
    Hunked {
        path: String,
        header: FileHeader,
        hunks: Vec<HunkPlan>,
    },
    /// `large == true && expanded == false`: header + collapse notice, hunk
    /// bodies suppressed.
    Collapsed {
        path: String,
        header: FileHeader,
        total_lines: usize,
        hunk_count: usize,
    },
    /// Binary file body: no patch text.
    Binary { path: String, header: FileHeader },
    /// Mode-only change (no hunks). When mode change *and* content both
    /// changed, the parser yields `ModeChanged` *with* hunks; that case
    /// renders as `Hunked` with the mode line in the header.
    ModeOnly {
        path: String,
        header: FileHeader,
        old_mode: u32,
        new_mode: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    /// Display label such as "Modified", "Added", "Renamed: a → b (90%)",
    /// "Mode 100644 → 100755". Single line.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkPlan {
    /// `@@ -A,B +C,D @@ suffix` header line.
    pub header: String,
    /// Suppress the visual `@@` header for hunks that carry no useful
    /// positional info — e.g. an all-additions hunk on a brand-new file
    /// (`-0,0 +1,N`) or an all-deletions hunk on a removed file. The
    /// renderer hides the row when this is true; the `header` string is
    /// retained for tests + telemetry.
    pub suppress_header: bool,
    pub rows: Vec<LinePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePlan {
    pub kind: DiffLineKind,
    pub content: String,
    /// 1-based old-side line number, or `None` for additions / hunk-marker
    /// rows. Drives the left gutter cell.
    pub old_line: Option<u32>,
    /// 1-based new-side line number, or `None` for deletions / hunk-marker
    /// rows. Drives the right gutter cell.
    pub new_line: Option<u32>,
}

/// Build the pure render plan.
pub fn build_render_plan(diffs: &[FileDiff], expanded: bool) -> Vec<FilePlan> {
    diffs.iter().map(|d| build_file_plan(d, expanded)).collect()
}

fn build_file_plan(d: &FileDiff, expanded: bool) -> FilePlan {
    let path = d.path.display().to_string();
    let header = FileHeader {
        label: format_status_label(&d.status),
    };
    match &d.status {
        DiffStatus::Binary => FilePlan::Binary { path, header },
        DiffStatus::ModeChanged {
            old_mode, new_mode, ..
        } if d.hunks.is_empty() => FilePlan::ModeOnly {
            path,
            header,
            old_mode: *old_mode,
            new_mode: *new_mode,
        },
        _ => {
            if d.large && !expanded {
                let total_lines: usize = d.hunks.iter().map(|h| h.lines.len()).sum();
                FilePlan::Collapsed {
                    path,
                    header,
                    total_lines,
                    hunk_count: d.hunks.len(),
                }
            } else {
                let hunks = d
                    .hunks
                    .iter()
                    .map(|h| {
                        // Walk the hunk once, tracking the running line
                        // numbers on each side. Context bumps both;
                        // Added bumps new only; Removed bumps old only;
                        // NoNewlineHint carries no positional info.
                        let mut old_n = h.old_start.saturating_sub(1);
                        let mut new_n = h.new_start.saturating_sub(1);
                        let rows: Vec<LinePlan> = h
                            .lines
                            .iter()
                            .map(|l| {
                                let (old_line, new_line) = match l.kind {
                                    DiffLineKind::Context => {
                                        old_n += 1;
                                        new_n += 1;
                                        (Some(old_n), Some(new_n))
                                    }
                                    DiffLineKind::Added => {
                                        new_n += 1;
                                        (None, Some(new_n))
                                    }
                                    DiffLineKind::Removed => {
                                        old_n += 1;
                                        (Some(old_n), None)
                                    }
                                    DiffLineKind::NoNewlineHint => (None, None),
                                };
                                LinePlan {
                                    kind: l.kind,
                                    content: l.content.clone(),
                                    old_line,
                                    new_line,
                                }
                            })
                            .collect();
                        // Suppress the `@@` header when one side of the
                        // hunk carries no information — the user can already
                        // tell from the "Added" / "Deleted" status label
                        // plus the all-`+`/`-` row stream.
                        let suppress_header = (h.old_start == 0 && h.old_lines == 0)
                            || (h.new_start == 0 && h.new_lines == 0);
                        HunkPlan {
                            header: format!(
                                "@@ -{},{} +{},{} @@{}",
                                h.old_start,
                                h.old_lines,
                                h.new_start,
                                h.new_lines,
                                if h.header_suffix.is_empty() {
                                    String::new()
                                } else {
                                    format!(" {}", h.header_suffix)
                                }
                            ),
                            suppress_header,
                            rows,
                        }
                    })
                    .collect();
                FilePlan::Hunked {
                    path,
                    header,
                    hunks,
                }
            }
        }
    }
}

fn format_status_label(s: &DiffStatus) -> String {
    match s {
        DiffStatus::Added => "Added".to_string(),
        DiffStatus::Modified => "Modified".to_string(),
        DiffStatus::Deleted => "Deleted".to_string(),
        DiffStatus::Renamed { from, similarity } => {
            format!("Renamed from {} ({}% similar)", from.display(), similarity)
        }
        DiffStatus::Copied { from, similarity } => {
            format!("Copied from {} ({}% similar)", from.display(), similarity)
        }
        DiffStatus::ModeChanged { old_mode, new_mode } => {
            format!("Mode {old_mode:o} → {new_mode:o}")
        }
        DiffStatus::Binary => "Binary".to_string(),
    }
}

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
        } => {
            let mut col = block
                .child(file_header_strip(
                    format!("{path}  ·  {}", header.label),
                    rctx,
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

fn file_header_strip(text: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_tab))
        .px(px(rctx.density.pad_panel))
        .bg(rctx.theme.bg_panel_alt)
        .text_size(px(rctx.typography.t_label_caps))
        .font_weight(rctx.typography.w_semibold)
        .text_color(rctx.theme.fg_base)
        .child(text)
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
        if !h.suppress_header {
            col = col.child(hunk_header(h.header.clone(), rctx));
        }
        for r in &h.rows {
            col = col.child(line_row(r, rctx, gutter_digits));
        }
    }
    col
}

fn digit_count(n: u32) -> usize {
    // Minimum gutter cell width of 2 digits keeps narrow files (≤ 9 lines)
    // from looking cramped against the divider.
    n.checked_ilog10().map(|d| d as usize + 1).unwrap_or(0).max(2)
}

fn hunk_header(header: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_row))
        .px(px(rctx.density.pad_panel))
        .bg(rctx.theme.bg_panel_alt)
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.status_warn)
        .child(header)
}

fn line_row(
    line: &LinePlan,
    rctx: &RenderCtx<'_>,
    gutter_digits: usize,
) -> impl IntoElement {
    let (prefix, fg, row_bg) = match line.kind {
        DiffLineKind::Context => (' ', rctx.theme.fg_muted, None),
        DiffLineKind::Added => (
            '+',
            rctx.theme.status_ok,
            // Faded green background telegraphs the added range stronger
            // than the `+` glyph alone. `a = 0.18` is the sweet spot
            // against the charcoal cockpit theme: clearly green to the
            // eye without bleaching the foreground text.
            Some(gpui::Hsla {
                a: 0.18,
                ..rctx.theme.git.added
            }),
        ),
        DiffLineKind::Removed => (
            '-',
            rctx.theme.status_error,
            Some(gpui::Hsla {
                a: 0.18,
                ..rctx.theme.git.deleted
            }),
        ),
        DiffLineKind::NoNewlineHint => ('\\', rctx.theme.fg_subtle, None),
    };
    let old_cell = match line.old_line {
        Some(n) => format!("{n:>width$}", width = gutter_digits),
        None => " ".repeat(gutter_digits),
    };
    let new_cell = match line.new_line {
        Some(n) => format!("{n:>width$}", width = gutter_digits),
        None => " ".repeat(gutter_digits),
    };
    let gutter = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(rctx.density.pad_panel))
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
    row.child(gutter)
        .child(
            div()
                .flex_1()
                .px(px(rctx.density.pad_panel))
                .child(format!("{prefix} {}", line.content)),
        )
}

fn expand_row(label: String, rctx: &RenderCtx<'_>, cx: &mut Context<DiffView>) -> impl IntoElement {
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
