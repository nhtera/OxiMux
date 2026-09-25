//! The iOS Simulator panel's persisted state, in the global `SettingsRepo`
//! key/value store.
//!
//! Only intent survives a restart — which worktree uses which device, and
//! which devices we booted (so the 10-minute idle shutdown and the quit-time
//! shutdown still apply after a crash) — never a process. `sim_feature_used`
//! gates the background device watcher: someone who never opened the panel
//! pays for no polling at all.

use oximux_simulator::registry::Snapshot;
use oximux_storage::SettingsRepo;

/// JSON [`Snapshot`]: attachments + owned boots.
pub const KEY_REGISTRY: &str = "sim_registry_v1";

/// `"1"` once the simulator panel was opened or a device attached.
pub const KEY_FEATURE_USED: &str = "sim_feature_used";

/// The saved snapshot; empty when absent or unreadable (a lost attachment
/// only costs one click, a panicking startup costs far more).
pub fn load_snapshot(repo: &SettingsRepo) -> Snapshot {
    match repo.get(KEY_REGISTRY) {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!(?err, "simulator registry snapshot unreadable; starting empty");
            Snapshot::default()
        }),
        Ok(None) => Snapshot::default(),
        Err(err) => {
            tracing::warn!(?err, "simulator registry snapshot not loaded");
            Snapshot::default()
        }
    }
}

pub fn save_snapshot(repo: &SettingsRepo, snapshot: &Snapshot) {
    let Ok(json) = serde_json::to_string(snapshot) else { return };
    if let Err(err) = repo.set(KEY_REGISTRY, &json) {
        tracing::warn!(?err, "simulator registry snapshot not saved");
    }
}

pub fn feature_used(repo: &SettingsRepo) -> bool {
    matches!(repo.get(KEY_FEATURE_USED), Ok(Some(v)) if v == "1")
}

pub fn mark_feature_used(repo: &SettingsRepo) {
    if let Err(err) = repo.set(KEY_FEATURE_USED, "1") {
        tracing::warn!(?err, "sim_feature_used not saved");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_simulator::DeviceId;
    use oximux_storage::open_memory;

    #[test]
    fn snapshot_and_feature_flag_round_trip() {
        let db = open_memory().unwrap();
        let repo = SettingsRepo::new(db);
        assert_eq!(load_snapshot(&repo), Snapshot::default());
        assert!(!feature_used(&repo));
        let snap = Snapshot { attachments: vec![], owned_boots: vec![DeviceId("U".into())] };
        save_snapshot(&repo, &snap);
        mark_feature_used(&repo);
        assert_eq!(load_snapshot(&repo), snap);
        assert!(feature_used(&repo));
    }

    #[test]
    fn a_corrupt_snapshot_reads_as_empty() {
        let repo = SettingsRepo::new(open_memory().unwrap());
        repo.set(KEY_REGISTRY, "{not json").unwrap();
        assert_eq!(load_snapshot(&repo), Snapshot::default());
    }
}
