//! Pure-unit tests for `build_render_plan` — no GPUI, no tokio. Fast.
//!
//! Covers:
//!   - empty diff vec → empty plan
//!   - hunked file → `FilePlan::Hunked` with line plan rows in order
//!   - binary file → `FilePlan::Binary` (no hunks)
//!   - mode-only change → `FilePlan::ModeOnly`
//!   - mode + content → `FilePlan::Hunked` (content wins)
//!   - rename header label includes from-path + similarity
//!   - large diff + expanded=false → `FilePlan::Collapsed` with totals
//!   - large diff + expanded=true  → `FilePlan::Hunked` (full body)

use oximux_app::shell::diff_view::file_header::{build_row_owner, collect_headers};
use oximux_app::shell::diff_view::paint::{
    PreparedRow, RulerMark, overview_runs, prepare, prepare_split, region_anchor_rows,
};
use oximux_app::shell::diff_view::render::{Highlight, 
    FilePlan, MAX_RENDERED_DIFF_BYTES, MAX_RENDERED_DIFF_LINES, RenderCtx, build_render_plan,
};
use oximux_core::{DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff, change_regions};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::PathBuf;

fn line(kind: DiffLineKind, content: &str) -> DiffLine {
    DiffLine {
        kind,
        content: content.to_string(),
    }
}

fn hunk(old: (u32, u32), new: (u32, u32), suffix: &str, lines: Vec<DiffLine>) -> DiffHunk {
    DiffHunk {
        old_start: old.0,
        old_lines: old.1,
        new_start: new.0,
        new_lines: new.1,
        header_suffix: suffix.to_string(),
        lines,
    }
}

fn file(path: &str, status: DiffStatus, hunks: Vec<DiffHunk>, large: bool) -> FileDiff {
    FileDiff {
        path: PathBuf::from(path),
        status,
        hunks,
        large,
        mode: None,
    }
}

#[test]
fn empty_diff_vec_gives_empty_plan() {
    let plan = build_render_plan(&[], false, Highlight::On { light: false });
    assert!(plan.is_empty());
}

#[test]
fn oversized_by_line_count_suppresses_body_even_when_expanded() {
    // Past the hard line ceiling the file becomes `Oversized` regardless of
    // the expand flag — building one row per line is exactly what we avoid.
    let n = MAX_RENDERED_DIFF_LINES + 10;
    let lines: Vec<DiffLine> = (0..n).map(|_| line(DiffLineKind::Added, "x")).collect();
    let h = hunk((0, 0), (1, n as u32), "", lines);
    let f = file("generated.rs", DiffStatus::Added, vec![h], true);
    // expanded = true must NOT override the hard cap.
    let plan = build_render_plan(&[f], true, Highlight::On { light: false });
    assert_eq!(plan.len(), 1);
    match &plan[0] {
        FilePlan::Oversized { total_lines, .. } => assert_eq!(*total_lines, n),
        other => panic!("expected Oversized, got {other:?}"),
    }
}

#[test]
fn oversized_by_bytes_with_few_long_lines() {
    // A handful of very long lines (a minified bundle, a base64 blob) trips
    // the byte ceiling without coming near the line ceiling.
    let big = "a".repeat(MAX_RENDERED_DIFF_BYTES / 4 + 1);
    let lines: Vec<DiffLine> = (0..5).map(|_| line(DiffLineKind::Added, &big)).collect();
    let h = hunk((0, 0), (1, 5), "", lines);
    let f = file("bundle.min.js", DiffStatus::Added, vec![h], false);
    let plan = build_render_plan(&[f], true, Highlight::On { light: false });
    assert!(
        matches!(plan[0], FilePlan::Oversized { .. }),
        "few long lines should trip the byte cap"
    );
}

#[test]
fn normal_small_file_is_not_oversized() {
    let h = hunk((1, 1), (1, 1), "", vec![line(DiffLineKind::Added, "small")]);
    let f = file("a.rs", DiffStatus::Added, vec![h], false);
    let plan = build_render_plan(&[f], false, Highlight::On { light: false });
    assert!(!matches!(plan[0], FilePlan::Oversized { .. }));
}

