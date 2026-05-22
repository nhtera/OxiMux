//! `FileTree` — headless GPUI entity backing the file-tree pane.
//!
//! Owns the node store + open-dir map + filesystem watcher; emits
//! `FileTreeEvent` for the (future) view layer (step 4). Walker runs on
//! the background executor; watcher events arrive via tokio mpsc drained
//! from a `cx.spawn` task that survives for the entity's lifetime.

pub mod walker;
pub mod watcher;

use gpui::{Context, EventEmitter, Task};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Single source of truth for directories that are always skipped — used
/// by both the walker (`ALWAYS_SKIP`) and the watcher (`is_ignored`). Add
/// once here, both filters pick it up.
pub(crate) const SKIP_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".cache",
    "vendor",
    "coverage",
];

/// Monotonic identity for a tree node. Allocated from a global counter
/// shared across `FileTree` instances — collisions are impossible inside
/// a process, but the ID is not stable across restarts (don't persist).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeNodeId(pub u64);

static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1);

impl TreeNodeId {
    fn alloc() -> Self {
        Self(NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone)]
pub struct FileTreeNode {
    pub id: TreeNodeId,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Set once `expand` has populated children — distinguishes "directory
    /// the user never expanded" from "expanded empty directory".
    pub loaded: bool,
    /// IDs of the direct children currently stored in the parent
    /// `FileTree::nodes` map. Tracked so re-expanding a directory can
    /// evict the stale child entries instead of orphaning them.
    pub children: Vec<TreeNodeId>,
}

#[derive(Debug, Clone)]
pub enum FileTreeEvent {
    /// Walker finished — `children` are the direct entries of `parent_id`.
    Loaded {
        parent_id: TreeNodeId,
        children: Vec<FileTreeNode>,
    },
    /// Watcher fired for an open subtree; consumer should re-request
    /// children of `id`. The diff lives with the view, not here.
    Refresh(TreeNodeId),
    /// Watcher error surfaced to caller — usually fatal (root removed).
    WatchError(String),
}

pub struct FileTree {
    root: PathBuf,
    root_id: TreeNodeId,
    nodes: HashMap<TreeNodeId, FileTreeNode>,
    /// Inverted index: path → id, populated lazily as dirs are expanded.
    /// Used by the watcher loop to map an event back to a node in O(1).
    open_dirs: HashMap<PathBuf, TreeNodeId>,
    /// Keep the FSEvents stream alive for the entity's lifetime. Dropped
    /// when the entity is dropped — `notify-debouncer-full` cancels the
    /// underlying watch from its own `Drop` impl.
    _debouncer: Option<
        notify_debouncer_full::Debouncer<
            notify_debouncer_full::notify::RecommendedWatcher,
            notify_debouncer_full::FileIdMap,
        >,
    >,
}

impl EventEmitter<FileTreeEvent> for FileTree {}

impl FileTree {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let root_id = TreeNodeId::alloc();
        let mut nodes = HashMap::new();
        nodes.insert(
            root_id,
            FileTreeNode {
                id: root_id,
                path: root.clone(),
                is_dir: true,
                loaded: false,
                children: Vec::new(),
            },
        );
        let mut open_dirs = HashMap::new();
        open_dirs.insert(root.clone(), root_id);

        // Spawn watcher loop. The debouncer is constructed on the spawn
        // task (it does a blocking watch registration internally) and
        // handed back to `self` via `weak.update`. While that handoff is
        // in flight, watcher events queue in the mpsc buffer — none lost.
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<notify_debouncer_full::DebounceEventResult>();
        let watcher_root = root.clone();
        cx.spawn(async move |weak, cx| {
            let debouncer = match watcher::spawn_watcher(&watcher_root, tx) {
                Ok(d) => d,
                Err(e) => {
                    weak.update(cx, |_this, cx| {
                        cx.emit(FileTreeEvent::WatchError(e.to_string()));
                    })
                    .ok();
                    return;
                }
            };
            weak.update(cx, |this, _cx| {
                this._debouncer = Some(debouncer);
            })
            .ok();

            while let Some(result) = rx.recv().await {
                let weak2 = weak.clone();
                weak2
                    .update(cx, |this, cx| this.handle_debounce_result(result, cx))
                    .ok();
            }
        })
        .detach();

        Self {
            root,
            root_id,
            nodes,
            open_dirs,
            _debouncer: None,
        }
    }

    pub fn root_id(&self) -> TreeNodeId {
        self.root_id
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    pub fn node(&self, id: TreeNodeId) -> Option<&FileTreeNode> {
        self.nodes.get(&id)
    }

    /// Walk the direct children of `id` on the background executor and
    /// emit `FileTreeEvent::Loaded` when done. Idempotent: re-expanding
    /// an already-loaded dir simply re-runs the walk and re-emits.
    pub fn expand(&mut self, id: TreeNodeId, cx: &mut Context<Self>) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        if !node.is_dir {
            return;
        }
        let path = node.path.clone();
        let walk: Task<Vec<(PathBuf, bool)>> = cx.background_executor().spawn(async move {
            let mut entries = walker::list_children(&path);
            walker::sort_entries(&mut entries);
            entries
        });

        cx.spawn(async move |weak, cx| {
            let entries = walk.await;
            weak.update(cx, |this, cx| {
                this.store_children(id, entries, cx);
            })
            .ok();
        })
        .detach();
    }

