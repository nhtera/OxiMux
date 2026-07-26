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
use oximux_storage::SettingsRepo;

/// Settings key under which the whole cache map is persisted as JSON. The
/// `_v1` suffix lets a future `GitState` shape change orphan the old blob
/// (deserialize fails → empty map → one "Loading" flash, then self-heals)
/// instead of mis-seeding.
const PERSIST_KEY: &str = "git_state_cache_v1";

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

    /// Rehydrate the cache from its persisted JSON blob so a *cold* launch
    /// can seed the first poller from the last session's snapshot — the
    /// difference between "loading git…" and the prior state painting
    /// instantly on open. A missing/corrupt/legacy blob yields an empty
    /// cache (the normal `Loading` fallback); persistence is best-effort and
    /// never blocks boot on a read error.
    pub fn load_from(settings: &SettingsRepo) -> Self {
        let map = settings
            .get(PERSIST_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<HashMap<PathBuf, GitState>>(&json).ok())
            .unwrap_or_default();
        GitStateCache(Arc::new(RwLock::new(map)))
    }

    /// Persist the whole map as JSON. Called on the quit/last-window-close
    /// save path so the next launch can seed from it. Best-effort: a
    /// serialize failure or a settings write error is swallowed — a missing
    /// cache only costs one "Loading" flash next time.
    pub fn save_to(&self, settings: &SettingsRepo) {
        if let Ok(map) = self.0.read()
            && let Ok(json) = serde_json::to_string(&*map)
        {
            let _ = settings.set(PERSIST_KEY, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::{SettingsRepo, open_memory};

    fn sample_state(branch: &str) -> GitState {
        GitState {
            branch: Some(branch.to_string()),
            ..GitState::default()
        }
    }

    #[test]
    fn save_then_load_round_trips_the_map() {
        let settings = SettingsRepo::new(open_memory().expect("memory db"));
        let workdir = Path::new("/tmp/repo-a");

        let cache = GitStateCache::default();
        cache.put(workdir, sample_state("main"));
        cache.save_to(&settings);

        // A fresh cache rehydrated from the same settings sees the entry —
        // this is the cold-launch seed path.
        let reloaded = GitStateCache::load_from(&settings);
        assert_eq!(reloaded.get(workdir), Some(sample_state("main")));
    }

    #[test]
    fn load_from_empty_settings_yields_empty_cache() {
        let settings = SettingsRepo::new(open_memory().expect("memory db"));
        let cache = GitStateCache::load_from(&settings);
        assert_eq!(cache.get(Path::new("/tmp/anything")), None);
    }

    #[test]
    fn corrupt_blob_degrades_to_empty_not_panic() {
        let settings = SettingsRepo::new(open_memory().expect("memory db"));
        settings.set(PERSIST_KEY, "{not valid json").expect("set");
        // Must not panic; a corrupt/legacy blob just falls back to Loading.
        let cache = GitStateCache::load_from(&settings);
        assert_eq!(cache.get(Path::new("/tmp/x")), None);
    }
}
