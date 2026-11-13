//! The data directory's host core, as `oximux serve` builds it.
//!
//! The core is the same one the desktop runs — spawn lineage, the caps, the
//! health clock, the attention stream — and it reads the same rows in the same
//! database. That is the whole point: a data directory enforces one set of
//! caps whichever host holds it, so an agent cannot widen its own posture by
//! being started under `serve` instead of the app.
//!
//! This is its own function so the wiring can be tested without the socket
//! stack: that the config comes from the settings store rather than a default
//! struct is the kind of thing that regresses silently.

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{HostConfig, HostCore, HostCoreDeps};

/// Build the host core over `db`, with the caps `settings` holds.
pub(crate) fn build_host_core(
    rt: tokio::runtime::Handle,
    db: &oximux_storage::db::Db,
    registry: Arc<SessionRegistry>,
    settings: &oximux_storage::SettingsRepo,
) -> Arc<HostCore> {
    HostCore::new(HostCoreDeps {
        rt,
        lineage: oximux_agents::lineage::LineageStore::new(db.conn()),
        registry,
        // Not `HostConfig::default()`: the keys the desktop's Settings pane
        // writes are the keys this host obeys.
        config: HostConfig::load(|key| settings.get(key).ok().flatten()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cap stored in the database reaches the core. Written as the desktop
    /// writes it (`HostConfig::entries`), read as serve reads it, so the two
    /// cannot drift to different key names without this failing.
    #[test]
    fn the_caps_come_from_the_database_and_not_from_the_defaults() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let db = oximux_storage::db::open_memory().unwrap();
        let settings = oximux_storage::SettingsRepo::new(db.clone());
        let stored = HostConfig { stall_after_s: 42, ..HostConfig::default() };
        for (key, value) in stored.entries() {
            settings.set(key, &value).unwrap();
        }

        let core = build_host_core(
            rt.handle().clone(),
            &db,
            Arc::new(SessionRegistry::new()),
            &settings,
        );

        assert_eq!(core.config().stall_after_s, 42, "the stall clock came from the store");
        assert_eq!(
            core.config().caps.depth,
            stored.caps.depth,
            "and so did the depth cap the desktop wrote",
        );
    }

    /// An empty store is defaults, not zeroes — a serve with no settings row
    /// must enforce the shipped caps rather than "no spawning at all" or an
    /// unbounded tree.
    #[test]
    fn an_empty_store_leaves_the_shipped_defaults() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let db = oximux_storage::db::open_memory().unwrap();
        let settings = oximux_storage::SettingsRepo::new(db.clone());

        let core = build_host_core(
            rt.handle().clone(),
            &db,
            Arc::new(SessionRegistry::new()),
            &settings,
        );

        assert_eq!(core.config().caps.depth, HostConfig::default().caps.depth);
        assert_eq!(core.config().stall_after_s, HostConfig::default().stall_after_s);
    }

    /// Serve writes a record saying a *serve* holds the directory, and the
    /// next host reads it back as its prior owner. The desktop writes
    /// `"desktop"` through the same call, which is how `foreign_serve_holder`
    /// can tell the two apart.
    #[test]
    fn the_lock_record_names_serve_as_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("host.lock");
        let first = oximux_single_instance::try_acquire_as(&lock, "serve").unwrap();
        let oximux_single_instance::AcquireOutcome::Acquired(guard) = first else {
            panic!("the first acquire should win an unheld lock")
        };
        assert!(guard.previous().is_none(), "nothing held it before");
        drop(guard);

        let second = oximux_single_instance::try_acquire_as(&lock, "serve").unwrap();
        let oximux_single_instance::AcquireOutcome::Acquired(guard) = second else {
            panic!("a dropped lock is free")
        };
        let previous = guard.previous().expect("the first holder left a record");
        assert_eq!(previous.role.as_deref(), Some("serve"));
        assert!(
            !matches!(
                oximux_remote_host::PriorOwner::from_previous(Some(previous)),
                oximux_remote_host::PriorOwner::None
            ),
            "a record that exists is never classified as no prior owner",
        );
    }
}
