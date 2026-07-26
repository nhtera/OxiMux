//! Pure-data pane tree — split / close / focus-order math for the workspace
//! grid. No GPUI types here; the host keeps the entity store in a parallel
//! `HashMap` and looks up by id during render.
//!
//! Binary splits: every `split_leaf` wraps the target in a fresh `Split`
//! node, halving the target's slice between old and new. Matches the default
//! split semantics in common terminal emulators. Rebalancing or equal-N-way splits are
//! a separate concern (an explicit "balance" action could flatten + equalize
//! a same-axis cascade if/when the UX demands it).

/// Stable identifier for a pane leaf in the content tree. Issued
/// monotonically by the host; never reused after a pane closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// Stable identifier for a pane GROUP in the workspace layout tree.
/// Each group owns its own tab strip; the workspace owns a
/// `PaneTree<PaneGroupId>` of these. Issued monotonically by the host
/// (`PaneGroupManager`); never reused after a group closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneGroupId(pub u64);

/// Convenience alias: the per-group content tree (terminal/editor leaves
/// inside one tab strip). Reserved for callers that still need the
/// internal split; v1 workspace splits use [`GroupTree`] instead.
pub type ContentTree = PaneTree<PaneId>;

/// Convenience alias: the workspace-level split tree of pane groups.
/// Splitting the workspace creates a new sibling leaf in this tree.
pub type GroupTree = PaneTree<PaneGroupId>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Where the new pane goes relative to the target along the new split axis.
/// `After` is the legacy semantics (Right / Down); `Before` powers the
/// Left / Up split actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitInsert {
    Before,
    After,
}

/// Weights are stored as f32 alongside `children`, parallel-indexed. Only the
/// *ratio* between siblings matters at render — `dispatch_grids` and
/// `build_node` both treat `weights[i] / weights.iter().sum()` as the
/// fractional size of child `i`. `PartialEq`/`Eq` on f32 is intentionally
/// avoided by switching `PaneTree` from `derive(Eq)` to manual semantics —
/// only `PartialEq` is meaningful for weights (NaN). Tests compare
/// structurally via dedicated helpers, not `==` on the whole tree.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneTree<L = PaneId> {
    Leaf(L),
    Split {
        axis: Axis,
        children: Vec<PaneTree<L>>,
        /// Parallel to `children`; one weight per child. New splits start at
        /// 1.0 each (so a binary split is 50/50). Drag-resize mutates these.
        /// Always kept positive — `MIN_FLEX` is the floor, enforced by
        /// `set_split_weights`.
        weights: Vec<f32>,
    },
}

/// Minimum weight any child may shrink to. Prevents a divider drag from
/// hiding a pane entirely. Picked so a clamped pane still has enough room
/// for a usable terminal grid at typical viewport sizes.
pub const MIN_FLEX: f32 = 0.1;

impl<L: Copy + PartialEq> PaneTree<L> {
    pub fn leaf_count(&self) -> usize {
        match self {
            PaneTree::Leaf(_) => 1,
            PaneTree::Split { children, .. } => children.iter().map(Self::leaf_count).sum(),
        }
    }

