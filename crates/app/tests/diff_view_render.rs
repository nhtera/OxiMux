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

use oximux_app::shell::diff_view::render::{FilePlan, build_render_plan};
use oximux_core::{DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff};
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
    }
}

#[test]
fn empty_diff_vec_gives_empty_plan() {
    let plan = build_render_plan(&[], false);
    assert!(plan.is_empty());
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
    let plan = build_render_plan(
        &[file("logo.png", DiffStatus::Binary, vec![], false)],
        false,
    );
    match &plan[0] {
        FilePlan::Binary { path, header } => {
            assert_eq!(path, "logo.png");
            assert_eq!(header.label, "Binary");
        }
        other => panic!("expected Binary, got {other:?}"),
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
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false);
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
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false);
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
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false);
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
    let plan = build_render_plan(&[file("a", DiffStatus::Modified, vec![h], false)], false);
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
    );
    assert_eq!(plan.len(), 3);
    let labels: Vec<&str> = plan
        .iter()
        .map(|p| match p {
            FilePlan::Hunked { path, .. } => path.as_str(),
            FilePlan::Collapsed { path, .. } => path.as_str(),
            FilePlan::Binary { path, .. } => path.as_str(),
            FilePlan::ModeOnly { path, .. } => path.as_str(),
        })
        .collect();
    assert_eq!(labels, ["a.rs", "b.rs", "c.rs"]);
}
