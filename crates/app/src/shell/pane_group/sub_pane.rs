//! Sub-pane tree for a single terminal tab.
//!
//! A `TerminalSplitTree` lets one terminal tab hold MULTIPLE PTYs in a
//! split layout — `Cmd+D` carves a column, `Cmd+Shift+D` carves a row.
//! Matches the sub-pane model of common terminals and the reference editor.
//!
//! Each split surface (leaf) is itself a small TAB CONTAINER ([`LeafTabs`]):
//! it holds one or more terminals and renders its own compact tab bar.
//! Most of the time a leaf has exactly one tab, in which case it behaves
//! exactly like a plain single-terminal pane; adding a tab to a leaf
//! (the per-pane `+`) pushes another terminal into the same surface.
//!
//! Layout state lives in the `PaneTree<usize>` whose leaves index into
//! the `panes` vector. Closing a leaf sets its slot to `None` and removes
//! its tree leaf; the vector never shifts so existing leaf indices stay
//! valid. Singletons collapse automatically (one-child Split nodes demote
//! to their child).

use gpui::{Entity, Subscription};

use crate::persisted_terminals::{PersistedTree, restore_tree};
use crate::shell::pane_tree::{Axis, PaneTree, SplitInsert};
use crate::shell::terminal_view::TerminalView;

/// One terminal inside a leaf's tab strip. The `Subscription` keeps the
/// owning `PaneGroup` re-rendering on this view's notifications.
pub struct LeafTab {
    view: Entity<TerminalView>,
    _observer: Subscription,
}

impl LeafTab {
    pub fn view(&self) -> &Entity<TerminalView> {
        &self.view
    }
}

/// A single split surface: a stack of terminal tabs with one active.
/// Always non-empty in a well-formed tree — when the last tab in a leaf
/// closes, the leaf itself is removed from the tree.
pub struct LeafTabs {
    tabs: Vec<LeafTab>,
    active: usize,
}

impl LeafTabs {
    fn single(view: Entity<TerminalView>, observer: Subscription) -> Self {
        Self {
            tabs: vec![LeafTab {
                view,
                _observer: observer,
            }],
            active: 0,
        }
    }

    /// Number of tabs in this leaf (always >= 1).
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Active tab index.
    pub fn active(&self) -> usize {
        self.active
    }

    /// The active tab's view.
    pub fn active_view(&self) -> &Entity<TerminalView> {
        &self.tabs[self.active].view
    }

    /// Read-only access to the tabs for the per-pane tab bar.
    pub fn tabs(&self) -> &[LeafTab] {
        &self.tabs
    }

    /// Append a tab and make it active. Returns the new tab index.
    fn push(&mut self, view: Entity<TerminalView>, observer: Subscription) -> usize {
        self.tabs.push(LeafTab {
            view,
            _observer: observer,
        });
        self.active = self.tabs.len() - 1;
        self.active
    }

    /// Switch the active tab. No-op for out-of-range indices.
    fn set_active(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active = idx;
        }
    }

    /// Close the active tab. Returns `false` when it was the LAST tab
    /// (caller removes the whole leaf instead). On success the active
    /// index clamps to the previous tab.
    fn close_active(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        true
    }

    /// Cycle the active tab forward/backward, wrapping. No-op with <2 tabs.
    fn cycle(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        self.active = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
    }
}

/// One terminal tab's sub-pane state. Always non-empty in a well-formed
/// tab — when the last sub-pane closes, the owning `PaneGroup` should
/// close the tab itself rather than leaving an empty `TerminalSplitTree`.
pub struct TerminalSplitTree {
    /// Insertion-order vector of leaf tab-containers. Index slots are
    /// NEVER reused — closing a leaf sets its slot to `None` and the tree
    /// drops the corresponding leaf. Keeping `panes.len()` monotonic
    /// preserves leaf-index validity in the tree.
    panes: Vec<Option<LeafTabs>>,
    /// Layout tree whose leaves are indices into `panes`. Singletons
    /// collapse on every mutation via `PaneTree::collapse_singletons`.
    tree: PaneTree<usize>,
    /// Active leaf index — drives keyboard input routing, the "focused"
    /// rim glow, and Cmd+W disambiguation.
    active: usize,
    /// When `Some(idx)`, render bypasses the tree and shows ONLY the
    /// named leaf at full body size — Cmd+Shift+Enter "zoom" toggle.
    /// Other leaves stay alive (PTYs continue running, scrollback
    /// accumulates) so a second toggle restores the prior layout.
    zoomed: Option<usize>,
}

