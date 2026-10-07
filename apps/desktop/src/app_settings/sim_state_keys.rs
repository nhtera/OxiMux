//! The Mobile Emulator panel's persisted state, in the global `SettingsRepo`
//! key/value store.
//!
//! Only intent survives a restart — which worktree uses which device, and
//! which devices we booted (so the 10-minute idle shutdown and the quit-time
//! shutdown still apply after a crash) — never a process. `sim_feature_used`
//! gates the background device watcher: someone who never opened the panel
//! pays for no polling at all.

use std::collections::HashSet;

use oximux_simulator::DeviceId;
use oximux_simulator::registry::Snapshot;
use oximux_storage::{SettingsRepo, SimApprovalRepo};

/// JSON [`Snapshot`]: attachments + owned boots.
pub const KEY_REGISTRY: &str = "sim_registry_v1";

/// `"1"` once the simulator panel was opened or a device attached.
pub const KEY_FEATURE_USED: &str = "sim_feature_used";

/// JSON list of devices the user shut down from the panel (the power
/// button's latch: agents may not boot them again).
pub const KEY_STOPPED: &str = "sim_stopped_v1";

/// `"1"` once approvals saved for real devices were revoked (agent access
/// to a real device now lasts until OxiMux quits; it used to be saved).
pub const KEY_PHYSICAL_CONSENT_MIGRATED: &str = "sim_physical_consent_migrated_v1";

/// Revoke, once, every saved approval for a real device. Nothing reads them
/// any more (see `AgentState::load`); this removes them from the database
/// and from Settings' list. A failure is retried at the next launch.
pub fn revoke_physical_approvals_once(repo: &SettingsRepo, approvals: &SimApprovalRepo) {
    if matches!(repo.get(KEY_PHYSICAL_CONSENT_MIGRATED), Ok(Some(v)) if v == "1") {
        return;
    }
    let Ok(saved) = approvals.list().inspect_err(|err| tracing::warn!(?err, "simulator approvals unreadable")) else { return };
    for approval in saved.iter().filter(|a| DeviceId(a.udid.clone()).is_physical()) {
        if let Err(err) = approvals.revoke(&approval.udid) {
            tracing::warn!(?err, "a real device's saved approval was not revoked");
            return;
        }
        tracing::info!(udid = %approval.udid, "revoked a saved approval: agent access to a real device lasts until OxiMux quits");
    }
    if let Err(err) = repo.set(KEY_PHYSICAL_CONSENT_MIGRATED, "1") {
        tracing::warn!(?err, "physical consent migration not recorded");
    }
}

/// JSON map of the phones OxiMux paired over Wi-Fi: mDNS instance
/// (`adb-<id>`) → the phone's serial. In the database, not `simulator.toml`:
/// it decides which network transport may join a phone's row (and its
/// agent approval), and agents' tools write that file as a matter of course.
pub const KEY_WIFI_PAIRED: &str = "sim_wifi_paired_v1";

/// The phones paired over Wi-Fi; none when absent or unreadable (a lost map
/// only shows a paired phone as "Not paired by OxiMux" until it pairs again).
pub fn load_wifi_paired(repo: &SettingsRepo) -> std::collections::HashMap<String, String> {
    match repo.get(KEY_WIFI_PAIRED) {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => Default::default(),
    }
}

pub fn save_wifi_paired(repo: &SettingsRepo, paired: &std::collections::HashMap<String, String>) {
    let sorted: std::collections::BTreeMap<&String, &String> = paired.iter().collect();
    let Ok(json) = serde_json::to_string(&sorted) else { return };
    if let Err(err) = repo.set(KEY_WIFI_PAIRED, &json) {
        tracing::warn!(?err, "paired Wi-Fi phones not saved");
    }
}

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

/// The latched devices; none when absent or unreadable (the latch is a
/// courtesy to the user, not a security boundary: losing it costs one boot).
pub fn load_stopped(repo: &SettingsRepo) -> HashSet<DeviceId> {
    match repo.get(KEY_STOPPED) {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => HashSet::new(),
    }
}

pub fn save_stopped(repo: &SettingsRepo, stopped: &HashSet<DeviceId>) {
    let mut ids: Vec<&DeviceId> = stopped.iter().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let Ok(json) = serde_json::to_string(&ids) else { return };
    if let Err(err) = repo.set(KEY_STOPPED, &json) {
        tracing::warn!(?err, "simulator stop latch not saved");
    }
}

/// The signing team the user chose for the iPhone control runner (a team
/// id). In the database, not `simulator.toml`: it decides which Apple
/// account OxiMux builds and registers with, and agents write that file.
pub const KEY_IPHONE_TEAM: &str = "sim_iphone_team_v1";

