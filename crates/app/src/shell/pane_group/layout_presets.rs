//! Pure layout-preset transforms for the workspace group tree.
//!
//! `apply_preset` rebuilds a `PaneTree` from its existing leaves into a new
//! shape WITHOUT creating or destroying any leaf. Content is preserved because
//! callers keep the same `PaneGroupId` → `PaneGroup` entity map; only the
//! tree topology changes.
//!
//! **Contract for BottomTerminal:** the caller must ensure a terminal-bearing
//! group already exists (or create one) and pass its id as `terminal`. If
//! `terminal` is `None` or not in the tree this preset falls back to Stacked.

use crate::shell::pane_tree::{Axis, PaneTree};

/// The three workspace layout presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// All groups stacked top-to-bottom in a single vertical split column.
    Stacked,
    /// All groups placed side-by-side in a single horizontal split row.
    Horizontal,
    /// Main groups horizontally arranged on top; one designated terminal
    /// group docked across the bottom at ~25% height. Falls back to
    /// Stacked when `terminal` is absent.
    BottomTerminal,
}

/// Rebuild `tree` into the requested `preset` shape.
///
/// - Every leaf already present in `tree` appears exactly once in the result.
/// - No leaves are created or destroyed by this function.
/// - `terminal`: for `Preset::BottomTerminal`, the leaf id that goes at the
///   bottom. If `None` or not found in the tree, falls back to `Stacked`.
pub fn apply_preset<L: Copy + PartialEq>(
    tree: &PaneTree<L>,
    preset: Preset,
    terminal: Option<L>,
) -> PaneTree<L> {
    let leaves = tree.in_order_leaves();

    match preset {
        Preset::Stacked => stacked(leaves),
        Preset::Horizontal => horizontal(leaves),
        Preset::BottomTerminal => bottom_terminal(leaves, terminal),
    }
}

/// Vertical split: leaves arranged top-to-bottom. Single leaf returns
/// `Leaf` directly (no unnecessary Split wrapper).
fn stacked<L: Copy>(leaves: Vec<L>) -> PaneTree<L> {
    if leaves.len() == 1 {
        return PaneTree::Leaf(leaves[0]);
    }
    let weights = vec![1.0f32; leaves.len()];
    PaneTree::Split {
        axis: Axis::Vertical,
        children: leaves.into_iter().map(PaneTree::Leaf).collect(),
        weights,
    }
}

/// Horizontal split: leaves arranged side-by-side. Single leaf returns
/// `Leaf` directly.
fn horizontal<L: Copy>(leaves: Vec<L>) -> PaneTree<L> {
    if leaves.len() == 1 {
        return PaneTree::Leaf(leaves[0]);
    }
    let weights = vec![1.0f32; leaves.len()];
    PaneTree::Split {
        axis: Axis::Horizontal,
        children: leaves.into_iter().map(PaneTree::Leaf).collect(),
        weights,
    }
}

