//! Tree representation for the SCM panel's Tree-view mode.
//!
//! Pure data: takes a section's slice of [`FileStatus`], builds a trie
//! keyed by path components, computes per-folder dominant-status rollup,
//! then flattens into [`RenderRow`]s respecting a collapsed-folder set.
//! No `gpui` / no rendering — the visual layer lives in
//! `git_panel::tree_render`.
//!
//! **Folder rollup:** `Conflict > Modified > Added/Untracked > Renamed >
//! Copied`. `Deleted` intentionally does NOT propagate to parents — a
//! folder whose only descendants are deleted gets `rollup_status = None`
//! and its badge is suppressed (otherwise every freshly-removed module
//! would surface a misleading red D badge on its ancestors).

use oximux_core::{FileStatus, IndexStatus, WorktreeStatus};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Which side of the porcelain pair this tree slice renders. Drives the
/// per-leaf status-letter derivation (`Staged` = index column; `Unstaged`
/// = worktree column; `Untracked` = always `Untracked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeSection {
    Staged,
    Unstaged,
    Untracked,
}

/// Status letter shared by leaf badges and folder rollup. Conflict is a
/// tree-only synthetic status — leaves render it as `!`, folders surface
/// it as the highest-severity rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflict,
}

impl NodeStatus {
    /// Severity used for folder rollup. Higher wins. `Deleted` returns
    /// `None` so it never propagates to parents (matches the reference UX).
    fn rollup_severity(self) -> Option<u8> {
        match self {
            NodeStatus::Conflict => Some(5),
            NodeStatus::Modified => Some(4),
            NodeStatus::Added | NodeStatus::Untracked => Some(3),
            NodeStatus::Renamed => Some(2),
            NodeStatus::Copied => Some(1),
            NodeStatus::Deleted => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
}

/// One node in the tree. Children keyed by their immediate path component
/// so iteration order is deterministic (`BTreeMap` = lexical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Path from the tree root, e.g. `crates/app/src/main.rs`. The root
    /// node carries an empty path.
    pub path: PathBuf,
    pub kind: NodeKind,
    pub children: BTreeMap<String, TreeNode>,
    /// `Some` on Dir nodes whose subtree has a non-deleted leaf; `None`
    /// on File nodes and on Dir nodes whose leaves are all deleted.
    pub rollup_status: Option<NodeStatus>,
    /// `Some` on File nodes; `None` on Dir nodes.
    pub leaf_status: Option<NodeStatus>,
}

/// One render row emitted by [`flatten`] in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRow {
    pub path: PathBuf,
    pub kind: NodeKind,
    pub depth: u8,
    pub rollup_status: Option<NodeStatus>,
    pub leaf_status: Option<NodeStatus>,
    /// Number of File descendants under this row. `0` for File rows;
    /// the folder badge uses this for the child-count chip.
    pub child_count: u32,
}

/// Build a tree from a section's `FileStatus` iterator. Accepts anything
/// iterable as `&FileStatus` so the caller can pass `&[FileStatus]`
/// (tests) or `&[&FileStatus]::iter().copied()` (production sectioned
/// slice) without an intermediate copy. Files are inserted at their full
/// relative path; intermediate directories are created on demand. Folder
/// rollup is computed bottom-up after the trie is full.
///
/// **Root-path contract:** the returned root node's `path` is always
/// `PathBuf::new()` (the empty path). [`sibling_dirs`] relies on this
/// to detect "the parent of a top-level dir is the tree root itself."
pub fn build_tree<'a, I>(files: I, section: TreeSection) -> TreeNode
where
    I: IntoIterator<Item = &'a FileStatus>,
{
    let mut root = TreeNode {
        path: PathBuf::new(),
        kind: NodeKind::Dir,
        children: BTreeMap::new(),
        rollup_status: None,
        leaf_status: None,
    };
    for f in files {
        let leaf_status = leaf_status_for(f, section);
        insert(&mut root, &f.path, leaf_status);
    }
    compute_rollup(&mut root);
    root
}

