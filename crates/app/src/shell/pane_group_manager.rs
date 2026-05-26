//! Pure-data manager for the workspace's pane-group layout tree.
//!
//! The workspace holds ONE `PaneGroupManager` per project. The manager
//! owns:
//!
//! - `group_tree: GroupTree` (alias of `PaneTree<PaneGroupId>`) — the
//!   binary split tree describing how groups are arranged on screen.
//! - `active_group_id` — which leaf is "focused" for file-open routing
//!   and split-target resolution.
//! - a monotonic ID counter — new groups get fresh IDs that are never
//!   reused, matching `PaneId`'s discipline.
//!
//! This module is GPUI-free. The `PaneGroup` entity (created in step 2)
//! is registered separately and looked up by id. Split / close / focus
//! mutations land here so they can be unit-tested without a runtime.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::shell::pane_tree::{Axis, GroupTree, PaneGroupId, PaneTree, SplitInsert};

/// Outcome of a `split_active_group` call. The caller is expected to
/// allocate the actual `PaneGroup` entity for the new id and register it
/// in its own map. Returning the id keeps this module entity-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSplitOutcome {
    /// The group that was split (the one that held focus before the
    /// split). Stays at its original screen slot, half the area.
    pub source_group: PaneGroupId,
    /// Newly-allocated id for the sibling group. Caller materializes the
    /// `PaneGroup` entity for this id with its initial Terminal tab.
    pub new_group: PaneGroupId,
}

/// Reason `close_active_group` refused. The host surfaces these to the
/// user via the HUD or a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseGroupError {
    /// Last group standing — closing it would empty the workspace,
    /// which we refuse per D8 of the step-06 sub-plan.
    LastGroup,
    /// Group id was not in the tree (stale).
    NotFound,
}

pub struct PaneGroupManager {
    group_tree: GroupTree,
    active_group_id: PaneGroupId,
    next_id: AtomicU64,
}

impl PaneGroupManager {
    /// Seed the manager with a single group at id 0. The host registers
    /// a matching `PaneGroup` entity in its own map.
    pub fn new() -> Self {
        let initial = PaneGroupId(0);
        Self {
            group_tree: PaneTree::Leaf(initial),
            active_group_id: initial,
            next_id: AtomicU64::new(1),
        }
    }

    /// Rebuild a manager from a restored layout tree + active group id +
    /// the next-free id to issue. Used by the snapshot restore path so
    /// the workspace's multi-group split layout survives restart. Caller
    /// is responsible for registering matching `PaneGroup` entities in
    /// its own map; this is the pure-data half only.
    pub fn from_tree(tree: GroupTree, active: PaneGroupId, next_id: u64) -> Self {
        Self {
            group_tree: tree,
            active_group_id: active,
            next_id: AtomicU64::new(next_id),
        }
    }

    /// Read accessor for the layout tree — caller uses this to walk the
    /// tree on render. Mutations stay encapsulated.
    pub fn group_tree(&self) -> &GroupTree {
        &self.group_tree
    }

    pub fn active_group_id(&self) -> PaneGroupId {
        self.active_group_id
    }

    /// Set the active group. Caller-driven (focus event, mouse click).
    /// No-op if the id isn't in the tree.
    pub fn set_active(&mut self, id: PaneGroupId) -> bool {
        let leaves = self.group_tree.in_order_leaves();
        if !leaves.contains(&id) {
            return false;
        }
        self.active_group_id = id;
        true
    }

    /// All group ids currently in the tree, in DFS order. Useful for
    /// render and persistence walks.
    pub fn in_order_groups(&self) -> Vec<PaneGroupId> {
        self.group_tree.in_order_leaves()
    }

    /// Split the active group along `axis`, with the new sibling
    /// inserted per `insert`. Returns the source + new ids on success.
    /// Caller is expected to construct the `PaneGroup` entity for the
    /// new id and to focus it.
    pub fn split_active_group(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
    ) -> Option<GroupSplitOutcome> {
        let source = self.active_group_id;
        let new_id = PaneGroupId(self.next_id.fetch_add(1, Ordering::Relaxed));
        if !self.group_tree.split_leaf_at(source, axis, new_id, insert) {
            return None;
        }
        self.active_group_id = new_id;
        Some(GroupSplitOutcome {
            source_group: source,
            new_group: new_id,
        })
    }

