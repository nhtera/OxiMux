//! Pure-unit tests for the file rail's jump-target index.
//!
//! `build_first_row_of_file` is the rail's click-to-jump map: file_idx →
//! the prepared-row index of that file's `FileHeader`. Every entry must
//! land on the matching header so a rail click scrolls the body to the
//! right file. No GPUI, no tokio.

use oximux_app::shell::diff_view::file_header::build_first_row_of_file;
use oximux_app::shell::diff_view::paint::{PreparedRow, prepare};
use oximux_app::shell::diff_view::render::{RenderCtx, build_render_plan};
use oximux_core::{DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::PathBuf;

fn line(kind: DiffLineKind, content: &str) -> DiffLine {
    DiffLine {
        kind,
        content: content.to_string(),
    }
}

fn one_change_file(path: &str) -> FileDiff {
    FileDiff {
        path: PathBuf::from(path),
        status: DiffStatus::Modified,
        hunks: vec![DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            header_suffix: String::new(),
            lines: vec![line(DiffLineKind::Added, "x")],
        }],
        large: false,
    }
}

fn prepared(files: &[FileDiff]) -> Vec<PreparedRow> {
    let plan = build_render_plan(files, false);
    let regions: Vec<_> = files.iter().map(oximux_core::change_regions).collect();
    let typography = Typography::default();
    let rctx = RenderCtx {
        theme: Theme::charcoal(),
        density: Density::default(),
        typography: &typography,
    };
    prepare(
        &plan,
        &regions,
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &rctx,
    )
}

#[test]
fn first_row_of_file_points_at_each_files_header() {
    let files = [
        one_change_file("a.rs"),
        one_change_file("b.rs"),
        one_change_file("c.rs"),
    ];
    let rows = prepared(&files);
    let first = build_first_row_of_file(&rows);

    // One entry per file, in order.
    assert_eq!(first.len(), 3, "one jump target per file");
    for (file_idx, &row) in first.iter().enumerate() {
        match &rows[row] {
            PreparedRow::FileHeader {
                file_idx: owner, ..
            } => assert_eq!(
                *owner, file_idx,
                "jump target for file {file_idx} must land on its own header"
            ),
            _ => panic!("file {file_idx} jump target is not a header"),
        }
    }
    // Targets are strictly increasing (files render top-to-bottom).
    assert!(first.windows(2).all(|w| w[0] < w[1]), "headers in body order");
}

#[test]
fn first_row_of_file_empty_for_no_files() {
    let rows = prepared(&[]);
    assert!(build_first_row_of_file(&rows).is_empty());
}
