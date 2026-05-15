//! Pure-data pane tree — split / close / focus-order math for the workspace
//! grid. No GPUI types here; `MainPane` keeps the entity store in a parallel
//! `HashMap<PaneId, Entity<TerminalView>>` and looks up by id during render.
//!
//! Binary splits: every `split_leaf` wraps the target in a fresh `Split`
//! node, halving the target's slice between old and new. Matches the default
//! split semantics in the reference terminal / iTerm. Rebalancing or equal-N-way splits are
//! a separate concern (an explicit "balance" action could flatten + equalize
//! a same-axis cascade if/when the UX demands it).

/// Stable identifier for a pane leaf in the workspace tree. Issued
/// monotonically by `MainPane`; never reused after a pane closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTree {
    Leaf(PaneId),
    Split { axis: Axis, children: Vec<PaneTree> },
}

impl PaneTree {
    pub fn leaf_count(&self) -> usize {
        match self {
            PaneTree::Leaf(_) => 1,
            PaneTree::Split { children, .. } => children.iter().map(Self::leaf_count).sum(),
        }
    }

    pub fn in_order_leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
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
    fn path_to(&self, target: PaneId) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        if self.path_to_inner(target, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn path_to_inner(&self, target: PaneId, path: &mut Vec<usize>) -> bool {
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
    /// each Cmd-D halves the focused pane's slice between old and new.
    /// Matches the reference terminal / iTerm binary-split semantics. Returns true on
    /// success.
    pub fn split_leaf(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        let Some(path) = self.path_to(target) else {
            return false;
        };
        let node = descend_mut(self, &path);
        // Placeholder: any valid PaneTree works; we overwrite immediately.
        let placeholder = PaneTree::Split {
            axis: Axis::Horizontal,
            children: Vec::new(),
        };
        let old = std::mem::replace(node, placeholder);
        *node = PaneTree::Split {
            axis,
            children: vec![old, PaneTree::Leaf(new_id)],
        };
        true
    }

    /// Remove the leaf matching `target` and collapse any single-child
    /// Splits. Returns false when the target is the root leaf (caller must
    /// guard "only one pane remains") or not in the tree.
    pub fn remove_leaf(&mut self, target: PaneId) -> bool {
        let Some(path) = self.path_to(target) else {
            return false;
        };
        if path.is_empty() {
            return false;
        }
        let parent = descend_mut(self, &path[..path.len() - 1]);
        let last = *path.last().unwrap();
        match parent {
            PaneTree::Split { children, .. } => {
                children.remove(last);
            }
            PaneTree::Leaf(_) => unreachable!("path leads through Split nodes"),
        }
        self.collapse_singletons();
        true
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

fn descend_mut<'a>(root: &'a mut PaneTree, path: &[usize]) -> &'a mut PaneTree {
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
            PaneTree::Split { axis, .. } => assert_eq!(*axis, Axis::Horizontal),
            _ => panic!("expected split root"),
        }
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
            PaneTree::Split { children, .. } => {
                assert_eq!(
                    children,
                    &vec![PaneTree::Leaf(id(2)), PaneTree::Leaf(id(3))]
                );
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
            PaneTree::Split { axis, children } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("expected horizontal split root"),
        }
    }
}