#[test]
fn hunked_modified_file_renders_lines_in_order() {
    let h = hunk(
        (1, 2),
        (1, 3),
        "fn main()",
        vec![
            line(DiffLineKind::Context, "let x = 1;"),
            line(DiffLineKind::Removed, "let y = 2;"),
            line(DiffLineKind::Added, "let y = 3;"),
            line(DiffLineKind::Added, "let z = 4;"),
        ],
    );
    let plan = build_render_plan(
        &[file("src/main.rs", DiffStatus::Modified, vec![h], false)],
        false,
        Highlight::On { light: false },
    );
    assert_eq!(plan.len(), 1);
    match &plan[0] {
        FilePlan::Hunked {
            path,
            header,
            hunks,
            added,
            removed,
        } => {
            assert_eq!(path, "src/main.rs");
            assert_eq!(header.label, "Modified");
            assert_eq!(hunks.len(), 1);
            assert_eq!(hunks[0].header, "@@ -1,2 +1,3 @@ fn main()");
            assert_eq!(hunks[0].rows.len(), 4);
            assert_eq!(hunks[0].rows[0].kind, DiffLineKind::Context);
            assert_eq!(hunks[0].rows[1].kind, DiffLineKind::Removed);
            assert_eq!(hunks[0].rows[2].kind, DiffLineKind::Added);
            assert_eq!(hunks[0].rows[3].kind, DiffLineKind::Added);
            // R1: stats tally walks all rows once at plan-build time.
            // 1 removed line + 2 added lines = (2, 1).
            assert_eq!(*added, 2);
            assert_eq!(*removed, 1);
        }
        other => panic!("expected Hunked, got {other:?}"),
    }
}

#[test]
fn binary_file_skips_body() {
    // A non-image binary stays a suppressed-body `Binary` notice.
    let plan = build_render_plan(
        &[file("data.bin", DiffStatus::Binary, vec![], false)],
        false,
        Highlight::On { light: false },
    );
    match &plan[0] {
        FilePlan::Binary { path, header } => {
            assert_eq!(path, "data.bin");
            assert_eq!(header.label, "Binary");
        }
        other => panic!("expected Binary, got {other:?}"),
    }
}

#[test]
fn image_binary_uses_image_variant() {
    // A binary whose extension is a previewable image routes to `Image` so
    // the diff view renders a before/after picture instead of a notice.
    let plan = build_render_plan(
        &[file("assets/logo.png", DiffStatus::Binary, vec![], false)],
        false,
        Highlight::On { light: false },
    );
    match &plan[0] {
        FilePlan::Image { path, header } => {
            assert_eq!(path, "assets/logo.png");
            assert_eq!(header.label, "Binary");
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn mode_only_change_uses_mode_only_variant() {
    let plan = build_render_plan(
        &[file(
            "run.sh",
            DiffStatus::ModeChanged {
                old_mode: 0o100644,
                new_mode: 0o100755,
            },
            vec![],
            false,
        )],
        false,
        Highlight::On { light: false },
    );
    match &plan[0] {
        FilePlan::ModeOnly {
            path,
            old_mode,
            new_mode,
            ..
        } => {
            assert_eq!(path, "run.sh");
            assert_eq!(*old_mode, 0o100644);
            assert_eq!(*new_mode, 0o100755);
        }
        other => panic!("expected ModeOnly, got {other:?}"),
    }
}

#[test]
fn mode_change_with_content_renders_as_hunked() {
    let h = hunk((1, 1), (1, 1), "", vec![line(DiffLineKind::Added, "x")]);
    let plan = build_render_plan(
        &[file(
            "run.sh",
            DiffStatus::ModeChanged {
                old_mode: 0o100644,
                new_mode: 0o100755,
            },
            vec![h],
            false,
        )],
        false,
        Highlight::On { light: false },
    );
    assert!(matches!(plan[0], FilePlan::Hunked { .. }));
}

#[test]
fn renamed_header_includes_from_path_and_similarity() {
    let plan = build_render_plan(
        &[file(
            "new.rs",
            DiffStatus::Renamed {
                from: PathBuf::from("old.rs"),
                similarity: 90,
            },
            vec![],
            false,
        )],
        false,
        Highlight::On { light: false },
    );
    match &plan[0] {
        FilePlan::Hunked { header, .. } => {
            assert!(header.label.contains("old.rs"));
            assert!(header.label.contains("90"));
        }
        other => panic!("expected Hunked (rename with empty hunks), got {other:?}"),
    }
}

#[test]
fn large_diff_collapsed_when_expanded_false() {
    let big_lines: Vec<DiffLine> = (0..1500)
        .map(|i| line(DiffLineKind::Added, &format!("line {i}")))
        .collect();
    let plan = build_render_plan(
        &[file(
            "huge.rs",
            DiffStatus::Modified,
            vec![hunk((1, 1500), (1, 1500), "", big_lines)],
            true,
        )],
        false,
        Highlight::On { light: false },
    );
    match &plan[0] {
        FilePlan::Collapsed {
            total_lines,
            hunk_count,
            ..
        } => {
            assert_eq!(*hunk_count, 1);
            assert_eq!(*total_lines, 1500);
        }
        other => panic!("expected Collapsed, got {other:?}"),
    }
}

#[test]
fn large_diff_expanded_renders_full_body() {
    let big_lines: Vec<DiffLine> = (0..1500)
        .map(|i| line(DiffLineKind::Added, &format!("line {i}")))
        .collect();
    let plan = build_render_plan(
        &[file(
            "huge.rs",
            DiffStatus::Modified,
            vec![hunk((1, 1500), (1, 1500), "", big_lines)],
            true,
        )],
        true,
        Highlight::On { light: false },
    );
    match &plan[0] {
        FilePlan::Hunked { hunks, .. } => {
            assert_eq!(hunks[0].rows.len(), 1500);
        }
        other => panic!("expected Hunked when expanded=true, got {other:?}"),
    }
}

#[test]
fn hunk_header_omits_suffix_separator_when_suffix_empty() {
    let h = hunk((10, 1), (10, 1), "", vec![line(DiffLineKind::Context, "x")]);
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false, Highlight::On { light: false });
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    assert_eq!(hunks[0].header, "@@ -10,1 +10,1 @@");
}

#[test]
fn no_newline_hint_after_removal_is_stripped() {
    // Standalone NoNewlineHint rows convey only "file lacks trailing
    // newline" — they're not content. The renderer drops them so the
    // body reads like the actual line-by-line change.
    let h = hunk(
        (1, 1),
        (1, 1),
        "",
        vec![
            line(DiffLineKind::Removed, "old line"),
            line(DiffLineKind::NoNewlineHint, " No newline at end of file"),
        ],
    );
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false, Highlight::On { light: false });
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    assert_eq!(hunks[0].rows.len(), 1);
    assert_eq!(hunks[0].rows[0].kind, DiffLineKind::Removed);
}

#[test]
fn collapse_no_newline_only_flip_becomes_context() {
    // Pattern git emits when a 1-line file gains content after itself —
    // "test" (no newline) → "test\n\nsss" (no newline). Both lines'
    // bytes are identical; the deletion + re-addition exists only because
    // the old line gained a trailing newline. Render as a single context
    // row so the user reads it as "line 1 unchanged, lines 2-3 added".
    let h = hunk(
        (1, 1),
        (1, 3),
        "",
        vec![
            line(DiffLineKind::Removed, "test"),
            line(DiffLineKind::NoNewlineHint, " No newline at end of file"),
            line(DiffLineKind::Added, "test"),
            line(DiffLineKind::Added, ""),
            line(DiffLineKind::Added, "sss"),
            line(DiffLineKind::NoNewlineHint, " No newline at end of file"),
        ],
    );
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false, Highlight::On { light: false });
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    let rows = &hunks[0].rows;
    assert_eq!(rows.len(), 3, "trio + two adds collapse to 3 rows");
    assert_eq!(rows[0].kind, DiffLineKind::Context);
    assert_eq!(rows[0].content, "test");
    assert_eq!(rows[0].old_line, Some(1));
    assert_eq!(rows[0].new_line, Some(1));
    assert_eq!(rows[1].kind, DiffLineKind::Added);
    assert_eq!(rows[1].new_line, Some(2));
    assert_eq!(rows[2].kind, DiffLineKind::Added);
    assert_eq!(rows[2].new_line, Some(3));
}