    pub fn in_order_leaves(&self) -> Vec<L> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<L>) {
        match self {
            PaneTree::Leaf(id) => out.push(*id),
            PaneTree::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }

    /// Path of child indices from root to the named leaf, or `None` if the
    /// id isn't in the tree. Empty `Vec` means root is the leaf.
    fn path_to(&self, target: L) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        if self.path_to_inner(target, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn path_to_inner(&self, target: L, path: &mut Vec<usize>) -> bool {
        match self {
            PaneTree::Leaf(id) => *id == target,
            PaneTree::Split { children, .. } => {
                for (i, c) in children.iter().enumerate() {
                    path.push(i);
                    if c.path_to_inner(target, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
        }
    }

    /// Wrap the leaf matching `target` in a `Split { axis, [old, new] }` —
    /// each split halves the focused leaf's slice between old and new.
    /// Binary-split semantics. The new leaf goes AFTER the old (right for
    /// horizontal, below for vertical). Returns true on success.
    pub fn split_leaf(&mut self, target: L, axis: Axis, new_id: L) -> bool {
        self.split_leaf_at(target, axis, new_id, SplitInsert::After)
    }

    /// Like [`split_leaf`] but lets the caller pick whether the new leaf
    /// goes before or after the focused leaf along the new axis. Drives the
    /// four-direction menu: Right/Down = After, Left/Up = Before.
    pub fn split_leaf_at(&mut self, target: L, axis: Axis, new_id: L, insert: SplitInsert) -> bool {
        let Some(path) = self.path_to(target) else {
            return false;
        };
        let node = descend_mut(self, &path);
        // Placeholder: any valid PaneTree works; we overwrite immediately.
        let placeholder = PaneTree::Split {
            axis: Axis::Horizontal,
            children: Vec::new(),
            weights: Vec::new(),
        };
        let old = std::mem::replace(node, placeholder);
        let children = match insert {
            SplitInsert::After => vec![old, PaneTree::Leaf(new_id)],
            SplitInsert::Before => vec![PaneTree::Leaf(new_id), old],
        };
        *node = PaneTree::Split {
            axis,
            children,
            // 50/50 — divider drag mutates this in place.
            weights: vec![1.0, 1.0],
        };
        true
    }

    /// Replace the weights at the Split node addressed by `path`. Returns
    /// `true` when the path lands on a `Split` whose `children.len()` matches
    /// `new_weights.len()`. Weights are clamped per-entry to `MIN_FLEX` so a
    /// drag can't shrink a leaf to zero.
    pub fn set_split_weights(&mut self, path: &[usize], new_weights: Vec<f32>) -> bool {
        let node = descend_mut(self, path);
        match node {
            PaneTree::Split {
                children, weights, ..
            } if children.len() == new_weights.len() => {
                for (slot, w) in weights.iter_mut().zip(new_weights) {
                    *slot = w.max(MIN_FLEX);
                }
                true
            }
            _ => false,
        }
    }

    /// Read-only view of weights at the Split addressed by `path`. Used by
    /// the divider drag handler to capture initial weights as part of the
    /// drag payload.
    pub fn split_weights(&self, path: &[usize]) -> Option<Vec<f32>> {
        let node = descend(self, path)?;
        match node {
            PaneTree::Split { weights, .. } => Some(weights.clone()),
            PaneTree::Leaf(_) => None,
        }
    }

    /// Remove the leaf matching `target` and collapse any single-child
    /// Splits. Returns false when the target is the root leaf (caller must
    /// guard "only one leaf remains") or not in the tree.
    pub fn remove_leaf(&mut self, target: L) -> bool {
        let Some(path) = self.path_to(target) else {
            return false;
        };
        if path.is_empty() {
            return false;
        }
        let parent = descend_mut(self, &path[..path.len() - 1]);
        let last = *path.last().unwrap();
        match parent {
            PaneTree::Split {
                children, weights, ..
            } => {
                // Strict invariant: `weights.len() == children.len()` at all
                // times. An earlier guarded `if last < weights.len()` here
                // was unsafe — a missing weight would silently desync the
                // arrays, and downstream zip-iteration would drop the
                // trailing child from render AND PTY dispatch. The
                // unconditional remove preserves the invariant; if the
                // invariant ever breaks elsewhere, this will panic loudly
                // rather than silently lose a leaf.
                children.remove(last);
                weights.remove(last);
            }
            PaneTree::Leaf(_) => unreachable!("path leads through Split nodes"),
        }
        self.collapse_singletons();
        true
    }

    /// Walk the tree recursively and panic if any `Split` node has
    /// `children.len() != weights.len()`. Cheap O(tree size) check, gated
    /// on `debug_assertions` so release builds skip it.
    #[cfg(debug_assertions)]
    pub fn debug_assert_invariants(&self) {
        match self {
            PaneTree::Leaf(_) => {}
            PaneTree::Split {
                children, weights, ..
            } => {
                assert_eq!(
                    children.len(),
                    weights.len(),
                    "PaneTree::Split invariant violated: children={} weights={}",
                    children.len(),
                    weights.len(),
                );
                for c in children {
                    c.debug_assert_invariants();
                }
            }
        }
    }

    /// Collapse any `Split` node with exactly one child into that child,
    /// recursively. Splits with zero children should not occur in a
    /// well-formed tree.
    fn collapse_singletons(&mut self) {
        if let PaneTree::Split { children, .. } = self {
            for c in children.iter_mut() {
                c.collapse_singletons();
            }
        }
        let demote = matches!(self, PaneTree::Split { children, .. } if children.len() == 1);
        if demote {
            let only = match self {
                PaneTree::Split { children, .. } => children.remove(0),
                _ => unreachable!(),
            };
            *self = only;
        }
    }
}

/// Immutable counterpart of `descend_mut` for `split_weights`.
fn descend<'a, L>(root: &'a PaneTree<L>, path: &[usize]) -> Option<&'a PaneTree<L>> {
    let mut node = root;
    for &i in path {
        node = match node {
            PaneTree::Split { children, .. } => children.get(i)?,
            PaneTree::Leaf(_) => return None,
        };
    }
    Some(node)
}

fn descend_mut<'a, L>(root: &'a mut PaneTree<L>, path: &[usize]) -> &'a mut PaneTree<L> {
    let mut node = root;
    for &i in path {
        node = match node {
            PaneTree::Split { children, .. } => &mut children[i],
            PaneTree::Leaf(_) => unreachable!("path is invariant on internal Split nodes"),
        };
    }
    node
}

