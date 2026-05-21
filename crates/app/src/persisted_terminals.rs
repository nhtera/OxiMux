//! Per-project terminal-tab persistence.
//!
//! Stored as a single JSON blob in the `settings` table under the key
//! `terminal_tabs:<project_id>` (size cap 64 KiB — generous for dozens of
//! tabs with deep split trees). The blob captures tab labels, active
//! index, the next monotonic label counter, and each tab's pane tree
//! (axes + weights + leaf count). Per-pane cwd is **not** persisted — all
//! panes restore at the owning project's `root_path`.
//!
//! What is **not** persisted:
//! - Agent tabs (the `CliRuntime` session is per-process; can't be revived).
//!   Save path filters them out.
//! - PTY scrollback / running command output (the shell process is dead).
//! - Per-pane focus inside a tab (active leaf restores to the first leaf).

use serde::{Deserialize, Serialize};

use crate::shell::pane_tree::{Axis, PaneId, PaneTree};

const KEY_PREFIX: &str = "terminal_tabs:";

pub fn settings_key(project_id: &str) -> String {
    format!("{KEY_PREFIX}{project_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTabs {
    pub tabs: Vec<PersistedTab>,
    pub active: usize,
    pub next_label_n: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTab {
    pub label: String,
    pub tree: PersistedTree,
}

/// Mirrors `PaneTree` but without GPUI-side `PaneId`s (regenerated on
/// restore). `Split` keeps weights so drag-resize is preserved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistedTree {
    Leaf,
    Split {
        axis: PersistedAxis,
        children: Vec<PersistedTree>,
        weights: Vec<f32>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PersistedAxis {
    Horizontal,
    Vertical,
}

impl From<Axis> for PersistedAxis {
    fn from(a: Axis) -> Self {
        match a {
            Axis::Horizontal => Self::Horizontal,
            Axis::Vertical => Self::Vertical,
        }
    }
}

impl From<PersistedAxis> for Axis {
    fn from(a: PersistedAxis) -> Self {
        match a {
            PersistedAxis::Horizontal => Self::Horizontal,
            PersistedAxis::Vertical => Self::Vertical,
        }
    }
}

/// Convert a live `PaneTree` (with `PaneId`s) into a persistable shape.
pub fn snapshot_tree(t: &PaneTree) -> PersistedTree {
    match t {
        PaneTree::Leaf(_) => PersistedTree::Leaf,
        PaneTree::Split {
            axis,
            children,
            weights,
        } => PersistedTree::Split {
            axis: (*axis).into(),
            children: children.iter().map(snapshot_tree).collect(),
            weights: weights.clone(),
        },
    }
}

/// Walk a `PersistedTree` and produce a parallel `PaneTree` whose leaves
/// are assigned fresh `PaneId`s via `alloc`. Caller (typically `MainPane`)
/// supplies the allocator so id space stays monotonic.
pub fn restore_tree<F>(p: &PersistedTree, alloc: &mut F) -> PaneTree
where
    F: FnMut() -> PaneId,
{
    match p {
        PersistedTree::Leaf => PaneTree::Leaf(alloc()),
        PersistedTree::Split {
            axis,
            children,
            weights,
        } => PaneTree::Split {
            axis: (*axis).into(),
            children: children.iter().map(|c| restore_tree(c, alloc)).collect(),
            weights: weights.clone(),
        },
    }
}

/// Collect leaves in-order so the renderer + the per-leaf TerminalView
/// entities can be created in the same order they appear in the tree.
pub fn count_leaves(t: &PersistedTree) -> usize {
    match t {
        PersistedTree::Leaf => 1,
        PersistedTree::Split { children, .. } => children.iter().map(count_leaves).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree() -> PaneTree {
        PaneTree::Split {
            axis: Axis::Horizontal,
            children: vec![
                PaneTree::Leaf(PaneId(1)),
                PaneTree::Split {
                    axis: Axis::Vertical,
                    children: vec![PaneTree::Leaf(PaneId(2)), PaneTree::Leaf(PaneId(3))],
                    weights: vec![1.0, 2.0],
                },
            ],
            weights: vec![1.0, 1.5],
        }
    }

    #[test]
    fn snapshot_then_restore_preserves_topology_and_weights() {
        let original = make_tree();
        let snap = snapshot_tree(&original);
        let mut next = 100u64;
        let restored = restore_tree(&snap, &mut || {
            let id = PaneId(next);
            next += 1;
            id
        });
        // PaneIds differ but the structural shape + weights match.
        match (&original, &restored) {
            (
                PaneTree::Split { weights: w1, .. },
                PaneTree::Split { weights: w2, .. },
            ) => assert_eq!(w1, w2),
            _ => panic!("structure mismatch"),
        }
        assert_eq!(count_leaves(&snap), 3);
    }

    #[test]
    fn settings_key_format() {
        assert_eq!(settings_key("proj_abc"), "terminal_tabs:proj_abc");
    }

    #[test]
    fn round_trip_json_serde() {
        let snap = snapshot_tree(&make_tree());
        let blob = PersistedTabs {
            tabs: vec![PersistedTab {
                label: "Terminal 1".into(),
                tree: snap,
            }],
            active: 0,
            next_label_n: 2,
        };
        let s = serde_json::to_string(&blob).unwrap();
        let back: PersistedTabs = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.next_label_n, 2);
    }
}