#[test]
fn collapse_does_not_swallow_real_content_changes() {
    // Removed("foo") + NoNewlineHint + Added("bar") — contents differ,
    // so this is a genuine line edit at a no-newline boundary. Keep both
    // sides; only strip the noise NoNewlineHint between them.
    let h = hunk(
        (1, 1),
        (1, 1),
        "",
        vec![
            line(DiffLineKind::Removed, "foo"),
            line(DiffLineKind::NoNewlineHint, " No newline at end of file"),
            line(DiffLineKind::Added, "bar"),
            line(DiffLineKind::NoNewlineHint, " No newline at end of file"),
        ],
    );
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false, Highlight::On { light: false });
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    let rows = &hunks[0].rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, DiffLineKind::Removed);
    assert_eq!(rows[0].content, "foo");
    assert_eq!(rows[1].kind, DiffLineKind::Added);
    assert_eq!(rows[1].content, "bar");
}

#[test]
fn paired_modified_lines_get_word_spans() {
    // 1:1 Removed/Added pair where one token differs (`x` → `y`). After
    // build_render_plan, both paired rows carry word-level spans; tokens
    // that match between sides drop back to Same so only the changed
    // word pops bright.
    let h = hunk(
        (1, 1),
        (1, 1),
        "",
        vec![
            line(DiffLineKind::Removed, "let x = 1;"),
            line(DiffLineKind::Added, "let y = 1;"),
        ],
    );
    let plan = build_render_plan(
        &[file("src/main.rs", DiffStatus::Modified, vec![h], false)],
        false,
        Highlight::On { light: false },
    );
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    let removed_row = &hunks[0].rows[0];
    let added_row = &hunks[0].rows[1];
    assert!(
        removed_row.spans.is_some(),
        "paired removed row carries spans"
    );
    assert!(added_row.spans.is_some(), "paired added row carries spans");
}