    /// Split a specific (non-necessarily-active) group along `axis`.
    /// Used by drag-to-split so the new sibling lands next to the group
    /// the user dropped onto, regardless of which group held focus when
    /// the drag started. The new id is returned so the caller can
    /// register the matching `PaneGroup` entity. The active group is
    /// updated to `new_id` so the moved tab's group becomes focused.
    pub fn split_at_target(
        &mut self,
        target: PaneGroupId,
        axis: Axis,
        insert: SplitInsert,
    ) -> Option<GroupSplitOutcome> {
        let new_id = PaneGroupId(self.next_id.fetch_add(1, Ordering::Relaxed));
        if !self.group_tree.split_leaf_at(target, axis, new_id, insert) {
            return None;
        }
        self.active_group_id = new_id;
        Some(GroupSplitOutcome {
            source_group: target,
            new_group: new_id,
        })
    }

    /// Replace the weights at the Split node addressed by `path`. Returns
    /// `true` when the path lands on a valid Split and the new weights
    /// vector matches its child count. Used by the drag-resize handler
    /// on dividers between sibling groups; `PaneTree::set_split_weights`
    /// clamps each entry to `MIN_FLEX` so a drag can't shrink a group to
    /// zero.
    pub fn set_split_weights(&mut self, path: &[usize], new_weights: Vec<f32>) -> bool {
        self.group_tree.set_split_weights(path, new_weights)
    }

    /// Remove the active group. Last-group close is refused per D8 of
    /// the step-06 sub-plan; the caller is expected to surface that to
    /// the user (or silently ignore). On success, focus shifts to the
    /// group that slid into the closed group's slot via `PaneTree`'s
    /// existing collapse-singletons logic — the caller picks the new
    /// active id by re-reading `in_order_groups`.
    pub fn close_active_group(&mut self) -> Result<PaneGroupId, CloseGroupError> {
        let id = self.active_group_id;
        if self.group_tree.leaf_count() <= 1 {
            return Err(CloseGroupError::LastGroup);
        }
        // Capture the closed group's visual position BEFORE removing it so
        // we can refocus the sibling on its left (the user's intuition:
        // closing a group sends focus back the way the eye reads). Falls
        // back to the new leftmost survivor when the closed group was
        // already leftmost.
        let closed_visual_idx = self
            .group_tree
            .in_order_leaves()
            .iter()
            .position(|leaf| *leaf == id);
        if !self.group_tree.remove_leaf(id) {
            return Err(CloseGroupError::NotFound);
        }
        let survivors = self.group_tree.in_order_leaves();
        let new_active = match closed_visual_idx {
            // `idx > 0` → left neighbor at `idx - 1` (survivors share the
            // pre-removal layout because remove_leaf only drops one entry).
            Some(idx) if idx > 0 => survivors
                .get(idx - 1)
                .copied()
                .unwrap_or_else(|| survivors[0]),
            // Closed group was leftmost OR not found (shouldn't happen
            // after the remove_leaf above succeeded) → focus the new
            // leftmost survivor. `leaf_count > 1` above guarantees a
            // survivor exists.
            _ => *survivors
                .first()
                .expect("leaf_count > 1 above guarantees at least one survivor"),
        };
        self.active_group_id = new_active;
        Ok(id)
    }
}

