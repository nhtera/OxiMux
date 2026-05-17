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
    let plan = build_render_plan(&[file("src/main.rs", DiffStatus::Modified, vec![h], false)], false);
    assert_eq!(plan.len(), 1);
    match &plan[0] {
        FilePlan::Hunked {
            path,
            header,
            hunks,
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
fn no_newline_hint_kind_passes_through() {
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
    assert_eq!(hunks[0].rows[1].kind, DiffLineKind::NoNewlineHint);
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