impl TerminalSplitTree {
    /// Construct a tree with a single sub-pane (one leaf, one tab).
    pub fn new_single(view: Entity<TerminalView>, observer: Subscription) -> Self {
        Self {
            panes: vec![Some(LeafTabs::single(view, observer))],
            tree: PaneTree::Leaf(0),
            active: 0,
            zoomed: None,
        }
    }

    /// Restore-side constructor. `tree_proto` carries the shape (axes +
    /// weights); `leaves` is the per-leaf (view, observer) pair in DFS
    /// visit order; `active_dfs_pos` is the active leaf's DFS position.
    /// Each restored leaf starts with a single tab; multi-tab leaves are
    /// rehydrated separately by the factory.
    ///
    /// The restorer walks `tree_proto` in DFS order and allocates slot
    /// ids `0..N` in the same order, so the i-th DFS leaf binds to
    /// `leaves[i]` AND to slot `i` in the resulting `panes` vec. The
    /// active sub-pane index is therefore identical to the DFS
    /// position — no remapping needed.
    ///
    /// `leaves.len()` must equal `count_leaves(tree_proto)`; debug-asserted.
    pub fn from_persisted(
        tree_proto: &PersistedTree,
        leaves: Vec<(Entity<TerminalView>, Subscription)>,
        active_dfs_pos: usize,
    ) -> Self {
        let mut next_slot: usize = 0;
        let tree = restore_tree::<usize, _>(tree_proto, &mut || {
            let s = next_slot;
            next_slot += 1;
            s
        });
        debug_assert_eq!(
            next_slot,
            leaves.len(),
            "leaves count must match tree leaf count"
        );
        let mut panes: Vec<Option<LeafTabs>> = Vec::with_capacity(leaves.len());
        for (view, obs) in leaves {
            panes.push(Some(LeafTabs::single(view, obs)));
        }
        let active = active_dfs_pos.min(panes.len().saturating_sub(1));
        Self {
            panes,
            tree,
            active,
            zoomed: None,
        }
    }

    /// Currently-zoomed sub-pane index, if any. Render uses this to
    /// short-circuit tree traversal and mount only the zoomed pane.
    pub fn zoomed(&self) -> Option<usize> {
        self.zoomed
    }

    /// Toggle zoom on the active sub-pane. When the active pane is
    /// already zoomed, restore the tree layout. No-op when there's only
    /// one live pane (nothing to zoom away from).
    pub fn toggle_zoom_active(&mut self) {
        if self.live_count() <= 1 {
            self.zoomed = None;
            return;
        }
        if self.zoomed.is_some() {
            self.zoomed = None;
        } else {
            self.zoomed = Some(self.active);
        }
    }

    /// Number of LIVE sub-panes (leaves). Closed slots don't count.
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

    /// Borrow a leaf's ACTIVE tab view by leaf index. Returns `None` for
    /// closed slots or out-of-range indices. This preserves the historic
    /// "one view per leaf" contract for every caller that just wants the
    /// terminal a leaf is showing.
    pub fn get(&self, idx: usize) -> Option<&Entity<TerminalView>> {
        self.panes
            .get(idx)
            .and_then(|slot| slot.as_ref())
            .map(|leaf| leaf.active_view())
    }

    /// Borrow a leaf's full tab container by leaf index. Used by the
    /// per-pane tab bar render + tab operations.
    pub fn leaf(&self, idx: usize) -> Option<&LeafTabs> {
        self.panes.get(idx).and_then(|slot| slot.as_ref())
    }

    /// Borrow the active leaf's tab container.
    pub fn active_leaf(&self) -> Option<&LeafTabs> {
        self.leaf(self.active)
    }

    /// Borrow the active sub-pane's view. `None` only if the tree was
    /// mutated into an inconsistent state (active points at a closed
    /// slot) — callers should treat that as a logic bug.
    pub fn active_view(&self) -> Option<&Entity<TerminalView>> {
        self.get(self.active)
    }