impl Default for PaneGroupManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_seeds_single_group_at_id_zero() {
        let mgr = PaneGroupManager::new();
        assert_eq!(mgr.active_group_id(), PaneGroupId(0));
        assert_eq!(mgr.in_order_groups(), vec![PaneGroupId(0)]);
    }

    #[test]
    fn split_active_returns_source_and_new_ids() {
        let mut mgr = PaneGroupManager::new();
        let outcome = mgr
            .split_active_group(Axis::Horizontal, SplitInsert::After)
            .expect("split should succeed");
        assert_eq!(outcome.source_group, PaneGroupId(0));
        assert_eq!(outcome.new_group, PaneGroupId(1));
        // Active focus follows the new sibling (industry-standard
        // editor convention for fresh splits).
        assert_eq!(mgr.active_group_id(), PaneGroupId(1));
        assert_eq!(mgr.in_order_groups(), vec![PaneGroupId(0), PaneGroupId(1)]);
    }

    #[test]
    fn cascading_splits_allocate_unique_ids() {
        let mut mgr = PaneGroupManager::new();
        let a = mgr
            .split_active_group(Axis::Horizontal, SplitInsert::After)
            .unwrap()
            .new_group;
        let b = mgr
            .split_active_group(Axis::Vertical, SplitInsert::After)
            .unwrap()
            .new_group;
        let c = mgr
            .split_active_group(Axis::Horizontal, SplitInsert::Before)
            .unwrap()
            .new_group;
        assert_eq!(a, PaneGroupId(1));
        assert_eq!(b, PaneGroupId(2));
        assert_eq!(c, PaneGroupId(3));
        assert_eq!(mgr.group_tree().leaf_count(), 4);
    }

    #[test]
    fn close_last_group_refuses() {
        let mut mgr = PaneGroupManager::new();
        assert_eq!(mgr.close_active_group(), Err(CloseGroupError::LastGroup));
        // State unchanged.
        assert_eq!(mgr.active_group_id(), PaneGroupId(0));
    }

    #[test]
    fn close_non_last_group_collapses_layout() {
        let mut mgr = PaneGroupManager::new();
        mgr.split_active_group(Axis::Horizontal, SplitInsert::After);
        // Active = PaneGroupId(1). Close it.
        let closed = mgr.close_active_group().expect("non-last close succeeds");
        assert_eq!(closed, PaneGroupId(1));
        // Layout collapses back to a single leaf at id 0 (the survivor).
        assert_eq!(mgr.group_tree().leaf_count(), 1);
        assert_eq!(mgr.active_group_id(), PaneGroupId(0));
    }

    #[test]
    fn close_rightmost_focuses_left_sibling() {
        // Three-group layout: [0, 1, 2]. Close 2 → 1 should become
        // active (left-of-removed), not 0 (leftmost).
        let mut mgr = PaneGroupManager::new();
        mgr.split_active_group(Axis::Horizontal, SplitInsert::After); // 0,1; active=1
        mgr.split_active_group(Axis::Horizontal, SplitInsert::After); // 0,1,2; active=2
        assert_eq!(
            mgr.in_order_groups(),
            vec![PaneGroupId(0), PaneGroupId(1), PaneGroupId(2)]
        );
        let closed = mgr.close_active_group().expect("non-last close succeeds");
        assert_eq!(closed, PaneGroupId(2));
        // Left-of-removed = PaneGroupId(1). NOT 0.
        assert_eq!(mgr.active_group_id(), PaneGroupId(1));
    }

    #[test]
    fn close_leftmost_focuses_new_leftmost() {
        // Three groups [0, 1, 2]. Close 0 → no left sibling, so focus
        // the new leftmost which is 1.
        let mut mgr = PaneGroupManager::new();
        mgr.split_active_group(Axis::Horizontal, SplitInsert::After); // 0,1; active=1
        mgr.split_active_group(Axis::Horizontal, SplitInsert::After); // 0,1,2; active=2
        assert!(mgr.set_active(PaneGroupId(0)));
        let closed = mgr.close_active_group().expect("non-last close succeeds");
        assert_eq!(closed, PaneGroupId(0));
        // No left of leftmost → new leftmost survivor.
        assert_eq!(mgr.active_group_id(), PaneGroupId(1));
    }

    #[test]
    fn from_tree_seeds_layout_and_active() {
        // Workspace restore handoff: rebuild a manager from a known
        // tree + active id + next-issued id. The tree's leaves drive
        // `in_order_groups`, the active id sets focus, and the next-id
        // counter resumes monotonic allocation past whatever the
        // restorer used.
        let tree: GroupTree = PaneTree::Split {
            axis: Axis::Horizontal,
            children: vec![
                PaneTree::Leaf(PaneGroupId(7)),
                PaneTree::Leaf(PaneGroupId(8)),
            ],
            weights: vec![1.0, 1.0],
        };
        let mut mgr = PaneGroupManager::from_tree(tree, PaneGroupId(8), 9);
        assert_eq!(mgr.active_group_id(), PaneGroupId(8));
        assert_eq!(mgr.in_order_groups(), vec![PaneGroupId(7), PaneGroupId(8)]);
        // Next split allocates the saved `next_id` (9), not 1.
        let outcome = mgr
            .split_active_group(Axis::Vertical, SplitInsert::After)
            .expect("split should succeed");
        assert_eq!(outcome.new_group, PaneGroupId(9));
    }

    #[test]
    fn set_active_accepts_known_id_rejects_stale() {
        let mut mgr = PaneGroupManager::new();
        mgr.split_active_group(Axis::Horizontal, SplitInsert::After);
        assert!(mgr.set_active(PaneGroupId(0)));
        assert_eq!(mgr.active_group_id(), PaneGroupId(0));
        // Unknown id leaves state untouched.
        assert!(!mgr.set_active(PaneGroupId(999)));
        assert_eq!(mgr.active_group_id(), PaneGroupId(0));
    }
}