/// JSON list of the iPhones whose control the user turned on.
pub const KEY_IPHONE_CONTROLLED: &str = "sim_iphone_controlled_v1";

pub fn load_iphone_team(repo: &SettingsRepo) -> Option<String> {
    repo.get(KEY_IPHONE_TEAM).ok().flatten().filter(|t| !t.is_empty())
}

pub fn save_iphone_team(repo: &SettingsRepo, team: &str) {
    if let Err(err) = repo.set(KEY_IPHONE_TEAM, team) {
        tracing::warn!(?err, "iPhone signing team not saved");
    }
}

pub fn load_iphone_controlled(repo: &SettingsRepo) -> HashSet<DeviceId> {
    match repo.get(KEY_IPHONE_CONTROLLED) {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => HashSet::new(),
    }
}

pub fn save_iphone_controlled(repo: &SettingsRepo, controlled: &HashSet<DeviceId>) {
    let mut ids: Vec<&DeviceId> = controlled.iter().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let Ok(json) = serde_json::to_string(&ids) else { return };
    if let Err(err) = repo.set(KEY_IPHONE_CONTROLLED, &json) {
        tracing::warn!(?err, "controlled iPhones not saved");
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
    fn the_stop_latch_round_trips_and_a_corrupt_one_reads_as_empty() {
        let repo = SettingsRepo::new(open_memory().unwrap());
        assert!(load_stopped(&repo).is_empty());
        let stopped: HashSet<DeviceId> = [DeviceId("B".into()), DeviceId("avd:Pixel".into())].into();
        save_stopped(&repo, &stopped);
        assert_eq!(load_stopped(&repo), stopped);
        repo.set(KEY_STOPPED, "[oops").unwrap();
        assert!(load_stopped(&repo).is_empty());
    }

    /// Saved approvals for real devices are revoked once; simulators' and
    /// emulators' stay, and a phone approved again later is not touched by a
    /// second run (it is not saved anyway).
    #[test]
    fn physical_approvals_are_revoked_once() {
        let db = open_memory().unwrap();
        let (repo, approvals) = (SettingsRepo::new(db.clone()), SimApprovalRepo::new(db));
        for udid in ["U-1", "avd:Pixel", "adb:R58M123", "iosdev:00008110-001A2C3E0A88401E"] {
            approvals.grant(udid, "device").unwrap();
        }
        revoke_physical_approvals_once(&repo, &approvals);
        let left: Vec<String> = approvals.list().unwrap().into_iter().map(|a| a.udid).collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.iter().all(|u| !DeviceId(u.clone()).is_physical()));
        approvals.grant("adb:R58M123", "phone").unwrap();
        revoke_physical_approvals_once(&repo, &approvals);
        assert_eq!(approvals.list().unwrap().len(), 3, "the migration ran once");
    }

    #[test]
    fn the_wifi_pairing_map_round_trips_and_a_corrupt_one_reads_as_empty() {
        let repo = SettingsRepo::new(open_memory().unwrap());
        assert!(load_wifi_paired(&repo).is_empty());
        let paired = std::collections::HashMap::from([("adb-R58-x1".to_owned(), "R58".to_owned())]);
        save_wifi_paired(&repo, &paired);
        assert_eq!(load_wifi_paired(&repo), paired);
        repo.set(KEY_WIFI_PAIRED, "{oops").unwrap();
        assert!(load_wifi_paired(&repo).is_empty());
    }

    #[test]
    fn a_corrupt_snapshot_reads_as_empty() {
        let repo = SettingsRepo::new(open_memory().unwrap());
        repo.set(KEY_REGISTRY, "{not json").unwrap();
        assert_eq!(load_snapshot(&repo), Snapshot::default());
    }

    #[test]
    fn the_iphone_team_and_controlled_phones_round_trip() {
        let repo = SettingsRepo::new(open_memory().unwrap());
        assert_eq!(load_iphone_team(&repo), None);
        assert!(load_iphone_controlled(&repo).is_empty());
        save_iphone_team(&repo, "TEAM000001");
        let phones: HashSet<DeviceId> = [DeviceId("iosdev:00008130-0001".into()), DeviceId("iosdev:00008130-0002".into())].into();
        save_iphone_controlled(&repo, &phones);
        assert_eq!(load_iphone_team(&repo).as_deref(), Some("TEAM000001"));
        assert_eq!(load_iphone_controlled(&repo), phones);
        // Unreadable: none controlled (never a panic at startup).
        repo.set(KEY_IPHONE_CONTROLLED, "{not json").unwrap();
        assert!(load_iphone_controlled(&repo).is_empty());
    }
}