#[test]
fn rust_file_rows_carry_syntax_tokens() {
    // build_render_plan detects language from file path and populates
    // `tokens` per row via syntect. Rust keywords like `let` should land
    // on a distinct color from string literals — exact values are theme-
    // dependent so we only assert the rows carry *some* tokens.
    let h = hunk(
        (1, 1),
        (1, 1),
        "",
        vec![line(DiffLineKind::Added, r#"let x = "hello";"#)],
    );
    let plan = build_render_plan(
        &[file("src/main.rs", DiffStatus::Modified, vec![h], false)],
        false,
        Highlight::On { light: false },
    );
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    let row = &hunks[0].rows[0];
    assert!(
        !row.tokens.is_empty(),
        "rust row should carry syntax tokens for keyword/string"
    );
    assert!(row.tokens.first().is_some_and(|t| t.start == 0));
    assert!(
        row.tokens
            .last()
            .is_some_and(|t| t.end == row.content.len()),
        "tokens should cover the whole row content"
    );
}

#[test]
fn oversize_diff_skips_syntax_highlighting() {
    use oximux_app::shell::diff_view::render::SYNTAX_HIGHLIGHT_BUDGET_LINES;
    // A diff whose total body exceeds the budget renders without syntax
    // tokens (tints/signs/word-diff still apply) so a huge multi-file diff
    // stays responsive.
    let lines: Vec<_> = (0..SYNTAX_HIGHLIGHT_BUDGET_LINES + 1)
        .map(|_| line(DiffLineKind::Added, r#"let x = "hello";"#))
        .collect();
    let big = hunk((1, 1), (1, lines.len() as u32), "", lines);
    let plan = build_render_plan(
        &[file("src/main.rs", DiffStatus::Modified, vec![big], false)],
        false,
        Highlight::On { light: false },
    );
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    assert!(
        hunks[0].rows.iter().all(|r| r.tokens.is_empty()),
        "over-budget diff must skip syntax tokens"
    );
}

#[test]
fn unknown_extension_rows_have_no_syntax_tokens() {
    // Extension-less or unrecognized file → no language → tokens empty.
    // Renderer reads the empty vec as "fall back to mono color".
    let h = hunk(
        (1, 1),
        (1, 1),
        "",
        vec![line(DiffLineKind::Added, "just some plain text")],
    );
    let plan = build_render_plan(
        &[file("LICENSE", DiffStatus::Modified, vec![h], false)],
        false,
        Highlight::On { light: false },
    );
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    assert!(
        hunks[0].rows[0].tokens.is_empty(),
        "unknown-language row should have no tokens"
    );
}

#[test]
fn unpaired_lines_have_no_word_spans() {
    // 1 Removed vs 2 Added — pair-up policy refuses unequal runs. Both
    // sides keep `spans = None` and the renderer paints them with the
    // whole-line tint instead.
    let h = hunk(
        (1, 1),
        (1, 2),
        "",
        vec![
            line(DiffLineKind::Removed, "let x = 1;"),
            line(DiffLineKind::Added, "let y = 1;"),
            line(DiffLineKind::Added, "let z = 2;"),
        ],
    );
    let plan = build_render_plan(
        &[file("src/main.rs", DiffStatus::Modified, vec![h], false)],
        false,
        Highlight::On { light: false },
    );
    let FilePlan::Hunked { hunks, .. } = &plan[0] else {
        panic!("expected hunked");
    };
    for row in &hunks[0].rows {
        assert!(row.spans.is_none(), "unpaired row should not carry spans");
    }
}

#[test]
fn multi_file_plan_preserves_order() {
    let plan = build_render_plan(
        &[
            file("a.rs", DiffStatus::Added, vec![], false),
            file("b.rs", DiffStatus::Deleted, vec![], false),
            file("c.rs", DiffStatus::Modified, vec![], false),
        ],
        false,
        Highlight::On { light: false },
    );
    assert_eq!(plan.len(), 3);
    let labels: Vec<&str> = plan
        .iter()
        .map(|p| match p {
            FilePlan::Hunked { path, .. } => path.as_str(),
            FilePlan::Collapsed { path, .. } => path.as_str(),
            FilePlan::Binary { path, .. } => path.as_str(),
            FilePlan::Image { path, .. } => path.as_str(),
            FilePlan::ModeOnly { path, .. } => path.as_str(),
            FilePlan::Oversized { path, .. } => path.as_str(),
        })
        .collect();
    assert_eq!(labels, ["a.rs", "b.rs", "c.rs"]);
}

/// Helper: build the side-by-side rows for one file and return each
/// `SplitLine`'s `(has_left, has_right)` occupancy, in order.
fn split_occupancy(f: &FileDiff) -> Vec<(bool, bool)> {
    let plan = build_render_plan(std::slice::from_ref(f), false, Highlight::On { light: false });
    let regions = vec![change_regions(f)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    prepare_split(&plan, &regions, &HashSet::new(), &HashSet::new(), &[], &Default::default(), &rctx)
        .into_iter()
        .filter_map(|r| match r {
            PreparedRow::SplitLine { left, right, .. } => Some((left.is_some(), right.is_some())),
            _ => None,
        })
        .collect()
}

#[test]
fn split_aligns_one_to_one_modify_without_fillers() {
    // ctx, removed, added, ctx — the removed/added pair must land on ONE
    // SplitLine row (left+right), never split across filler rows.
    let f = file(
        "a.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 3),
            (1, 3),
            "",
            vec![
                line(DiffLineKind::Context, "keep"),
                line(DiffLineKind::Removed, "old"),
                line(DiffLineKind::Added, "new"),
                line(DiffLineKind::Context, "tail"),
            ],
        )],
        false,
    );
    let occ = split_occupancy(&f);
    // Every row is fully occupied (two context + one paired modify).
    assert_eq!(occ, vec![(true, true), (true, true), (true, true)]);
}

#[test]
fn split_block_replace_emits_filler_for_longer_side() {
    // 2 removed vs 3 added → rows: (r0,a0), (r1,a1), (filler,a2).
    let f = file(
        "b.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 4),
            (1, 5),
            "",
            vec![
                line(DiffLineKind::Context, "keep"),
                line(DiffLineKind::Removed, "r1"),
                line(DiffLineKind::Removed, "r2"),
                line(DiffLineKind::Added, "a1"),
                line(DiffLineKind::Added, "a2"),
                line(DiffLineKind::Added, "a3"),
                line(DiffLineKind::Context, "tail"),
            ],
        )],
        false,
    );
    let occ = split_occupancy(&f);
    assert_eq!(
        occ,
        vec![
            (true, true),  // ctx "keep"
            (true, true),  // r1 | a1
            (true, true),  // r2 | a2
            (false, true), // filler | a3
            (true, true),  // ctx "tail"
        ]
    );
}

#[test]
fn split_pure_addition_has_left_fillers() {
    // New-only lines (e.g. an addition block) → left filler, right content.
    let f = file(
        "c.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 1),
            (1, 3),
            "",
            vec![
                line(DiffLineKind::Context, "keep"),
                line(DiffLineKind::Added, "a1"),
                line(DiffLineKind::Added, "a2"),
            ],
        )],
        false,
    );
    let occ = split_occupancy(&f);
    assert_eq!(occ, vec![(true, true), (false, true), (false, true)]);
}