fn insert(parent: &mut TreeNode, full_path: &Path, leaf_status: Option<NodeStatus>) {
    let mut components = full_path.iter();
    let Some(first) = components.next() else {
        return;
    };
    let first_str = first.to_string_lossy().into_owned();
    let remaining: PathBuf = components.collect();

    if remaining.as_os_str().is_empty() {
        let leaf_path = parent.path.join(&first_str);
        parent.children.insert(
            first_str,
            TreeNode {
                path: leaf_path,
                kind: NodeKind::File,
                children: BTreeMap::new(),
                rollup_status: None,
                leaf_status,
            },
        );
    } else {
        let child_path = parent.path.join(&first_str);
        let child = parent.children.entry(first_str).or_insert_with(|| TreeNode {
            path: child_path,
            kind: NodeKind::Dir,
            children: BTreeMap::new(),
            rollup_status: None,
            leaf_status: None,
        });
        insert(child, &remaining, leaf_status);
    }
}

fn compute_rollup(node: &mut TreeNode) -> Option<NodeStatus> {
    match node.kind {
        NodeKind::File => node.leaf_status,
        NodeKind::Dir => {
            let mut best: Option<NodeStatus> = None;
            let mut best_severity: Option<u8> = None;
            for child in node.children.values_mut() {
                if let Some(status) = compute_rollup(child)
                    && let Some(sev) = status.rollup_severity()
                    && best_severity.is_none_or(|b| sev > b)
                {
                    best = Some(status);
                    best_severity = Some(sev);
                }
            }
            node.rollup_status = best;
            best
        }
    }
}

fn leaf_status_for(f: &FileStatus, section: TreeSection) -> Option<NodeStatus> {
    if f.conflict_kind.is_some() {
        return Some(NodeStatus::Conflict);
    }
    match section {
        TreeSection::Untracked => Some(NodeStatus::Untracked),
        TreeSection::Staged => match f.index {
            IndexStatus::Modified => Some(NodeStatus::Modified),
            IndexStatus::Added => Some(NodeStatus::Added),
            IndexStatus::Deleted => Some(NodeStatus::Deleted),
            IndexStatus::Renamed => Some(NodeStatus::Renamed),
            IndexStatus::Copied => Some(NodeStatus::Copied),
            IndexStatus::Unmerged => Some(NodeStatus::Conflict),
            _ => None,
        },
        TreeSection::Unstaged => match f.worktree {
            WorktreeStatus::Modified => Some(NodeStatus::Modified),
            WorktreeStatus::Deleted => Some(NodeStatus::Deleted),
            WorktreeStatus::Renamed => Some(NodeStatus::Renamed),
            WorktreeStatus::Unmerged => Some(NodeStatus::Conflict),
            _ => None,
        },
    }
}

/// Flatten the tree into render order. Dir rows come before their
/// children; Dirs in `collapsed` skip their subtree entirely. Lexical
/// order via the underlying `BTreeMap`.
pub fn flatten(tree: &TreeNode, collapsed: &HashSet<PathBuf>) -> Vec<RenderRow> {
    let mut rows = Vec::new();
    for child in tree.children.values() {
        walk(child, 0, collapsed, &mut rows);
    }
    rows
}

fn walk(node: &TreeNode, depth: u8, collapsed: &HashSet<PathBuf>, rows: &mut Vec<RenderRow>) {
    let child_count = if matches!(node.kind, NodeKind::Dir) {
        count_leaves(node)
    } else {
        0
    };
    rows.push(RenderRow {
        path: node.path.clone(),
        kind: node.kind,
        depth,
        rollup_status: node.rollup_status,
        leaf_status: node.leaf_status,
        child_count,
    });
    if matches!(node.kind, NodeKind::Dir) && !collapsed.contains(&node.path) {
        for child in node.children.values() {
            walk(child, depth.saturating_add(1), collapsed, rows);
        }
    }
}

fn count_leaves(node: &TreeNode) -> u32 {
    match node.kind {
        NodeKind::File => 1,
        NodeKind::Dir => node.children.values().map(count_leaves).sum(),
    }
}

/// Walk the subtree rooted at `dir_path` and collect every leaf path
/// (relative to the worktree root) into a flat `Vec`. Returns an empty
/// `Vec` when `dir_path` is not a known directory in the tree.
///
/// Used by the folder hover-action cluster (Stage all / Unstage all /
/// Discard all in folder) and by Shift+click subtree multi-select.
pub fn subtree_leaves(tree: &TreeNode, dir_path: &Path) -> Vec<PathBuf> {
    let Some(dir_node) = find_dir(tree, dir_path) else {
        return Vec::new();
    };
    let mut leaves = Vec::new();
    collect_leaves(dir_node, &mut leaves);
    leaves
}