#[cfg(test)]
mod tests {
    //! Pure-structure smoke tests — exercise split / remove / focus-order
    //! math without a real GPUI app.

    use super::*;

    fn id(n: u64) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn single_leaf_has_count_one() {
        let t = PaneTree::Leaf(id(0));
        assert_eq!(t.leaf_count(), 1);
        assert_eq!(t.in_order_leaves(), vec![id(0)]);
    }

    #[test]
    fn split_horizontal_creates_two_leaves() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf(id(0), Axis::Horizontal, id(1)));
        assert_eq!(t.leaf_count(), 2);
        assert_eq!(t.in_order_leaves(), vec![id(0), id(1)]);
        match &t {
            PaneTree::Split { axis, weights, .. } => {
                assert_eq!(*axis, Axis::Horizontal);
                // Fresh split is 50/50 — divider drag mutates this in place.
                assert_eq!(weights, &vec![1.0, 1.0]);
            }
            _ => panic!("expected split root"),
        }
    }

    #[test]
    fn split_leaf_at_before_places_new_pane_first() {
        // Left/Up splits use SplitInsert::Before so the new pane lands on
        // the leading side of the focused leaf along the new axis.
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf_at(id(0), Axis::Horizontal, id(1), SplitInsert::Before));
        // in_order_leaves walks children left-to-right, so the new pane (1)
        // must come BEFORE the original (0).
        assert_eq!(t.in_order_leaves(), vec![id(1), id(0)]);
    }

    #[test]
    fn split_leaf_at_after_matches_legacy_semantics() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf_at(id(0), Axis::Vertical, id(1), SplitInsert::After));
        assert_eq!(t.in_order_leaves(), vec![id(0), id(1)]);
    }

    #[test]
    fn set_split_weights_clamps_and_returns_true() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf(id(0), Axis::Horizontal, id(1)));
        // Empty path lands on the root Split.
        assert!(t.set_split_weights(&[], vec![1.5, 0.5]));
        assert_eq!(t.split_weights(&[]), Some(vec![1.5, 0.5]));
        // Below-MIN_FLEX is clamped up, not rejected.
        assert!(t.set_split_weights(&[], vec![1.95, 0.0]));
        let w = t.split_weights(&[]).unwrap();
        assert_eq!(w[0], 1.95);
        assert_eq!(w[1], MIN_FLEX);
    }

    #[test]
    fn set_split_weights_rejects_mismatched_len() {
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        assert!(!t.set_split_weights(&[], vec![1.0]));
    }

    #[test]
    fn remove_leaf_drops_corresponding_weight() {
        // Build [0 | 1 | 2], then remove leaf 1 → [0, 2] with weights [1, 1].
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        t.split_leaf(id(1), Axis::Horizontal, id(2));
        // Inner Split is at path [1] (right child of root). Its children are
        // [Leaf(1), Leaf(2)] with weights [1, 1]. Remove leaf 1 → that
        // inner Split collapses to Leaf(2). Root weights survive unchanged.
        assert!(t.remove_leaf(id(1)));
        assert_eq!(t.in_order_leaves(), vec![id(0), id(2)]);
        let root_weights = t.split_weights(&[]).expect("root should still be split");
        assert_eq!(root_weights.len(), 2);
        assert_eq!(root_weights, vec![1.0, 1.0]);
    }

    #[test]
    fn nested_split_2x2() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(t.split_leaf(id(0), Axis::Horizontal, id(1)));
        assert!(t.split_leaf(id(1), Axis::Vertical, id(2)));
        assert_eq!(t.leaf_count(), 3);
        assert_eq!(t.in_order_leaves(), vec![id(0), id(1), id(2)]);
    }

    #[test]
    fn split_missing_target_returns_false() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(!t.split_leaf(id(99), Axis::Horizontal, id(1)));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_leaf_collapses_single_child_split() {
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        assert!(t.remove_leaf(id(1)));
        assert_eq!(t, PaneTree::Leaf(id(0)));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn remove_root_leaf_returns_false() {
        let mut t = PaneTree::Leaf(id(0));
        assert!(!t.remove_leaf(id(0)));
        assert_eq!(t.leaf_count(), 1);
    }

    #[test]
    fn sequential_splits_cascade_binary() {
        // Each Cmd-D on the freshly-spawned right pane halves it again.
        // Three splits → [0 | [1 | [2 | 3]]] with 0=50%, 1=25%, 2=12.5%, 3=12.5%.
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        t.split_leaf(id(1), Axis::Horizontal, id(2));
        t.split_leaf(id(2), Axis::Horizontal, id(3));
        assert_eq!(t.leaf_count(), 4);
        assert_eq!(t.in_order_leaves(), vec![id(0), id(1), id(2), id(3)]);
        // Walk: root.children[1].children[1].children == [Leaf(2), Leaf(3)].
        let lvl1 = match &t {
            PaneTree::Split { children, .. } => &children[1],
            _ => panic!("expected root split"),
        };
        let lvl2 = match lvl1 {
            PaneTree::Split { children, .. } => &children[1],
            _ => panic!("expected nested split"),
        };
        match lvl2 {
            PaneTree::Split {
                children, weights, ..
            } => {
                assert_eq!(
                    children,
                    &vec![PaneTree::Leaf(id(2)), PaneTree::Leaf(id(3))]
                );
                assert_eq!(weights, &vec![1.0, 1.0]);
            }
            _ => panic!("expected innermost split"),
        }
    }

    #[test]
    fn remove_nested_leaf_preserves_siblings() {
        // Start: Split-H [0, Split-V [1, 2]]. Remove 1 → Split-H [0, 2].
        let mut t = PaneTree::Leaf(id(0));
        t.split_leaf(id(0), Axis::Horizontal, id(1));
        t.split_leaf(id(1), Axis::Vertical, id(2));
        assert!(t.remove_leaf(id(1)));
        assert_eq!(t.in_order_leaves(), vec![id(0), id(2)]);
        match &t {
            PaneTree::Split { axis, children, .. } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected horizontal split root"),
        }
    }

    /// Regression: Codex adversarial-review flagged a soundness gap where a
    /// `weights` vector shorter than `children` would silently truncate at
    /// `zip` in `build_node` / `dispatch_grids_inner`, hiding a pane from
    /// both render and PTY dispatch. We now (a) keep `weights.len() ==
    /// children.len()` strictly across every mutation and (b)
    /// `debug_assert!` the invariant before render. This test asserts (a)
    /// across the full mutation surface.
    /// Genericity smoke: the same `PaneTree` API has to work for both
    /// content leaves (`PaneTree<PaneId>`) and the workspace's group
    /// layout (`PaneTree<PaneGroupId>`). This test pins the latter so a
    /// future change to the trait bounds can't silently break the
    /// workspace-level split tree.
    #[test]
    fn split_works_for_pane_group_id_leaf_type() {
        let mut t: PaneTree<PaneGroupId> = PaneTree::Leaf(PaneGroupId(0));
        assert!(t.split_leaf(PaneGroupId(0), Axis::Horizontal, PaneGroupId(1)));
        assert_eq!(t.leaf_count(), 2);
        assert_eq!(t.in_order_leaves(), vec![PaneGroupId(0), PaneGroupId(1)]);
        assert!(t.remove_leaf(PaneGroupId(1)));
        assert_eq!(t, PaneTree::Leaf(PaneGroupId(0)));
    }

    #[test]
    fn split_invariant_holds_across_mutations() {
        let mut t = PaneTree::Leaf(id(0));
        t.debug_assert_invariants();
        // split → 2 children + 2 weights
        assert!(t.split_leaf(id(0), Axis::Horizontal, id(1)));
        t.debug_assert_invariants();
        // nested split → 2 + 2 at root, 2 + 2 at inner
        assert!(t.split_leaf(id(1), Axis::Vertical, id(2)));
        t.debug_assert_invariants();
        // three-deep cascade — exercises the longest path
        assert!(t.split_leaf(id(2), Axis::Horizontal, id(3)));
        t.debug_assert_invariants();
        // set_split_weights at root + at a nested Split
        assert!(t.set_split_weights(&[], vec![2.0, 1.0]));
        t.debug_assert_invariants();
        // remove a leaf — historical bug site (conditional weight removal)
        assert!(t.remove_leaf(id(3)));
        t.debug_assert_invariants();
        // remove again → collapse_singletons fires
        assert!(t.remove_leaf(id(2)));
        t.debug_assert_invariants();
    }
}
