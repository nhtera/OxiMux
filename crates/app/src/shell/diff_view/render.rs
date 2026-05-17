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
    pub rows: Vec<LinePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePlan {
    pub kind: DiffLineKind,
    pub content: String,
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
                    .map(|h| HunkPlan {
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
                        rows: h
                            .lines
                            .iter()
                            .map(|l| LinePlan {
                                kind: l.kind,
                                content: l.content.clone(),
                            })
                            .collect(),
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
    // GPUI's base `div()` has no vertical scroll. The diff body is sized by
    // its parent container in `DiffView::render`; long bodies overflow until
    // a virtualized list lands in a later phase. Cap at 1000 lines (via
    // `Collapsed` plan) keeps the worst case bounded for v1.
    let mut col = div().flex().flex_col().h_full().w_full();
    if plan.is_empty() {
        return col
            .child(placeholder("No diff".to_string(), rctx))
            .into_any_element();
    }
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
    let mut col = div().flex().flex_col();
    for h in hunks {
        col = col.child(hunk_header(h.header.clone(), rctx));
        for r in &h.rows {
            col = col.child(line_row(r, rctx));
        }
    }
    col
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

fn line_row(line: &LinePlan, rctx: &RenderCtx<'_>) -> impl IntoElement {
    let (prefix, fg) = match line.kind {
        DiffLineKind::Context => (' ', rctx.theme.fg_muted),
        DiffLineKind::Added => ('+', rctx.theme.status_ok),
        DiffLineKind::Removed => ('-', rctx.theme.status_error),
        DiffLineKind::NoNewlineHint => ('\\', rctx.theme.fg_subtle),
    };
    let text = format!("{prefix}{}", line.content);
    div()
        .flex()
        .items_center()
        .h(px(rctx.density.h_row))
        .px(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(fg)
        .child(text)
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
