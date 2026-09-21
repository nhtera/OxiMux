//! Tree layout for the files inside an expanded stash.
//!
//! # A shim, not a fourth `TreeSection`
//!
//! [`source_control::tree`] is pure data and already takes an arbitrary slice
//! of [`FileStatus`] with a [`TreeSection`] discriminator that picks which
//! porcelain column a leaf's letter comes from. A stash file is not a
//! porcelain record — it is a [`StashFile`], carrying a [`DiffStatus`] and an
//! origin — so it is mapped onto that shape here.
//!
//! Adding `TreeSection::Stash` was the obvious alternative and it is the wrong
//! one: the three existing variants mean *staged*, *unstaged* and *untracked*,
//! which are statements about the index. A stash has no index to be on one
//! side of, and a fourth variant would put that claim into the type every
//! reader of `tree.rs` has to understand. The shim keeps the lie out of the
//! shared module and confines it to one function here, where the comment can
//! say what is actually happening: [`TreeSection::Staged`] is being used as a
//! carrier for "read the letter out of the index column", because that is the
//! column the mapping below writes into.
//!
//! [`source_control::tree`]: crate::shell::source_control::tree
//! [`FileStatus`]: oximux_core::FileStatus
//! [`DiffStatus`]: oximux_core::DiffStatus

use std::collections::HashSet;
use std::path::PathBuf;

use oximux_core::{DiffStatus, FileStatus, IndexStatus, StashFile, ViewMode, WorktreeStatus};

use crate::shell::source_control::tree::{NodeKind, RenderRow, TreeSection, build_tree, flatten};
use crate::shell::stash_panel::StashPanel;
use crate::shell::stash_panel::file_row::FLAT_INDENT_STEPS;

/// Extra indent per tree LEVEL, in multiples of the panel's horizontal
/// padding. One step per level: deep enough to read as nesting, shallow
/// enough that a nested path keeps its width in a 220px panel.
pub(super) const TREE_INDENT_STEPS: f32 = 1.0;

/// How deep the indent is allowed to grow before it stops.
///
/// A stash of a deeply nested path would otherwise push the file name out of a
/// 220px panel entirely. Capping the indent costs the reader one level of
/// visual nesting; not capping it costs them the file name.
const MAX_INDENT_DEPTH: u8 = 8;

/// Indent, in padding-steps, for a row at `depth`.
///
/// Depth 0 sits at **exactly** the flat list's indent, and each level adds one
/// step from there. Expressed against `FLAT_INDENT_STEPS` rather than as a
/// literal so the two cannot drift: flipping the toggle must not slide the
/// whole block sideways, and a top-level file that lines up with the stash
/// row's own chevron stops reading as "inside that stash". Found by
/// screenshot — the first version started at one step and did exactly that.
pub(super) fn indent_steps(depth: u8) -> f32 {
    FLAT_INDENT_STEPS + depth.min(MAX_INDENT_DEPTH) as f32 * TREE_INDENT_STEPS
}

/// Map a stash's files onto the `FileStatus` shape `build_tree` reads.
///
/// Only `path` and `index` are populated. The rest of `FileStatus` describes
/// things a stash file has no answer for — an unstaged counterpart, line
/// counts, a merge conflict — and inventing values for them would put data
/// into the tree that nothing downstream may trust. `WorktreeStatus::Unmodified` and
/// `conflict_kind: None` are what `with_status` writes anyway.
fn as_statuses(files: &[StashFile]) -> Vec<FileStatus> {
    files
        .iter()
        .map(|f| {
            FileStatus::with_status(f.path.clone(), index_status_for(&f.status), WorktreeStatus::Unmodified)
        })
        .collect()
}

/// `DiffStatus` → the index column `TreeSection::Staged` reads.
///
/// `ModeChanged` and `Binary` both land on `Modified`: the tree's letter set
/// has no glyph for either, and the flat list already collapses them the same
/// way (`file_row::status_badge`). Keeping the two collapses identical is the
/// point — a file must not change letter when the layout toggle is flipped.
fn index_status_for(status: &DiffStatus) -> IndexStatus {
    match status {
        DiffStatus::Added => IndexStatus::Added,
        DiffStatus::Deleted => IndexStatus::Deleted,
        DiffStatus::Renamed { .. } => IndexStatus::Renamed,
        DiffStatus::Copied { .. } => IndexStatus::Copied,
        DiffStatus::Modified | DiffStatus::ModeChanged { .. } | DiffStatus::Binary => {
            IndexStatus::Modified
        }
    }
}

