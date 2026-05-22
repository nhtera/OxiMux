//! Filesystem watcher with 200ms debounce.
//!
//! macOS `RecommendedWatcher` = FSEvents — file-level paths, no re-stat
//! needed to know what changed. Single recursive watch from the project
//! root; the OS has no path-exclusion API so we filter `node_modules/`
//! style paths in `is_ignored()` before emitting `FileTreeEvent::Refresh`.
//!
//! `DebounceEventHandler` has a blanket impl for `FnMut(...) + Send +
//! 'static`, so we forward into a tokio mpsc via closure — no special
//! crate feature needed.

use crate::file_tree::TreeNodeId;
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdMap, new_debouncer,
    notify::{self, RecommendedWatcher, RecursiveMode},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;

const DEBOUNCE_MS: u64 = 200;

/// Spawns the FSEvents watcher; debounced events arrive on `tx`. Returned
/// `Debouncer` must be kept alive — dropping it stops the FSEvents stream.
pub fn spawn_watcher(
    root: &Path,
    tx: mpsc::UnboundedSender<DebounceEventResult>,
) -> Result<Debouncer<RecommendedWatcher, FileIdMap>, notify::Error> {
    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    )?;
    debouncer.watch(root, RecursiveMode::Recursive)?;
    Ok(debouncer)
}

/// True if any path component is in the always-skip list. Userspace filter
/// run on every debounced event — ns-scale string compare per component.
/// Reads the same `SKIP_NAMES` list as `walker::ALWAYS_SKIP` so the two
/// filters cannot diverge.
pub fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|s| crate::file_tree::SKIP_NAMES.contains(&s))
            .unwrap_or(false)
    })
}

/// Map an event path back to the deepest open ancestor's `TreeNodeId`.
/// O(1) for exact hits; O(depth) for nested paths under an open dir.
/// `None` = path falls outside any open subtree → no UI refresh needed.
pub fn find_node_to_invalidate(
    event_path: &Path,
    open_dirs: &HashMap<PathBuf, TreeNodeId>,
) -> Option<TreeNodeId> {
    if let Some(&id) = open_dirs.get(event_path) {
        return Some(id);
    }
    let mut cur = event_path.parent();
    while let Some(p) = cur {
        if let Some(&id) = open_dirs.get(p) {
            return Some(id);
        }
        cur = p.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn is_ignored_git_path() {
        assert!(is_ignored(Path::new("/r/.git/HEAD")));
        assert!(is_ignored(Path::new("/r/.git")));
    }

    #[test]
    fn is_ignored_nested_node_modules() {
        assert!(is_ignored(Path::new("/r/a/node_modules/b/index.js")));
        assert!(is_ignored(Path::new("/r/pkg/target/debug/foo")));
    }

    #[test]
    fn is_ignored_normal_path_false() {
        assert!(!is_ignored(Path::new("/r/src/main.rs")));
        assert!(!is_ignored(Path::new("/r/crates/editor/Cargo.toml")));
    }

    #[test]
    fn find_node_to_invalidate_exact() {
        let mut open = HashMap::new();
        open.insert(PathBuf::from("/r/src"), TreeNodeId(42));
        let got = find_node_to_invalidate(Path::new("/r/src"), &open);
        assert_eq!(got, Some(TreeNodeId(42)));
    }

    #[test]
    fn find_node_to_invalidate_parent_walk() {
        let mut open = HashMap::new();
        open.insert(PathBuf::from("/r/src"), TreeNodeId(7));
        let got = find_node_to_invalidate(Path::new("/r/src/sub/deep/file.rs"), &open);
        assert_eq!(got, Some(TreeNodeId(7)));
    }

    #[test]
    fn find_node_to_invalidate_none() {
        let mut open = HashMap::new();
        open.insert(PathBuf::from("/r/a"), TreeNodeId(1));
        let got = find_node_to_invalidate(Path::new("/r/b/x"), &open);
        assert_eq!(got, None);
    }
}
