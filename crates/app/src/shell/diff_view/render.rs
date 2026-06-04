//! Pure data plan for the DiffView. Splitting the plan-building from the
//! `IntoElement` construction (see sibling `paint.rs`) lets tests assert
//! on the plan without spinning up GPUI and keeps each side under the
//! file-size soft cap.
//!
//! `build_render_plan` walks `&[FileDiff]` and produces a `Vec<FilePlan>`
//! summarising what each file contributes — collapsed marker, hunks,
//! special body (binary, mode-only, rename header). Word-level diff
//! pairings live alongside each `LinePlan` row so the renderer can paint
//! only the changed tokens.

use crate::shell::diff_view::syntax::{HiToken, Language, detect_language, highlight_line};
use crate::shell::diff_view::word_diff::{TokenSpan, diff_words, pair_runs};
use oximux_core::{DiffLine, DiffLineKind, DiffStatus, FileDiff};
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
        /// Total `+` rows across all hunks (post no-newline-collapse).
        /// Renders next to the path as `+N` in green.
        added: u32,
        /// Total `-` rows across all hunks (post no-newline-collapse).
        /// Renders next to the path as `-N` in red.
        removed: u32,
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
    /// Word-level diff spans for paired Removed/Added rows. `Some` only when
    /// this row is half of a 1:1 pair; the renderer paints each span with
    /// its own color so only the changed words pop bright. `None` falls back
    /// to the whole-line tint. See `word_diff::pair_runs` for pair-up rules.
    pub spans: Option<Vec<TokenSpan>>,
    /// Syntect-driven syntax tokens for this row's content. Empty when the
    /// file's language is `Unknown` or the row is blank. The renderer paints
    /// each token in its own color so keywords/strings/comments read at a
    /// glance. Word-diff spans take precedence on Removed/Added paired rows
    /// — see `paint::line_row` for the merge policy.
    pub tokens: Vec<HiToken>,
}

/// Total diff-body lines past which syntax highlighting is skipped for the
/// whole plan. Syntect tokenization is the dominant per-line cost; a combined
/// multi-file diff can sum to tens of thousands of lines and stall the render
/// thread. Past this budget the body still renders with tints, `+`/`−` signs,
/// gutter slivers, and word-diff — only per-token syntax color drops.
pub const SYNTAX_HIGHLIGHT_BUDGET_LINES: usize = 4000;

/// Build the pure render plan. Highlighting is gated on the total body size:
/// a very large multi-file diff renders without syntax color to stay
/// responsive (see [`SYNTAX_HIGHLIGHT_BUDGET_LINES`]).
pub fn build_render_plan(diffs: &[FileDiff], expanded: bool) -> Vec<FilePlan> {
    let total_lines: usize = diffs
        .iter()
        .flat_map(|d| d.hunks.iter())
        .map(|h| h.lines.len())
        .sum();
    let highlight = total_lines <= SYNTAX_HIGHLIGHT_BUDGET_LINES;
    diffs
        .iter()
        .map(|d| build_file_plan(d, expanded, highlight))
        .collect()
}