/// The rows one stash's tree paints, in display order.
pub(super) fn rows(files: &[StashFile], collapsed: &HashSet<PathBuf>) -> Vec<RenderRow> {
    let statuses = as_statuses(files);
    flatten(&build_tree(&statuses, TreeSection::Staged), collapsed)
}

impl StashPanel {
    /// Paths of the files one stash is currently showing, in painted order.
    ///
    /// The keyboard cursor walks this, so it has to agree with the renderer
    /// exactly — including that a file under a collapsed folder is not there.
    pub(super) fn visible_file_paths(&self, sha: &str, files: &[StashFile]) -> Vec<PathBuf> {
        match self.view_mode() {
            ViewMode::Flat => files.iter().map(|f| f.path.clone()).collect(),
            ViewMode::Tree => rows(files, &self.collapsed_for(sha))
                .into_iter()
                .filter(|r| r.kind == NodeKind::File)
                .map(|r| r.path)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::StashFileOrigin;

    fn file(path: &str, status: DiffStatus) -> StashFile {
        StashFile {
            path: PathBuf::from(path),
            status,
            origin: StashFileOrigin::Tracked,
        }
    }

    #[test]
    fn nested_paths_become_folder_rows_with_their_leaves_under_them() {
        let files = vec![
            file("src/a.rs", DiffStatus::Modified),
            file("src/deep/b.rs", DiffStatus::Added),
            file("top.rs", DiffStatus::Deleted),
        ];
        let rows = rows(&files, &HashSet::new());
        // Compared as `PathBuf`, not as the rendered string. `rows` builds
        // each path with `PathBuf::push`, which joins with the platform
        // separator — so a string assertion here passes on unix and fails on
        // Windows with `src\\a.rs`, which is what CI caught. Comparing
        // `Path`s compares COMPONENTS, and Windows accepts either separator
        // when it splits them, so one literal is correct on both.
        let shape: Vec<(PathBuf, NodeKind, u8)> = rows
            .iter()
            .map(|r| (r.path.clone(), r.kind, r.depth))
            .collect();
        assert_eq!(
            shape,
            vec![
                (PathBuf::from("src"), NodeKind::Dir, 0),
                (PathBuf::from("src/a.rs"), NodeKind::File, 1),
                (PathBuf::from("src/deep"), NodeKind::Dir, 1),
                (PathBuf::from("src/deep/b.rs"), NodeKind::File, 2),
                (PathBuf::from("top.rs"), NodeKind::File, 0),
            ],
        );
    }

    #[test]
    fn a_collapsed_folder_hides_its_subtree() {
        let files = vec![
            file("src/a.rs", DiffStatus::Modified),
            file("top.rs", DiffStatus::Modified),
        ];
        let collapsed = HashSet::from([PathBuf::from("src")]);
        let rows = rows(&files, &collapsed);
        assert_eq!(
            rows.iter()
                .map(|r| r.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec!["src".to_string(), "top.rs".to_string()],
        );
    }

    // The toggle must not change what a file's status letter says. Both
    // layouts derive it from the same `DiffStatus`, so this pins the two
    // collapses (`ModeChanged`/`Binary` → Modified) to each other.
    #[test]
    fn mode_changes_and_binaries_read_as_modified_in_both_layouts() {
        assert_eq!(
            index_status_for(&DiffStatus::ModeChanged {
                old_mode: 0o100644,
                new_mode: 0o100755
            }),
            IndexStatus::Modified,
        );
        assert_eq!(index_status_for(&DiffStatus::Binary), IndexStatus::Modified);
    }

    #[test]
    fn the_indent_stops_growing_past_the_cap() {
        assert!(indent_steps(200) <= indent_steps(MAX_INDENT_DEPTH));
        assert!(indent_steps(0) < indent_steps(1));
    }

    // The toggle must not slide the block sideways. A depth-0 tree row and a
    // flat file row are the same row at the same nesting level; when they
    // disagreed, a top-level file lined up with the stash row's own chevron
    // and stopped reading as being inside the stash.
    #[test]
    fn a_top_level_tree_row_sits_exactly_where_a_flat_file_row_sits() {
        assert_eq!(indent_steps(0), FLAT_INDENT_STEPS);
    }

    // A stash file carries no unstaged side and no conflict; the shim must not
    // invent either, because the tree's rollup reads both.
    #[test]
    fn the_shim_leaves_the_worktree_side_unmodified() {
        let statuses = as_statuses(&[file("a.rs", DiffStatus::Modified)]);
        assert_eq!(statuses[0].worktree, WorktreeStatus::Unmodified);
        assert!(statuses[0].conflict_kind.is_none());
    }
}
