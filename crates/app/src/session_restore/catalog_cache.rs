//! Process-wide cache of the last-known model catalog per dynamic-model agent.
//!
//! Codex and ACP agents (OpenCode, Cursor, Amp) don't declare a static model
//! list — their real catalog is only known after the backend spawns and
//! completes its handshake. The *New Agent* draft fetches it with a throwaway
//! "catalog probe" ([`oximux_agents::thread::probe_catalog`]) that cold-starts
//! the backend just to read the models, then throws the process away. That cold
//! start is cheap for Codex (a Rust binary, ~0.1s) but slow for a Node ACP agent
//! like OpenCode (~5s: interpreter start + plugin load + a 50-model session/new).
//!
//! Without a cache that cost is paid *every* time a draft picks that agent —
//! each new draft view starts with an empty per-view probe map. This cache
//! hoists the result to a process-wide [`Global`] keyed by adapter id, so:
//!   - the 2nd+ draft in a session paints the picker instantly (no re-spawn), and
//!   - a disk-persisted blob lets a *cold* launch seed the picker from the last
//!     session's catalog instantly (stale-while-revalidate) — the difference
//!     between a ~5s "no models yet" gap and the list painting on open.
//!
//! `fresh` tracks which adapters were re-probed live *this* session (in-memory,
//! never persisted), so a disk seed still triggers exactly one background
//! revalidation the first time it's used, then is trusted for the rest of the
//! session. A cache miss (first-ever probe of an agent, empty disk blob) falls
//! back to the normal `Loading` state until the probe lands.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use gpui::Global;
use oximux_agents::thread::ProbedCatalog;
use oximux_storage::SettingsRepo;

/// Settings key under which the persisted `entries` map is stored as JSON. The
/// `_v1` suffix lets a future `ProbedCatalog`/`ModelChoice` shape change orphan
/// the old blob (deserialize fails → empty cache → one cold probe, then
/// self-heals) instead of mis-seeding a stale shape.
const PERSIST_KEY: &str = "agent_catalog_cache_v1";

#[derive(Default)]
struct Inner {
    /// Last-known catalog per adapter id — the persisted, cross-session map.
    entries: HashMap<String, ProbedCatalog>,
    /// Adapter ids re-probed live this session (in-memory only). An id here is
    /// trusted without a re-probe; an id only in `entries` (a disk seed) is
    /// shown instantly but revalidated once.
    fresh: HashSet<String>,
}

/// Shared, cheaply-clonable handle to the catalog cache. The inner
/// `Arc<RwLock<…>>` means every clone (and the GPUI global) points at the same
/// map, so a probe fold-back on one draft warms every later draft.
#[derive(Clone, Default)]
pub struct CatalogCache(Arc<RwLock<Inner>>);

impl Global for CatalogCache {}

impl CatalogCache {
    /// Last-known catalog for `adapter_id`, if any (disk seed or this-session
    /// probe). Returns an owned clone so the caller doesn't hold the read lock.
    pub fn get(&self, adapter_id: &str) -> Option<ProbedCatalog> {
        self.0.read().ok()?.entries.get(adapter_id).cloned()
    }

    /// Whether `adapter_id` was re-probed live this session — i.e. its cached
    /// catalog is trusted and needs no revalidation. A disk seed is `false`
    /// (shown instantly but revalidated once), a fresh probe is `true`.
    pub fn is_fresh(&self, adapter_id: &str) -> bool {
        self.0.read().map(|i| i.fresh.contains(adapter_id)).unwrap_or(false)
    }

    /// Record a successful live probe: store the catalog and mark it fresh for
    /// the rest of the session. A poisoned lock is swallowed — a warm-cache
    /// write is never worth propagating a panic through the probe fold-back.
    pub fn record(&self, adapter_id: &str, catalog: ProbedCatalog) {
        if let Ok(mut inner) = self.0.write() {
            inner.entries.insert(adapter_id.to_string(), catalog);
            inner.fresh.insert(adapter_id.to_string());
        }
    }

    /// Rehydrate `entries` from the persisted JSON blob so a cold launch can
    /// seed the first draft's picker from the last session's catalog. `fresh`
    /// starts empty, so each seeded adapter is revalidated once on first use.
    /// A missing/corrupt/legacy blob yields an empty cache (normal `Loading`
    /// fallback); persistence is best-effort and never blocks boot.
    pub fn load_from(settings: &SettingsRepo) -> Self {
        let entries = settings
            .get(PERSIST_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<HashMap<String, ProbedCatalog>>(&json).ok())
            .unwrap_or_default();
        CatalogCache(Arc::new(RwLock::new(Inner { entries, fresh: HashSet::new() })))
    }

    /// Persist `entries` as JSON on the quit/save path so the next launch can
    /// seed from it. `fresh` is deliberately not persisted (it's a per-session
    /// revalidation marker). Best-effort — a serialize/write error only costs
    /// one cold probe next time.
    pub fn save_to(&self, settings: &SettingsRepo) {
        if let Ok(inner) = self.0.read()
            && let Ok(json) = serde_json::to_string(&inner.entries)
        {
            let _ = settings.set(PERSIST_KEY, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::ModelChoice;
    use oximux_storage::{SettingsRepo, open_memory};

    fn sample(wire: &str) -> ProbedCatalog {
        ProbedCatalog {
            models: vec![ModelChoice {
                wire: wire.to_string(),
                label: wire.to_string(),
                description: Some("a model".to_string()),
            }],
            default_model: Some(wire.to_string()),
        }
    }

    #[test]
    fn save_then_load_round_trips_entries() {
        let settings = SettingsRepo::new(open_memory().expect("memory db"));
        let cache = CatalogCache::default();
        cache.record("opencode", sample("gpt-5.6-sol"));
        cache.save_to(&settings);

        // A fresh cache rehydrated from the same settings sees the entry — the
        // cold-launch seed path.
        let reloaded = CatalogCache::load_from(&settings);
        assert_eq!(reloaded.get("opencode"), Some(sample("gpt-5.6-sol")));
        // …but a disk seed is NOT "fresh" — it must revalidate once this session.
        assert!(!reloaded.is_fresh("opencode"), "disk seed revalidates once");
    }

    #[test]
    fn record_marks_fresh_for_the_session() {
        let cache = CatalogCache::default();
        assert!(!cache.is_fresh("codex"), "unprobed → not fresh");
        assert_eq!(cache.get("codex"), None);
        cache.record("codex", sample("gpt-5.4"));
        assert!(cache.is_fresh("codex"), "a live probe is trusted this session");
        assert_eq!(cache.get("codex"), Some(sample("gpt-5.4")));
    }

    #[test]
    fn load_from_empty_settings_yields_empty_cache() {
        let settings = SettingsRepo::new(open_memory().expect("memory db"));
        let cache = CatalogCache::load_from(&settings);
        assert_eq!(cache.get("anything"), None);
        assert!(!cache.is_fresh("anything"));
    }

    #[test]
    fn corrupt_blob_degrades_to_empty_not_panic() {
        let settings = SettingsRepo::new(open_memory().expect("memory db"));
        settings.set(PERSIST_KEY, "{not valid json").expect("set");
        let cache = CatalogCache::load_from(&settings);
        assert_eq!(cache.get("x"), None);
    }
}