    fn store_children(
        &mut self,
        parent_id: TreeNodeId,
        entries: Vec<(PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) {
        // Evict any stale subtree from a previous expand. Without this,
        // re-expanding a directory leaks the old `TreeNodeId`s into
        // `self.nodes` AND leaves grandchildren in `open_dirs`, which
        // would cause the watcher to emit `Refresh` for unreachable nodes.
        let stale_children: Vec<TreeNodeId> = self
            .nodes
            .get_mut(&parent_id)
            .map(|p| std::mem::take(&mut p.children))
            .unwrap_or_default();
        for id in stale_children {
            remove_subtree(&mut self.nodes, &mut self.open_dirs, id);
        }

        let children: Vec<FileTreeNode> = entries
            .into_iter()
            .map(|(path, is_dir)| {
                let id = TreeNodeId::alloc();
                let node = FileTreeNode {
                    id,
                    path: path.clone(),
                    is_dir,
                    loaded: false,
                    children: Vec::new(),
                };
                if is_dir {
                    self.open_dirs.insert(path, id);
                }
                self.nodes.insert(id, node.clone());
                node
            })
            .collect();
        if let Some(parent) = self.nodes.get_mut(&parent_id) {
            parent.loaded = true;
            parent.children = children.iter().map(|n| n.id).collect();
        }
        cx.emit(FileTreeEvent::Loaded {
            parent_id,
            children,
        });
    }

    fn handle_debounce_result(
        &mut self,
        result: notify_debouncer_full::DebounceEventResult,
        cx: &mut Context<Self>,
    ) {
        let events = match result {
            Ok(events) => events,
            Err(errs) => {
                for e in errs {
                    cx.emit(FileTreeEvent::WatchError(e.to_string()));
                }
                return;
            }
        };
        let mut seen: HashMap<TreeNodeId, ()> = HashMap::new();
        for event in events {
            for p in &event.event.paths {
                if watcher::is_ignored(p) {
                    continue;
                }
                if let Some(id) = watcher::find_node_to_invalidate(p, &self.open_dirs)
                    && seen.insert(id, ()).is_none()
                    && self
                        .nodes
                        .get(&id)
                        .map(|n| n.path.exists())
                        .unwrap_or(false)
                {
                    // Guard: don't refresh if the dir itself was deleted;
                    // the event burst will land on a still-existing ancestor.
                    cx.emit(FileTreeEvent::Refresh(id));
                }
            }
        }
    }
}

/// Recursively evict `id` and the entire subtree below it from `nodes`
/// and `open_dirs`. Pure free function — `store_children` calls this so
/// re-expanding an ancestor doesn't leave grandchildren behind that the
/// watcher would still emit phantom `Refresh` events for.
fn remove_subtree(
    nodes: &mut HashMap<TreeNodeId, FileTreeNode>,
    open_dirs: &mut HashMap<PathBuf, TreeNodeId>,
    id: TreeNodeId,
) {
    let Some(node) = nodes.remove(&id) else {
        return;
    };
    if node.is_dir {
        open_dirs.remove(&node.path);
    }
    for child_id in node.children {
        remove_subtree(nodes, open_dirs, child_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(id: u64, path: &str, is_dir: bool, children: Vec<u64>) -> FileTreeNode {
        FileTreeNode {
            id: TreeNodeId(id),
            path: PathBuf::from(path),
            is_dir,
            loaded: !children.is_empty(),
            children: children.into_iter().map(TreeNodeId).collect(),
        }
    }

    #[test]
    fn remove_subtree_evicts_grandchildren_from_open_dirs() {
        // A(1) → B(2) → C(3). Re-expanding A must evict B + C from both
        // maps so a watcher event for /a/b/c/file does not Refresh C.
        let mut nodes = HashMap::new();
        nodes.insert(TreeNodeId(1), make_node(1, "/a", true, vec![2]));
        nodes.insert(TreeNodeId(2), make_node(2, "/a/b", true, vec![3]));
        nodes.insert(TreeNodeId(3), make_node(3, "/a/b/c", true, vec![]));
        let mut open_dirs = HashMap::new();
        open_dirs.insert(PathBuf::from("/a"), TreeNodeId(1));
        open_dirs.insert(PathBuf::from("/a/b"), TreeNodeId(2));
        open_dirs.insert(PathBuf::from("/a/b/c"), TreeNodeId(3));

        remove_subtree(&mut nodes, &mut open_dirs, TreeNodeId(2));

        assert!(
            !nodes.contains_key(&TreeNodeId(2)),
            "B not evicted from nodes"
        );
        assert!(
            !nodes.contains_key(&TreeNodeId(3)),
            "C not evicted from nodes"
        );
        assert!(
            !open_dirs.contains_key(&PathBuf::from("/a/b")),
            "B not evicted from open_dirs"
        );
        assert!(
            !open_dirs.contains_key(&PathBuf::from("/a/b/c")),
            "C not evicted from open_dirs"
        );
        assert!(nodes.contains_key(&TreeNodeId(1)), "A wrongly evicted");
    }

    #[test]
    fn remove_subtree_keeps_leaf_files_out_of_open_dirs() {
        let mut nodes = HashMap::new();
        nodes.insert(TreeNodeId(1), make_node(1, "/a/file.rs", false, vec![]));
        let mut open_dirs = HashMap::new();
        // file leaf was never in open_dirs to begin with — verify
        // remove_subtree doesn't panic on the missing-key path.
        remove_subtree(&mut nodes, &mut open_dirs, TreeNodeId(1));
        assert!(nodes.is_empty());
        assert!(open_dirs.is_empty());
    }

    #[test]
    fn remove_subtree_missing_id_no_op() {
        let mut nodes: HashMap<TreeNodeId, FileTreeNode> = HashMap::new();
        let mut open_dirs: HashMap<PathBuf, TreeNodeId> = HashMap::new();
        remove_subtree(&mut nodes, &mut open_dirs, TreeNodeId(99));
        assert!(nodes.is_empty());
        assert!(open_dirs.is_empty());
    }
}