/// Helper: overview-ruler runs for one file's INLINE body, as
/// `(start, end, mark)` tuples in order.
fn inline_runs(f: &FileDiff) -> Vec<(f32, f32, RulerMark)> {
    let plan = build_render_plan(std::slice::from_ref(f), false, Highlight::On { light: false });
    let regions = vec![change_regions(f)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    let rows = prepare(&plan, &regions, &HashSet::new(), &HashSet::new(), &[], &Default::default(), &rctx);
    overview_runs(&rows)
        .into_iter()
        .map(|r| (r.start, r.end, r.mark))
        .collect()
}

#[test]
fn overview_runs_coalesce_same_kind_block() {
    // ctx, rem, rem, add, add, ctx → continuous-flow inline rows (no `@@`):
    //   FileHeader, Line(ctx), Line(rem), Line(rem),
    //   Line(add), Line(add), Line(ctx)  = 7 rows.
    // The two removed rows fuse into ONE Removed run, the two added rows
    // into ONE Added run — the ruler paints a bar per block, not per line.
    let f = file(
        "a.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 4),
            (1, 4),
            "",
            vec![
                line(DiffLineKind::Context, "keep"),
                line(DiffLineKind::Removed, "r1"),
                line(DiffLineKind::Removed, "r2"),
                line(DiffLineKind::Added, "a1"),
                line(DiffLineKind::Added, "a2"),
                line(DiffLineKind::Context, "tail"),
            ],
        )],
        false,
    );
    let runs = inline_runs(&f);
    let marks: Vec<RulerMark> = runs.iter().map(|r| r.2).collect();
    assert_eq!(marks, vec![RulerMark::Removed, RulerMark::Added]);
    // Each run spans exactly 2 of the 7 rows and the Added run begins where
    // the Removed run ends (contiguous block, no gap).
    let width = 2.0 / 7.0;
    let (rs, re, _) = runs[0];
    let (as_, ae, _) = runs[1];
    assert!((re - rs - width).abs() < 1e-6, "removed run width");
    assert!((ae - as_ - width).abs() < 1e-6, "added run width");
    assert!((re - as_).abs() < 1e-6, "runs are contiguous");
    assert!(rs >= 0.0 && ae <= 1.0, "runs lie within the body");
}

#[test]
fn overview_split_modify_row_is_mixed() {
    // In side-by-side, a removed/added pair shares ONE SplitLine row, so its
    // ruler tick is Mixed (amber) rather than two separate add/remove bars.
    let f = file(
        "b.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 3),
            (1, 3),
            "",
            vec![
                line(DiffLineKind::Context, "keep"),
                line(DiffLineKind::Removed, "old"),
                line(DiffLineKind::Added, "new"),
                line(DiffLineKind::Context, "tail"),
            ],
        )],
        false,
    );
    let plan = build_render_plan(std::slice::from_ref(&f), false, Highlight::On { light: false });
    let regions = vec![change_regions(&f)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    let rows = prepare_split(&plan, &regions, &HashSet::new(), &HashSet::new(), &[], &Default::default(), &rctx);
    let runs = overview_runs(&rows);
    assert_eq!(runs.len(), 1, "one change block → one run");
    assert_eq!(runs[0].mark, RulerMark::Mixed);
}