/// Main content on top, one terminal leaf docked at the bottom.
///
/// Falls back to Stacked when:
/// - `terminal` is `None`
/// - `terminal` is not in `leaves`
/// - there is only 1 leaf (docking would leave zero top panes)
fn bottom_terminal<L: Copy + PartialEq>(leaves: Vec<L>, terminal: Option<L>) -> PaneTree<L> {
    let Some(term_id) = terminal else {
        return stacked(leaves);
    };
    if !leaves.contains(&term_id) {
        return stacked(leaves);
    }
    if leaves.len() == 1 {
        // Only the terminal leaf → stacked (single-leaf, no split needed).
        return stacked(leaves);
    }

    // Partition: terminal goes to bottom; everything else on top.
    let top_leaves: Vec<L> = leaves.iter().copied().filter(|&l| l != term_id).collect();

    let top = if top_leaves.len() == 1 {
        PaneTree::Leaf(top_leaves[0])
    } else {
        let w = vec![1.0f32; top_leaves.len()];
        PaneTree::Split {
            axis: Axis::Horizontal,
            children: top_leaves.into_iter().map(PaneTree::Leaf).collect(),
            weights: w,
        }
    };

    // Vertical split: top takes 3 parts, bottom terminal takes 1.
    PaneTree::Split {
        axis: Axis::Vertical,
        children: vec![top, PaneTree::Leaf(term_id)],
        weights: vec![3.0, 1.0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::pane_tree::PaneGroupId;

    fn g(n: u64) -> PaneGroupId {
        PaneGroupId(n)
    }

    fn leaf_ids(tree: &PaneTree<PaneGroupId>) -> Vec<u64> {
        tree.in_order_leaves().iter().map(|id| id.0).collect()
    }

    // --- Stacked ---

    #[test]
    fn stacked_single_leaf_returns_leaf() {
        let t = PaneTree::Leaf(g(0));
        let out = apply_preset(&t, Preset::Stacked, None);
        assert_eq!(out, PaneTree::Leaf(g(0)));
    }

    #[test]
    fn stacked_two_leaves_makes_vertical_split() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), crate::shell::pane_tree::Axis::Horizontal, g(1));
        let out = apply_preset(&t, Preset::Stacked, None);
        match &out {
            PaneTree::Split { axis, children, .. } => {
                assert_eq!(*axis, Axis::Vertical);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected Split"),
        }
        assert_eq!(leaf_ids(&out), vec![0, 1]);
    }

    #[test]
    fn stacked_three_leaves_all_vertical() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        t.split_leaf(g(1), Axis::Vertical, g(2));
        let out = apply_preset(&t, Preset::Stacked, None);
        let ids = leaf_ids(&out);
        assert_eq!(ids.len(), 3);
        // All leaves present, no duplicates.
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
        match &out {
            PaneTree::Split { axis, .. } => assert_eq!(*axis, Axis::Vertical),
            _ => panic!("expected root Split"),
        }
    }

    #[test]
    fn stacked_idempotent() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        t.split_leaf(g(1), Axis::Vertical, g(2));
        let once = apply_preset(&t, Preset::Stacked, None);
        let twice = apply_preset(&once, Preset::Stacked, None);
        assert_eq!(leaf_ids(&once), leaf_ids(&twice));
    }

    // --- Horizontal ---

    #[test]
    fn horizontal_single_leaf_returns_leaf() {
        let t = PaneTree::Leaf(g(0));
        let out = apply_preset(&t, Preset::Horizontal, None);
        assert_eq!(out, PaneTree::Leaf(g(0)));
    }

    #[test]
    fn horizontal_two_leaves_makes_horizontal_split() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Vertical, g(1));
        let out = apply_preset(&t, Preset::Horizontal, None);
        match &out {
            PaneTree::Split { axis, children, .. } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected Split"),
        }
        assert_eq!(leaf_ids(&out), vec![0, 1]);
    }

    #[test]
    fn horizontal_idempotent() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Vertical, g(1));
        t.split_leaf(g(1), Axis::Horizontal, g(2));
        let once = apply_preset(&t, Preset::Horizontal, None);
        let twice = apply_preset(&once, Preset::Horizontal, None);
        assert_eq!(leaf_ids(&once), leaf_ids(&twice));
    }

    // --- BottomTerminal ---

    #[test]
    fn bottom_terminal_none_falls_back_to_stacked() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        let out = apply_preset(&t, Preset::BottomTerminal, None);
        match &out {
            PaneTree::Split { axis, .. } => assert_eq!(*axis, Axis::Vertical),
            _ => panic!("expected stacked fallback"),
        }
    }

    #[test]
    fn bottom_terminal_not_in_tree_falls_back_to_stacked() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        let out = apply_preset(&t, Preset::BottomTerminal, Some(g(99)));
        match &out {
            PaneTree::Split { axis, .. } => assert_eq!(*axis, Axis::Vertical),
            _ => panic!("expected stacked fallback"),
        }
    }

    #[test]
    fn bottom_terminal_single_leaf_returns_leaf() {
        let t = PaneTree::Leaf(g(0));
        let out = apply_preset(&t, Preset::BottomTerminal, Some(g(0)));
        assert_eq!(out, PaneTree::Leaf(g(0)));
    }

    #[test]
    fn bottom_terminal_two_leaves_vertical_split() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        let out = apply_preset(&t, Preset::BottomTerminal, Some(g(1)));
        match &out {
            PaneTree::Split { axis, children, weights } => {
                assert_eq!(*axis, Axis::Vertical);
                assert_eq!(children.len(), 2);
                // weights: top=3, bottom=1
                assert_eq!(weights[0], 3.0);
                assert_eq!(weights[1], 1.0);
                // bottom child is the terminal leaf
                assert_eq!(children[1], PaneTree::Leaf(g(1)));
            }
            _ => panic!("expected Split"),
        }
        // All leaves preserved.
        let mut ids = leaf_ids(&out);
        ids.sort();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn bottom_terminal_three_leaves_top_horizontal() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        t.split_leaf(g(1), Axis::Vertical, g(2));
        // g(2) is the designated terminal.
        let out = apply_preset(&t, Preset::BottomTerminal, Some(g(2)));
        // Root: Vertical[top, Leaf(2)]
        match &out {
            PaneTree::Split { axis, children, .. } => {
                assert_eq!(*axis, Axis::Vertical);
                assert_eq!(children.len(), 2);
                // Top subtree contains g(0) and g(1) horizontally.
                match &children[0] {
                    PaneTree::Split { axis: top_axis, children: top_children, .. } => {
                        assert_eq!(*top_axis, Axis::Horizontal);
                        assert_eq!(top_children.len(), 2);
                    }
                    PaneTree::Leaf(_) => {
                        // 2-leaf top is a single Leaf — only valid for 1 top leaf
                        panic!("expected Horizontal split for 2 top leaves");
                    }
                }
                assert_eq!(children[1], PaneTree::Leaf(g(2)));
            }
            _ => panic!("expected root Split"),
        }
        // All leaves present.
        let mut ids = leaf_ids(&out);
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn bottom_terminal_idempotent() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        t.split_leaf(g(1), Axis::Vertical, g(2));
        let once = apply_preset(&t, Preset::BottomTerminal, Some(g(2)));
        let twice = apply_preset(&once, Preset::BottomTerminal, Some(g(2)));
        // Ordered compare (same as the stacked/horizontal idempotency tests) so
        // a reordering of the top-pane leaves across a double-apply is caught,
        // not just a count change.
        assert_eq!(leaf_ids(&once), leaf_ids(&twice));
    }

    // --- Leaf-preservation invariant ---

    #[test]
    fn apply_preset_never_loses_or_creates_leaves() {
        let mut t = PaneTree::Leaf(g(0));
        t.split_leaf(g(0), Axis::Horizontal, g(1));
        t.split_leaf(g(1), Axis::Vertical, g(2));
        t.split_leaf(g(2), Axis::Horizontal, g(3));
        let original: std::collections::BTreeSet<u64> = leaf_ids(&t).into_iter().collect();
        for preset in [Preset::Stacked, Preset::Horizontal, Preset::BottomTerminal] {
            let out = apply_preset(&t, preset, Some(g(3)));
            let result: std::collections::BTreeSet<u64> = leaf_ids(&out).into_iter().collect();
            assert_eq!(
                original, result,
                "leaf set changed for preset {preset:?}"
            );
        }
    }
}
