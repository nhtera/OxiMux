//! Sub-pane tree for a single terminal tab.
//!
//! A `TerminalSplitTree` lets one terminal tab hold MULTIPLE PTYs in a
//! split layout — `Cmd+D` carves a column, `Cmd+Shift+D` carves a row.
//! Matches iTerm / the reference terminal / the reference editor's sub-pane model.
//!
//! Layout state lives in the `PaneTree<usize>` whose leaves index into
//! the `panes` vector. Closing a sub-pane sets its slot to `None` and
//! removes its leaf; the vector never shifts so existing leaf indices
//! stay valid. Singletons collapse automatically (one-child Split nodes
//! demote to their child).

use gpui::{Entity, Subscription};

use crate::shell::pane_tree::{Axis, PaneTree, SplitInsert};
use crate::shell::terminal_view::TerminalView;

/// One terminal tab's sub-pane state. Always non-empty in a well-formed
/// tab — when the last sub-pane closes, the owning `PaneGroup` should
/// close the tab itself rather than leaving an empty `TerminalSplitTree`.
pub struct TerminalSplitTree {
    /// Insertion-order vector of sub-pane views. Index slots are NEVER
    /// reused — closing a sub-pane sets its slot to `None` and the tree
    /// drops the corresponding leaf. Keeping `panes.len()` monotonic
    /// preserves leaf-index validity in the tree.
    panes: Vec<Option<Entity<TerminalView>>>,
    /// Layout tree whose leaves are indices into `panes`. Singletons
    /// collapse on every mutation via `PaneTree::collapse_singletons`.
    tree: PaneTree<usize>,
    /// Active sub-pane index — drives keyboard input routing, the
    /// "focused" rim glow, and Cmd+W disambiguation.
    active: usize,
    /// Observers parallel to `panes`. Holding the `Subscription` keeps
    /// the parent `PaneGroup` re-rendering on inner-view notifications;
    /// `None` slots correspond to closed sub-panes.
    observers: Vec<Option<Subscription>>,
}

impl TerminalSplitTree {
    /// Construct a tree with a single sub-pane.
    pub fn new_single(view: Entity<TerminalView>, observer: Subscription) -> Self {
        Self {
            panes: vec![Some(view)],
            tree: PaneTree::Leaf(0),
            active: 0,
            observers: vec![Some(observer)],
        }
    }

    /// Number of LIVE sub-panes. Closed slots don't count.
    pub fn live_count(&self) -> usize {
        self.panes.iter().filter(|p| p.is_some()).count()
    }

    /// Read-only view of the layout tree. Render walks this.
    pub fn tree(&self) -> &PaneTree<usize> {
        &self.tree
    }

    /// Active sub-pane index — even after splits/closes, points at a
    /// valid slot in `panes`. Callers should re-resolve via `get` since
    /// a stale closure could observe a closed slot.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Borrow a sub-pane's view by index. Returns `None` for closed
    /// slots or out-of-range indices.
    pub fn get(&self, idx: usize) -> Option<&Entity<TerminalView>> {
        self.panes.get(idx).and_then(|slot| slot.as_ref())
    }

    /// Borrow the active sub-pane's view. `None` only if the tree was
    /// mutated into an inconsistent state (active points at a closed
    /// slot) — callers should treat that as a logic bug.
    pub fn active_view(&self) -> Option<&Entity<TerminalView>> {
        self.get(self.active)
    }

    /// Iterate every LIVE (index, view) pair in insertion order.
    /// Render uses this to mount each pane; persistence uses it to
    /// serialize scrollback per sub-pane.
    pub fn iter_live(&self) -> impl Iterator<Item = (usize, &Entity<TerminalView>)> + '_ {
        self.panes
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|v| (i, v)))
    }

    /// Split the active sub-pane along `axis`, inserting `new_view` on
    /// the chosen side. Returns the new sub-pane's index so the caller
    /// can hand its `Subscription` along immediately.
    pub fn split_active(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        new_view: Entity<TerminalView>,
        new_observer: Subscription,
    ) -> usize {
        let new_idx = self.panes.len();
        self.panes.push(Some(new_view));
        self.observers.push(Some(new_observer));
        let inserted = self.tree.split_leaf_at(self.active, axis, new_idx, insert);
        debug_assert!(inserted, "active sub-pane idx not found in tree");
        self.active = new_idx;
        new_idx
    }

    /// Close the active sub-pane. No-op when this is the LAST live
    /// pane — caller should close the owning tab instead. Returns
    /// `true` when the close happened; `false` signals "last pane, you
    /// handle it".
    pub fn close_active(&mut self) -> bool {
        if self.live_count() <= 1 {
            return false;
        }
        let closed = self.active;
        // Drop view + observer first so the PTY winds down before we
        // mutate the tree (avoids re-render seeing an orphan leaf).
        if let Some(slot) = self.panes.get_mut(closed) {
            *slot = None;
        }
        if let Some(obs) = self.observers.get_mut(closed) {
            *obs = None;
        }
        self.tree.remove_leaf(closed);
        // Pick a new active: first surviving leaf in in-order traversal.
        if let Some(next) = self.tree.in_order_leaves().first().copied() {
            self.active = next;
        }
        true
    }

    /// Mark `idx` as active. No-op for closed slots or out-of-range
    /// indices. Used by the rim-glow click handler to follow mouse
    /// focus inside the split layout.
    pub fn set_active(&mut self, idx: usize) {
        if self.get(idx).is_some() {
            self.active = idx;
        }
    }

    /// Cycle focus to the next (or previous, when `forward == false`)
    /// live sub-pane in in-order traversal. Wraps around. No-op when
    /// fewer than 2 sub-panes are live.
    pub fn cycle_focus(&mut self, forward: bool) {
        let leaves = self.tree.in_order_leaves();
        if leaves.len() < 2 {
            return;
        }
        let current = leaves.iter().position(|&i| i == self.active).unwrap_or(0);
        let next = if forward {
            (current + 1) % leaves.len()
        } else {
            (current + leaves.len() - 1) % leaves.len()
        };
        self.active = leaves[next];
    }
}