/// A folded file emits its header only — no body rows — while siblings
/// keep their full body. The header carries `folded = true`.
#[test]
fn folded_file_emits_header_only() {
    let mk = || {
        hunk(
            (1, 2),
            (1, 3),
            "",
            vec![
                line(DiffLineKind::Context, "ctx"),
                line(DiffLineKind::Added, "new"),
            ],
        )
    };
    let f0 = file("a.rs", DiffStatus::Modified, vec![mk()], false);
    let f1 = file("b.rs", DiffStatus::Modified, vec![mk()], false);
    let plan = build_render_plan(&[f0.clone(), f1.clone()], false, Highlight::On { light: false });
    let regions = vec![change_regions(&f0), change_regions(&f1)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    let mut collapsed = HashSet::new();
    collapsed.insert(0usize);
    let rows = prepare(&plan, &regions, &collapsed, &HashSet::new(), &[], &Default::default(), &rctx);
    let owner = build_row_owner(&rows);

    // File 0 contributes exactly one row — its (folded) header.
    let file0: Vec<&PreparedRow> = rows
        .iter()
        .zip(&owner)
        .filter(|(_, o)| **o == 0)
        .map(|(r, _)| r)
        .collect();
    assert_eq!(file0.len(), 1, "folded file emits header only");
    match file0[0] {
        PreparedRow::FileHeader {
            file_idx, folded, ..
        } => {
            assert_eq!(*file_idx, 0);
            assert!(*folded, "folded file's header carries folded=true");
        }
        _ => panic!("expected folded FileHeader"),
    }

    // File 1 keeps its body (header + region header + lines).
    assert!(
        owner.iter().filter(|o| **o == 1).count() > 1,
        "unfolded sibling keeps its body"
    );
}

/// Two edits 4 context lines apart fall WITHIN `2*HUNK_CONTEXT`, so
/// `change_regions` merges them into ONE region — but the split walk makes
/// TWO change blocks. Both blocks must carry that one region (so neither
/// strands without a staging card), and only the first block anchors it.
/// Regression: a block-counting scheme drifted `next_region` and left later
/// blocks with `region = None` (no Stage/Discard on hover).
#[test]
fn split_merged_region_keeps_all_blocks_tagged() {
    let f = file(
        "a.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 8),
            (1, 8),
            "",
            vec![
                line(DiffLineKind::Removed, "a"),
                line(DiffLineKind::Added, "A"),
                line(DiffLineKind::Context, "c1"),
                line(DiffLineKind::Context, "c2"),
                line(DiffLineKind::Context, "c3"),
                line(DiffLineKind::Context, "c4"),
                line(DiffLineKind::Removed, "b"),
                line(DiffLineKind::Added, "B"),
            ],
        )],
        false,
    );
    assert_eq!(change_regions(&f).len(), 1, "two close edits merge to 1 region");

    let plan = build_render_plan(std::slice::from_ref(&f), false, Highlight::On { light: false });
    let regions = vec![change_regions(&f)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    let rows = prepare_split(&plan, &regions, &HashSet::new(), &HashSet::new(), &[], &Default::default(), &rctx);

    let mut tagged = 0;
    let mut anchors = 0;
    for r in &rows {
        if let PreparedRow::SplitLine {
            region,
            region_anchor,
            left,
            right,
            ..
        } = r
        {
            let is_change = left.as_ref().is_some_and(|c| c.mark.is_some())
                || right.as_ref().is_some_and(|c| c.mark.is_some());
            if is_change {
                assert_eq!(*region, Some((0, 0)), "changed split row carries the merged region");
                tagged += 1;
                if *region_anchor {
                    anchors += 1;
                }
            }
        }
    }
    assert!(tagged >= 2, "both clusters' changed rows are tagged");
    assert_eq!(anchors, 1, "exactly one anchor for the merged region");
}