fn build_file_plan(d: &FileDiff, expanded: bool, highlight: bool) -> FilePlan {
    let path = d.path.display().to_string();
    let lang = detect_language(d.path.as_path());
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
                let hunks: Vec<HunkPlan> = d
                    .hunks
                    .iter()
                    .map(|h| {
                        // Walk the hunk once, tracking the running line
                        // numbers on each side. Context bumps both;
                        // Added bumps new only; Removed bumps old only;
                        // NoNewlineHint carries no positional info.
                        let mut old_n = h.old_start.saturating_sub(1);
                        let mut new_n = h.new_start.saturating_sub(1);
                        // Pre-collapse the no-newline-EOF pattern git emits
                        // when a file's trailing-newline state flips. Without
                        // this the same content line shows once as deletion
                        // and again as addition, which reads as "the line
                        // changed" even though the bytes are identical.
                        let collapsed = collapse_no_newline_eof(&h.lines);
                        let mut rows: Vec<LinePlan> = collapsed
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
                                let tokens =
                                    tokens_for_row(&l.content, l.kind, lang, highlight);
                                LinePlan {
                                    kind: l.kind,
                                    content: l.content.clone(),
                                    old_line,
                                    new_line,
                                    spans: None,
                                    tokens,
                                }
                            })
                            .collect();
                        // Second pass: compute word-level spans for paired
                        // Removed↔Added rows. Only 1:1 adjacent runs pair —
                        // see `word_diff::pair_runs`. Unpaired rows keep
                        // `spans = None` and render with the existing
                        // whole-line tint.
                        let kinds: Vec<DiffLineKind> = rows.iter().map(|r| r.kind).collect();
                        for pairing in pair_runs(&kinds) {
                            let (old_spans, new_spans) = diff_words(
                                &rows[pairing.old_row].content,
                                &rows[pairing.new_row].content,
                            );
                            rows[pairing.old_row].spans = Some(old_spans);
                            rows[pairing.new_row].spans = Some(new_spans);
                        }
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
                // Sum +N / -N once at file scope so the header strip can
                // render `+37 -12` in green/red. Cheaper than per-render
                // recompute and the data plan is the right home for it.
                let (added, removed) = sum_added_removed(&hunks);
                FilePlan::Hunked {
                    path,
                    header,
                    hunks,
                    added,
                    removed,
                }
            }
        }
    }
}

/// Walk every row in every hunk and tally Added / Removed counts. Context
/// and NoNewlineHint rows don't count toward either side. Used by the
/// header strip to display insertion/deletion chips.
fn sum_added_removed(hunks: &[HunkPlan]) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for h in hunks {
        for r in &h.rows {
            match r.kind {
                DiffLineKind::Added => added = added.saturating_add(1),
                DiffLineKind::Removed => removed = removed.saturating_add(1),
                _ => {}
            }
        }
    }
    (added, removed)
}

/// Compute syntax-highlighted tokens for one row's content. Skips the
/// no-newline marker (it isn't real source) and respects the language
/// stub — Unknown languages yield an empty vec, which the renderer reads
/// as "fall back to mono color".
fn tokens_for_row(
    content: &str,
    kind: DiffLineKind,
    lang: Language,
    highlight: bool,
) -> Vec<HiToken> {
    if !highlight || matches!(kind, DiffLineKind::NoNewlineHint) {
        return Vec::new();
    }
    highlight_line(content, lang)
}

/// Smooth over git's `\ No newline at end of file` quirk.
///
/// When a file's trailing-newline state flips (or new lines are appended to
/// a file that lacks a trailing newline), `git diff` represents the last
/// pre-existing line as a deletion *and* re-addition with a `\ No newline`
/// hint sitting between them — even though the bytes themselves are
/// unchanged. The user reads that as "the line changed". This pass:
///
///   1. Collapses every `Removed(X), NoNewlineHint, Added(X)` trio (where
///      the content matches) into a single `Context(X)` row.
///   2. Strips any remaining standalone `NoNewlineHint` rows. The
///      no-newline state is conveyed by the absence of trailing-newline
///      content, not by a body row, so the visual stays clean.
///
/// Real content edits at a no-newline boundary still render as removal
/// + addition because contents differ — only the no-op flip is collapsed.
pub fn collapse_no_newline_eof(lines: &[DiffLine]) -> Vec<DiffLine> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        // Detect the three-row pattern first so we don't strip the
        // NoNewlineHint that belongs to it in the next branch.
        if i + 2 < lines.len() {
            let a = &lines[i];
            let b = &lines[i + 1];
            let c = &lines[i + 2];
            if matches!(a.kind, DiffLineKind::Removed)
                && matches!(b.kind, DiffLineKind::NoNewlineHint)
                && matches!(c.kind, DiffLineKind::Added)
                && a.content == c.content
            {
                out.push(DiffLine {
                    kind: DiffLineKind::Context,
                    content: a.content.clone(),
                });
                i += 3;
                continue;
            }
        }
        if matches!(lines[i].kind, DiffLineKind::NoNewlineHint) {
            i += 1;
            continue;
        }
        out.push(lines[i].clone());
        i += 1;
    }
    out
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