fn collect_leaves(node: &TreeNode, out: &mut Vec<PathBuf>) {
    match node.kind {
        NodeKind::File => out.push(node.path.clone()),
        NodeKind::Dir => {
            for child in node.children.values() {
                collect_leaves(child, out);
            }
        }
    }
}

/// Find the immediate-sibling Dir paths of `dir_path` — every Dir under
/// the same parent except `dir_path` itself. Returns an empty `Vec` for
/// roots with no parent or when `dir_path` is not in the tree. Used by
/// Cmd+click "collapse all siblings, keep this one open".
pub fn sibling_dirs(tree: &TreeNode, dir_path: &Path) -> Vec<PathBuf> {
    let parent_path = dir_path.parent().unwrap_or(Path::new(""));
    // For root-level dirs (`dir_path` has no parent or its parent is the
    // empty path), the "parent" is the tree root itself.
    let parent_lookup = if parent_path.as_os_str().is_empty() {
        Some(tree)
    } else {
        find_dir(tree, parent_path)
    };
    let Some(parent_node) = parent_lookup else {
        return Vec::new();
    };
    parent_node
        .children
        .values()
        .filter(|c| matches!(c.kind, NodeKind::Dir) && c.path != dir_path)
        .map(|c| c.path.clone())
        .collect()
}