/// Helper: build inline rows for one file with the given expanded folds.
fn inline_rows(f: &FileDiff, expanded: &HashSet<(usize, u32)>) -> Vec<PreparedRow> {
    let plan = build_render_plan(std::slice::from_ref(f), false, Highlight::On { light: false });
    let regions = vec![change_regions(f)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    prepare(&plan, &regions, &HashSet::new(), expanded, &[], &Default::default(), &rctx)
}

/// A long unchanged-context run between two changes folds its middle: 3
/// lines kept on each side, the rest behind one `ContextFold`. Continuous
/// flow means NO `@@` marker rows are ever emitted.
#[test]
fn long_context_run_folds_middle() {
    // Added(new=1), 20×Context(new=2..21), Added(new=22) — the 20-line
    // context run (> threshold 10) collapses to 3 + fold(14) + 3.
    let mut lines = vec![line(DiffLineKind::Added, "first change")];
    for i in 0..20 {
        lines.push(line(DiffLineKind::Context, &format!("ctx {i}")));
    }
    lines.push(line(DiffLineKind::Added, "second change"));
    let f = file(
        "a.rs",
        DiffStatus::Modified,
        vec![hunk((1, 21), (1, 22), "", lines)],
        false,
    );

    let rows = inline_rows(&f, &HashSet::new());
    let folds: Vec<(usize, usize)> = rows
        .iter()
        .filter_map(|r| match r {
            PreparedRow::ContextFold { fold_id, count, .. } => Some((fold_id.1 as usize, *count)),
            _ => None,
        })
        .collect();
    assert_eq!(folds.len(), 1, "one folded run");
    // Hidden count = 20 - 2*3 = 14; fold anchored at the 4th context line
    // (new line number 5).
    assert_eq!(folds[0], (5, 14), "(first-hidden-line, hidden-count)");

    // Exactly 3 context lines survive on each side (6 total) plus the two
    // changed lines = 8 `Line` rows.
    let line_rows = rows
        .iter()
        .filter(|r| matches!(r, PreparedRow::Line(_)))
        .count();
    assert_eq!(line_rows, 8, "3 kept context each side + 2 changes");

    // Expanding the fold renders the full run (no ContextFold, all 20 ctx).
    let mut expanded = HashSet::new();
    expanded.insert((0usize, 5u32));
    let rows = inline_rows(&f, &expanded);
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, PreparedRow::ContextFold { .. })),
        "expanded fold emits no expander"
    );
    let line_rows = rows
        .iter()
        .filter(|r| matches!(r, PreparedRow::Line(_)))
        .count();
    assert_eq!(line_rows, 22, "20 context + 2 changes fully rendered");
}

/// A short context run (≤ threshold) never folds, and the first changed
/// line of a region carries the staging `region_anchor` flag.
#[test]
fn short_context_keeps_full_and_anchors_region() {
    let f = file(
        "a.rs",
        DiffStatus::Modified,
        vec![hunk(
            (1, 3),
            (1, 4),
            "",
            vec![
                line(DiffLineKind::Context, "keep"),
                line(DiffLineKind::Removed, "old"),
                line(DiffLineKind::Added, "new1"),
                line(DiffLineKind::Added, "new2"),
                line(DiffLineKind::Context, "tail"),
            ],
        )],
        false,
    );
    let rows = inline_rows(&f, &HashSet::new());
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, PreparedRow::ContextFold { .. })),
        "short context never folds"
    );
    // The first changed line (the removed "old") is the region anchor; the
    // following added lines are in the region but are NOT anchors.
    let anchors: Vec<(&str, bool)> = rows
        .iter()
        .filter_map(|r| match r {
            PreparedRow::Line(l) if l.region.is_some() => {
                Some((l.content.as_ref(), l.region_anchor))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        anchors,
        vec![("old", true), ("new1", false), ("new2", false)],
        "only the region's first changed line anchors the staging card"
    );
    // Context lines carry no region (so hovering them won't keep a card open).
    let ctx_with_region = rows.iter().any(|r| matches!(
        r,
        PreparedRow::Line(l) if l.region.is_some()
            && (l.content.as_ref() == "keep" || l.content.as_ref() == "tail")
    ));
    assert!(!ctx_with_region, "context lines carry no region tag");
}

/// `region_anchor_rows` maps every change region to the prepared-row index
/// of its anchor (first changed) line — what the host floats the staging
/// card against. Robust to context folding (anchors are never folded).
#[test]
fn region_anchor_rows_point_at_anchor_lines() {
    // Two changes separated by a long context run → two regions; the run
    // also folds, so anchor rows must still resolve through the fold.
    let mut lines = vec![line(DiffLineKind::Added, "first change")];
    for i in 0..20 {
        lines.push(line(DiffLineKind::Context, &format!("ctx {i}")));
    }
    lines.push(line(DiffLineKind::Added, "second change"));
    let f = file(
        "a.rs",
        DiffStatus::Modified,
        vec![hunk((1, 21), (1, 22), "", lines)],
        false,
    );
    let rows = inline_rows(&f, &HashSet::new());
    let map = region_anchor_rows(&rows);

    // One entry per change region.
    assert_eq!(map.len(), change_regions(&f).len());
    assert_eq!(map.len(), 2, "two changes separated by a wide gap → 2 regions");

    // Every mapped index points at a Line that is the region's anchor.
    for (region, &idx) in &map {
        match &rows[idx] {
            PreparedRow::Line(l) => {
                assert!(l.region_anchor, "mapped row is a region anchor");
                assert_eq!(l.region, Some(*region), "anchor row carries its region");
            }
            _ => panic!("anchor row must be a Line"),
        }
    }
}

/// `collect_headers` returns one entry per file, in order, carrying each
/// file's fold state.
#[test]
fn collect_headers_one_per_file_with_fold_state() {
    let mk = || {
        hunk(
            (1, 1),
            (1, 2),
            "",
            vec![line(DiffLineKind::Added, "x")],
        )
    };
    let f0 = file("a.rs", DiffStatus::Modified, vec![mk()], false);
    let f1 = file("b.rs", DiffStatus::Modified, vec![mk()], false);
    let plan = build_render_plan(&[f0.clone(), f1.clone()], false, Highlight::On { light: false });
    let regions = vec![change_regions(&f0), change_regions(&f1)];
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    let mut collapsed = HashSet::new();
    collapsed.insert(1usize);
    let rows = prepare(&plan, &regions, &collapsed, &HashSet::new(), &[], &Default::default(), &rctx);
    let headers = collect_headers(&rows);
    assert_eq!(headers.len(), 2, "one header per file");
    assert_eq!(headers[0].file_idx, 0);
    assert!(!headers[0].folded);
    assert_eq!(headers[1].file_idx, 1);
    assert!(headers[1].folded);
}

fn render_ctx_for<'a>(typography: &'a Typography) -> RenderCtx<'a> {
    RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography,
    }
}

