//! Unit tests for `build_display_rows` — the pure helper inside
//! `apps/desktop/src/shell/file_tree_view.rs`. No GPUI runtime, no
//! filesystem. The `nodes` callback is an in-memory `HashMap` lookup.

use oximux_app::shell::file_tree_view::{DisplayRow, RowKind, build_display_rows};
use oximux_editor::{FileTreeNode, TreeNodeId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn node(id: u64, path: &str, is_dir: bool, loaded: bool, children: Vec<u64>) -> FileTreeNode {
    FileTreeNode {
        id: TreeNodeId(id),
        path: PathBuf::from(path),
        is_dir,
        loaded,
        children: children.into_iter().map(TreeNodeId).collect(),
    }
}

fn lookup(map: &HashMap<u64, FileTreeNode>) -> impl Fn(TreeNodeId) -> Option<FileTreeNode> + '_ {
    move |id: TreeNodeId| map.get(&id.0).cloned()
}

fn ids_expanded(ids: &[u64]) -> HashSet<TreeNodeId> {
    ids.iter().copied().map(TreeNodeId).collect()
}

fn kinds(rows: &[DisplayRow]) -> Vec<&RowKind> {
    rows.iter().map(|r| &r.kind).collect()
}

#[test]
fn placeholder_row_on_unloaded_dir() {
    // root is dir, loaded=false (walker hasn't fired yet), expanded → expect
    // root row + placeholder child.
    let mut map = HashMap::new();
    map.insert(1, node(1, "/r", true, false, vec![]));
    let rows = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[1]));

    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0].kind, RowKind::Dir { loaded: false, .. }));
    assert!(matches!(rows[1].kind, RowKind::Placeholder { .. }));
    assert_eq!(rows[1].depth, 1);
}

#[test]
fn empty_dir_sentinel() {
    // Loaded dir with no children + expanded → expect EmptyDir row.
    let mut map = HashMap::new();
    map.insert(1, node(1, "/r", true, true, vec![]));
    let rows = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[1]));

    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0].kind, RowKind::Dir { loaded: true, .. }));
    assert!(matches!(rows[1].kind, RowKind::EmptyDir { .. }));
    assert_eq!(rows[1].label, "(empty)");
}

#[test]
fn file_row_no_children() {
    let mut map = HashMap::new();
    map.insert(1, node(1, "/r/main.rs", false, false, vec![]));
    let rows = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[]));

    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].kind, RowKind::File { .. }));
    assert_eq!(rows[0].label, "main.rs");
}

#[test]
fn depth_indent_three_levels() {
    // /a → /a/b → /a/b/c.rs — all three expanded.
    let mut map = HashMap::new();
    map.insert(1, node(1, "/a", true, true, vec![2]));
    map.insert(2, node(2, "/a/b", true, true, vec![3]));
    map.insert(3, node(3, "/a/b/c.rs", false, false, vec![]));
    let rows = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[1, 2]));

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].depth, 0);
    assert_eq!(rows[1].depth, 1);
    assert_eq!(rows[2].depth, 2);
    assert_eq!(rows[2].label, "c.rs");
}

#[test]
fn collapsed_dir_hides_children() {
    // /a expanded, /a/b NOT expanded → only /a + /a/b rows; /a/b's child absent.
    let mut map = HashMap::new();
    map.insert(1, node(1, "/a", true, true, vec![2]));
    map.insert(2, node(2, "/a/b", true, true, vec![3]));
    map.insert(3, node(3, "/a/b/c.rs", false, false, vec![]));
    let rows = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[1]));

    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[1].kind, RowKind::Dir { .. }));
    assert!(
        !kinds(&rows)
            .iter()
            .any(|k| matches!(k, RowKind::File { .. }))
    );
}

#[test]
fn collapsed_dir_click_expands_then_children_appear() {
    // Same nodes; only expanded set differs across two calls.
    let mut map = HashMap::new();
    map.insert(1, node(1, "/a", true, true, vec![2]));
    map.insert(2, node(2, "/a/b", true, true, vec![3]));
    map.insert(3, node(3, "/a/b/c.rs", false, false, vec![]));

    let before = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[1]));
    let after = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[1, 2]));

    assert_eq!(before.len(), 2);
    assert_eq!(after.len(), 3);
    assert!(matches!(after[2].kind, RowKind::File { .. }));
}

#[test]
fn loaded_dir_collapsed_renders_collapsed_chevron() {
    // H1 regression guard: a dir that was previously expanded (so
    // `loaded=true` persists in the model) should still render with the
    // collapsed chevron when its id is NOT in `expanded_ids`.
    let mut map = HashMap::new();
    map.insert(1, node(1, "/a", true, true, vec![2]));
    map.insert(2, node(2, "/a/b.rs", false, false, vec![]));

    // /a loaded but NOT expanded → must report expanded:false to the renderer.
    let rows = build_display_rows(TreeNodeId(1), &lookup(&map), &ids_expanded(&[]));
    assert_eq!(rows.len(), 1);
    match &rows[0].kind {
        RowKind::Dir {
            loaded: true,
            expanded: false,
            ..
        } => {}
        other => panic!("expected Dir {{ loaded:true, expanded:false }}, got {other:?}"),
    }
}

#[test]
fn placeholder_replaced_after_loaded() {
    // First call: dir unloaded → placeholder. Second call: same dir loaded
    // with one child → placeholder gone, real file row present.
    let mut map_a = HashMap::new();
    map_a.insert(1, node(1, "/r", true, false, vec![]));
    let before = build_display_rows(TreeNodeId(1), &lookup(&map_a), &ids_expanded(&[1]));
    assert!(matches!(before[1].kind, RowKind::Placeholder { .. }));

    let mut map_b = HashMap::new();
    map_b.insert(1, node(1, "/r", true, true, vec![2]));
    map_b.insert(2, node(2, "/r/x.rs", false, false, vec![]));
    let after = build_display_rows(TreeNodeId(1), &lookup(&map_b), &ids_expanded(&[1]));
    assert_eq!(after.len(), 2);
    assert!(matches!(after[1].kind, RowKind::File { .. }));
}