fn find_dir<'a>(tree: &'a TreeNode, target: &Path) -> Option<&'a TreeNode> {
    if target.as_os_str().is_empty() {
        return Some(tree);
    }
    let mut current = tree;
    for component in target.iter() {
        let key = component.to_string_lossy().into_owned();
        current = current.children.get(&key)?;
        if matches!(current.kind, NodeKind::File) {
            return None;
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::FileStatus;

    fn modified(path: &str) -> FileStatus {
        FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Unmodified,
            WorktreeStatus::Modified,
        )
    }

    fn deleted(path: &str) -> FileStatus {
        FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Unmodified,
            WorktreeStatus::Deleted,
        )
    }

    fn renamed(path: &str) -> FileStatus {
        FileStatus::with_status(
            PathBuf::from(path),
            IndexStatus::Unmodified,
            WorktreeStatus::Renamed,
        )
    }

    fn empty_collapsed() -> HashSet<PathBuf> {
        HashSet::new()
    }

    #[test]
    fn single_file_at_root_renders_one_leaf_row() {
        let files = vec![modified("README.md")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let rows = flatten(&tree, &empty_collapsed());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, PathBuf::from("README.md"));
        assert_eq!(rows[0].kind, NodeKind::File);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].leaf_status, Some(NodeStatus::Modified));
    }

    #[test]
    fn deeply_nested_path_creates_intermediate_dirs() {
        let files = vec![modified("a/b/c/d/leaf.rs")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let rows = flatten(&tree, &empty_collapsed());
        // 4 dirs + 1 leaf = 5 rows
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[4].depth, 4);
        assert_eq!(rows[4].kind, NodeKind::File);
        // All ancestors rollup to Modified.
        for r in &rows[..4] {
            assert_eq!(r.kind, NodeKind::Dir);
            assert_eq!(r.rollup_status, Some(NodeStatus::Modified));
            assert_eq!(r.child_count, 1);
        }
    }

    #[test]
    fn deleted_only_folder_suppresses_rollup_badge() {
        let files = vec![deleted("dir/a.rs"), deleted("dir/b.rs")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let dir = tree.children.get("dir").unwrap();
        // Leaves carry their Deleted status...
        for child in dir.children.values() {
            assert_eq!(child.leaf_status, Some(NodeStatus::Deleted));
        }
        // ...but the parent gets no rollup (matches the reference UX).
        assert_eq!(dir.rollup_status, None);
    }

    #[test]
    fn mixed_subtree_rolls_up_dominant_non_deleted_status() {
        // Folder has one Deleted + one Renamed leaf. Deleted is skipped;
        // Renamed surfaces as rollup.
        let files = vec![deleted("dir/gone.rs"), renamed("dir/kept.rs")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        assert_eq!(
            tree.children.get("dir").unwrap().rollup_status,
            Some(NodeStatus::Renamed)
        );
    }

    #[test]
    fn rollup_priority_modified_beats_renamed_beats_copied() {
        let files = vec![
            FileStatus::with_status(
                PathBuf::from("dir/copy.rs"),
                IndexStatus::Copied,
                WorktreeStatus::Unmodified,
            ),
            FileStatus::with_status(
                PathBuf::from("dir/rename.rs"),
                IndexStatus::Renamed,
                WorktreeStatus::Unmodified,
            ),
            FileStatus::with_status(
                PathBuf::from("dir/edit.rs"),
                IndexStatus::Modified,
                WorktreeStatus::Unmodified,
            ),
        ];
        let tree = build_tree(&files, TreeSection::Staged);
        assert_eq!(
            tree.children.get("dir").unwrap().rollup_status,
            Some(NodeStatus::Modified)
        );
    }

    #[test]
    fn collapsed_dir_skips_subtree_in_flatten() {
        let files = vec![
            modified("a/x.rs"),
            modified("a/y.rs"),
            modified("b/z.rs"),
        ];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("a"));
        let rows = flatten(&tree, &collapsed);
        // Expect: dir a (collapsed, no children) + dir b + leaf b/z.rs
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].path, PathBuf::from("a"));
        assert_eq!(rows[0].kind, NodeKind::Dir);
        assert_eq!(rows[1].path, PathBuf::from("b"));
        assert_eq!(rows[2].path, PathBuf::from("b/z.rs"));
    }

    #[test]
    fn folder_child_count_aggregates_leaves_only() {
        let files = vec![
            modified("crates/app/src/main.rs"),
            modified("crates/app/src/lib.rs"),
            modified("crates/core/src/lib.rs"),
        ];
        let tree = build_tree(&files, TreeSection::Unstaged);
        // `crates` rollup folder counts 3 leaves.
        assert_eq!(tree.children.get("crates").unwrap().rollup_status,
                   Some(NodeStatus::Modified));
        let rows = flatten(&tree, &empty_collapsed());
        let crates_row = rows.iter().find(|r| r.path == Path::new("crates")).unwrap();
        assert_eq!(crates_row.child_count, 3);
        let app_src_row = rows
            .iter()
            .find(|r| r.path == Path::new("crates/app/src"))
            .unwrap();
        assert_eq!(app_src_row.child_count, 2);
    }

    #[test]
    fn twelve_file_mixed_tree_renders_lexically_with_correct_rollups() {
        let files = vec![
            modified("a/keep1.rs"),
            deleted("a/gone1.rs"),
            modified("a/nested/keep2.rs"),
            renamed("b/moved.rs"),
            modified("b/edited.rs"),
            deleted("c/orphan1.rs"),
            deleted("c/orphan2.rs"),
            modified("d/single.rs"),
            modified("e/f/g/deep.rs"),
            modified("root1.rs"),
            modified("root2.rs"),
            renamed("z/last.rs"),
        ];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let rows = flatten(&tree, &empty_collapsed());
        // Folder a contains M + D + nested M; rollup = M (D doesn't count).
        assert_eq!(
            tree.children.get("a").unwrap().rollup_status,
            Some(NodeStatus::Modified)
        );
        // Folder c has only Deleted leaves → no rollup.
        assert_eq!(tree.children.get("c").unwrap().rollup_status, None);
        // Folder b: Modified beats Renamed.
        assert_eq!(
            tree.children.get("b").unwrap().rollup_status,
            Some(NodeStatus::Modified)
        );
        // Folder z is single renamed leaf.
        assert_eq!(
            tree.children.get("z").unwrap().rollup_status,
            Some(NodeStatus::Renamed)
        );
        // Lexical ordering: a, b, c, d, e, root1.rs, root2.rs, z (BTreeMap
        // sorts strings; uppercase Z would come before lowercase, but all
        // lowercase here so it's clean alphabetical).
        let top_paths: Vec<_> = rows
            .iter()
            .filter(|r| r.depth == 0)
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(top_paths, vec!["a", "b", "c", "d", "e", "root1.rs", "root2.rs", "z"]);
    }

    #[test]
    fn untracked_section_marks_all_leaves_as_untracked() {
        let files = vec![
            FileStatus::with_status(
                PathBuf::from("new/file1.rs"),
                IndexStatus::Untracked,
                WorktreeStatus::Untracked,
            ),
            FileStatus::with_status(
                PathBuf::from("new/file2.rs"),
                IndexStatus::Untracked,
                WorktreeStatus::Untracked,
            ),
        ];
        let tree = build_tree(&files, TreeSection::Untracked);
        let new_dir = tree.children.get("new").unwrap();
        assert_eq!(new_dir.rollup_status, Some(NodeStatus::Untracked));
        for child in new_dir.children.values() {
            assert_eq!(child.leaf_status, Some(NodeStatus::Untracked));
        }
    }

    #[test]
    fn conflict_outranks_modified_in_rollup() {
        let mut conflict = modified("dir/conflict.rs");
        conflict.conflict_kind = Some(oximux_core::ConflictKind::BothModified);
        let files = vec![modified("dir/plain.rs"), conflict];
        let tree = build_tree(&files, TreeSection::Unstaged);
        assert_eq!(
            tree.children.get("dir").unwrap().rollup_status,
            Some(NodeStatus::Conflict)
        );
    }

    #[test]
    fn subtree_leaves_returns_every_leaf_under_dir() {
        let files = vec![
            modified("a/b/c/leaf1.rs"),
            modified("a/b/leaf2.rs"),
            modified("a/leaf3.rs"),
            modified("z/other.rs"),
        ];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let mut leaves = subtree_leaves(&tree, Path::new("a"));
        leaves.sort();
        assert_eq!(
            leaves,
            vec![
                PathBuf::from("a/b/c/leaf1.rs"),
                PathBuf::from("a/b/leaf2.rs"),
                PathBuf::from("a/leaf3.rs"),
            ]
        );
    }

    #[test]
    fn subtree_leaves_empty_for_unknown_dir() {
        let files = vec![modified("a/x.rs")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        assert!(subtree_leaves(&tree, Path::new("nope/missing")).is_empty());
    }

    #[test]
    fn subtree_leaves_empty_when_target_is_a_file() {
        let files = vec![modified("a/x.rs")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        // Walking into a leaf path is invalid input — return empty
        // rather than the leaf itself so callers don't get a wrong-shape
        // result and re-stage a single file twice.
        assert!(subtree_leaves(&tree, Path::new("a/x.rs")).is_empty());
    }

    #[test]
    fn sibling_dirs_at_root_returns_every_other_top_level_dir() {
        let files = vec![
            modified("a/x.rs"),
            modified("b/y.rs"),
            modified("c/z.rs"),
            // File at root is NOT a sibling Dir.
            modified("readme.md"),
        ];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let mut siblings = sibling_dirs(&tree, Path::new("b"));
        siblings.sort();
        assert_eq!(siblings, vec![PathBuf::from("a"), PathBuf::from("c")]);
    }

    #[test]
    fn sibling_dirs_nested_excludes_self_and_files() {
        let files = vec![
            modified("crates/app/file.rs"),
            modified("crates/core/file.rs"),
            modified("crates/git/file.rs"),
        ];
        let tree = build_tree(&files, TreeSection::Unstaged);
        let mut siblings = sibling_dirs(&tree, Path::new("crates/core"));
        siblings.sort();
        assert_eq!(
            siblings,
            vec![PathBuf::from("crates/app"), PathBuf::from("crates/git")]
        );
    }

    #[test]
    fn sibling_dirs_empty_when_only_child() {
        let files = vec![modified("solo/only.rs")];
        let tree = build_tree(&files, TreeSection::Unstaged);
        assert!(sibling_dirs(&tree, Path::new("solo")).is_empty());
    }

    #[test]
    fn filtered_input_drops_non_matching_subtrees() {
        // Filter walk is implicit: GitPanel already filters
        // `state.files` before partitioning, so the tree we build
        // sees only the matching subset. Subtrees containing no
        // matches are absent entirely. This test simulates the
        // post-filter slice (caller pre-filtered for "keep").
        let all_files = [
            modified("a/keep_me.rs"),
            modified("a/keep_too.rs"),
            modified("b/drop_me.rs"),
            modified("c/drop_too.rs"),
        ];
        // Simulate filter for "keep" — only `a/*` survives.
        let filtered: Vec<_> = all_files
            .iter()
            .filter(|f| f.path.to_string_lossy().contains("keep"))
            .cloned()
            .collect();
        let tree = build_tree(&filtered, TreeSection::Unstaged);
        // Folder `a` survives with its 2 matching leaves.
        assert!(tree.children.contains_key("a"));
        let a = tree.children.get("a").unwrap();
        assert_eq!(a.children.len(), 2);
        // Folders `b` and `c` are absent — no matching leaves.
        assert!(!tree.children.contains_key("b"));
        assert!(!tree.children.contains_key("c"));
    }
}
