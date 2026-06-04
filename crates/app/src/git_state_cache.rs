//! Process-wide cache of the last-known `GitState` per working-tree root.
//!
//! Switching the active project rebuilds the right sidebar, which spawns a
//! fresh `StatusPoller` whose watch channel starts at `Loading`. Without a
//! cache the changed-files panel flashes a "Loading…" placeholder for the
//! duration of the first poll every time the user returns to a project they
//! already visited this session. This cache lets the rebuilt panel paint the
//! previous snapshot instantly while the poller revalidates underneath —
//! stale-while-revalidate.
//!
//! Stored as a GPUI global keyed by type, so its lifetime is the process.
//! Entries are keyed by the canonical workdir, so a seed only ever shows
//! that exact repository's own prior state — never another project's. A
//! cache miss (first-ever open of a repo this session) falls back to the
//! normal `Loading` placeholder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use gpui::Global;
use oximux_core::GitState;

/// Shared, cheaply-clonable handle to the last-known-state map. The inner
/// `Arc<RwLock<…>>` means every clone (and the GPUI global) points at the
/// same map.
#[derive(Clone, Default)]
pub struct GitStateCache(Arc<RwLock<HashMap<PathBuf, GitState>>>);

impl Global for GitStateCache {}

impl GitStateCache {
    /// Last-known state for `workdir`, if any successful poll has been
    /// recorded for it this session. Returns an owned clone so the caller
    /// doesn't hold the read lock.
    pub fn get(&self, workdir: &Path) -> Option<GitState> {
        self.0.read().ok()?.get(workdir).cloned()
    }

    /// Record the latest successful poll for `workdir`. A poisoned lock is
    /// swallowed: a stale-cache write is never worth propagating a panic
    /// through the status-poll consumer.
    pub fn put(&self, workdir: &Path, state: GitState) {
        if let Ok(mut map) = self.0.write() {
            map.insert(workdir.to_path_buf(), state);
        }
    }
}