/// One single-line modified file — enough to emit one changed `Line`/
/// `SplitLine` row carrying a sliver.
fn one_change_file(path: &str, content: &str) -> FileDiff {
    file(
        path,
        DiffStatus::Modified,
        vec![hunk(
            (1, 1),
            (1, 1),
            "",
            vec![line(DiffLineKind::Added, content)],
        )],
        false,
    )
}

/// `(file_idx, hollow)` for every changed inline `Line` row (those that carry
/// a sliver), given a multi-file plan + per-file staged tags.
fn inline_hollow_by_file(files: &[FileDiff], staged_per_file: &[bool]) -> Vec<(usize, bool)> {
    let plan = build_render_plan(files, false, Highlight::On { light: false });
    let regions: Vec<_> = files.iter().map(change_regions).collect();
    let typography = Typography::default();
    let rctx = render_ctx_for(&typography);
    prepare(
        &plan,
        &regions,
        &HashSet::new(),
        &HashSet::new(),
        staged_per_file,
        &Default::default(),
        &rctx,
    )
    .into_iter()
    .filter_map(|r| match r {
        PreparedRow::Line(l) if l.strip.is_some() => Some((l.file_idx, l.hollow)),
        _ => None,
    })
    .collect()
}

#[test]
fn staged_file_group_renders_hollow_sliver_only_for_staged_files() {
    // Combined-view tags: file 0 unstaged (solid), file 1 staged (hollow).
    let files = [
        one_change_file("u.rs", "x"),
        one_change_file("s.rs", "y"),
    ];
    let flags = inline_hollow_by_file(&files, &[false, true]);
    assert!(!flags.is_empty(), "both files contribute a changed line");
    for (file_idx, hollow) in flags {
        assert_eq!(
            hollow,
            file_idx == 1,
            "file {file_idx}: staged → hollow, unstaged → solid"
        );
    }
}

#[test]
fn single_file_diff_renders_solid_sliver() {
    // No group tags (empty staged_per_file) → uniformly solid, never hollow.
    let files = [one_change_file("a.rs", "z")];
    let flags = inline_hollow_by_file(&files, &[]);
    assert!(!flags.is_empty(), "must have at least one changed line");
    assert!(
        flags.iter().all(|(_, hollow)| !hollow),
        "single-file slivers are solid"
    );
}

#[test]
fn staged_file_group_renders_hollow_sliver_in_split_mode() {
    // The hollow post-pass must reach both columns of a `SplitLine` too.
    let files = [
        one_change_file("u.rs", "x"),
        one_change_file("s.rs", "y"),
    ];
    let plan = build_render_plan(&files, false, Highlight::On { light: false });
    let regions: Vec<_> = files.iter().map(change_regions).collect();
    let typography = Typography::default();
    let rctx = render_ctx_for(&typography);
    let rows = prepare_split(
        &plan,
        &regions,
        &HashSet::new(),
        &HashSet::new(),
        &[false, true],
        &Default::default(),
        &rctx,
    );
    let mut saw_unstaged_solid = false;
    let mut saw_staged_hollow = false;
    for r in rows {
        if let PreparedRow::SplitLine {
            file_idx,
            left,
            right,
            ..
        } = r
        {
            for cell in [left, right].into_iter().flatten() {
                if cell.strip.is_none() {
                    continue;
                }
                if file_idx == 1 {
                    assert!(cell.hollow, "staged file column must be hollow");
                    saw_staged_hollow = true;
                } else {
                    assert!(!cell.hollow, "unstaged file column must be solid");
                    saw_unstaged_solid = true;
                }
            }
        }
    }
    assert!(saw_unstaged_solid && saw_staged_hollow, "both cases exercised");
}