    /// Iterate every LIVE (leaf index, active-tab view) pair in
    /// insertion order. Render uses this to mount each leaf; the
    /// attention check uses it to poll active views.
    pub fn iter_live(&self) -> impl Iterator<Item = (usize, &Entity<TerminalView>)> + '_ {
        self.panes
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|leaf| (i, leaf.active_view())))
    }

    /// Iterate EVERY live terminal across all leaves and their tabs:
    /// `(leaf index, tab index, view)`. Persistence + scrollback capture
    /// use this so every tab in every pane is saved, not just the
    /// active one.
    pub fn iter_all_views(
        &self,
    ) -> impl Iterator<Item = (usize, usize, &Entity<TerminalView>)> + '_ {
        self.panes.iter().enumerate().flat_map(|(slot, opt)| {
            opt.as_ref().into_iter().flat_map(move |leaf| {
                leaf.tabs
                    .iter()
                    .enumerate()
                    .map(move |(t, lt)| (slot, t, &lt.view))
            })
        })
    }

    /// Split the active sub-pane along `axis`, inserting `new_view` on
    /// the chosen side as a fresh single-tab leaf. Returns the new
    /// sub-pane's index so the caller can act on it immediately.
    pub fn split_active(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        new_view: Entity<TerminalView>,
        new_observer: Subscription,
    ) -> usize {
        let new_idx = self.panes.len();
        self.panes.push(Some(LeafTabs::single(new_view, new_observer)));
        let inserted = self.tree.split_leaf_at(self.active, axis, new_idx, insert);
        debug_assert!(inserted, "active sub-pane idx not found in tree");
        self.active = new_idx;
        new_idx
    }

    /// Add a tab to the ACTIVE leaf and make it active within that leaf.
    /// The leaf stays in place in the layout tree — only its tab strip
    /// grows. Returns the new tab's index inside the leaf, or `None` if
    /// the active slot is somehow closed.
    pub fn add_tab_to_active(
        &mut self,
        new_view: Entity<TerminalView>,
        new_observer: Subscription,
    ) -> Option<usize> {
        let leaf = self.panes.get_mut(self.active)?.as_mut()?;
        Some(leaf.push(new_view, new_observer))
    }

    /// Set the active TAB inside leaf `leaf_idx`, and make that leaf the
    /// active sub-pane. No-op for closed/out-of-range leaves.
    pub fn set_active_tab(&mut self, leaf_idx: usize, tab_idx: usize) {
        if let Some(Some(leaf)) = self.panes.get_mut(leaf_idx) {
            leaf.set_active(tab_idx);
            self.active = leaf_idx;
        }
    }

    /// Cycle the active leaf's tab forward/backward. No-op when the
    /// active leaf has fewer than two tabs.
    pub fn cycle_tab_in_active(&mut self, forward: bool) {
        if let Some(Some(leaf)) = self.panes.get_mut(self.active) {
            leaf.cycle(forward);
        }
    }

    /// Close the active TAB inside the active leaf. Returns `true` when a
    /// tab was closed but the leaf survives (it had >1 tab). Returns
    /// `false` when the active leaf had only one tab — the caller should
    /// fall back to `close_active` (close the whole leaf) or, if that's
    /// also the last leaf, close the owning group tab.
    pub fn close_active_tab(&mut self) -> bool {
        match self.panes.get_mut(self.active) {
            Some(Some(leaf)) => leaf.close_active(),
            _ => false,
        }
    }

    /// Close the active sub-pane (the whole leaf, all its tabs). No-op
    /// when this is the LAST live pane — caller should close the owning
    /// tab instead. Returns `true` when the close happened; `false`
    /// signals "last pane, you handle it".
    pub fn close_active(&mut self) -> bool {
        if self.live_count() <= 1 {
            return false;
        }
        let closed = self.active;
        // Drop the leaf (views + observers) first so PTYs wind down
        // before we mutate the tree (avoids re-render seeing an orphan
        // leaf).
        if let Some(slot) = self.panes.get_mut(closed) {
            *slot = None;
        }
        self.tree.remove_leaf(closed);
        // Pick a new active: first surviving leaf in in-order traversal.
        if let Some(next) = self.tree.in_order_leaves().first().copied() {
            self.active = next;
        }
        true
    }

    /// Mark `idx` as the active leaf. No-op for closed slots or
    /// out-of-range indices. Used by the rim-glow click handler to follow
    /// mouse focus inside the split layout.
    pub fn set_active(&mut self, idx: usize) {
        if self.leaf(idx).is_some() {
            self.active = idx;
        }
    }

    /// Apply new weights to the Split node at `path` inside this tab's
    /// sub-pane tree. Returns `true` when the path lands on a Split and
    /// the new vector matches its child count. `PaneTree::set_split_weights`
    /// clamps each entry to `MIN_FLEX` so a drag can't shrink a sub-pane
    /// to zero. Used by the sub-pane divider resize handle.
    pub fn set_split_weights(&mut self, path: &[usize], weights: Vec<f32>) -> bool {
        self.tree.set_split_weights(path, weights)
    }

    /// Read-only snapshot of the Split weights at `path`. Used by drag
    /// short-circuit logic to skip cx.notify when nothing changed.
    pub fn split_weights(&self, path: &[usize]) -> Option<Vec<f32>> {
        self.tree.split_weights(path)
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
