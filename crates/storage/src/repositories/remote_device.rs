//! `RemoteDeviceRepo` — durable remote-control paired devices, so the authorized
//! set + each device's scope + revocation survive an app restart. The in-memory
//! `AuthStore` (in `oximux-remote-host`) seeds from [`list_all`](RemoteDeviceRepo::list_all)
//! at boot and writes through on register / revoke. Keyed by the lowercase-hex
//! app-signing pubkey.

use rusqlite::params;

use crate::db::Db;
use crate::error::StorageError;
use crate::repositories::now;

/// A device's authorization scope — the storage mirror of the host's
/// `DeviceScope`. `Full` for a static/global pairing (the default); `Sessions`
/// for a session-bound one-time ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteScope {
    Full,
    Sessions(Vec<String>),
}

/// One persisted paired device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDeviceRow {
    pub pubkey: String,
    pub name: String,
    pub scope: RemoteScope,
    pub revoked: bool,
    /// The opt-down tier: the device may read (transcripts, status, diffs) but
    /// every state-changing RPC is refused. Orthogonal to `scope`.
    pub read_only: bool,
}

#[derive(Clone)]
pub struct RemoteDeviceRepo {
    db: Db,
}

impl RemoteDeviceRepo {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Every recorded device (including revoked ones — they must stay known so a
    /// re-`Register` can't resurrect them). Used to seed the `AuthStore` at boot.
    pub fn list_all(&self) -> Result<Vec<RemoteDeviceRow>, StorageError> {
        let rows = self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT pubkey, name, scope, scope_sessions, revoked, read_only \
                 FROM remote_devices",
            )?;
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)? != 0,
                    row.get::<_, i64>(5)? != 0,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows
            .into_iter()
            .map(|(pubkey, name, scope, sessions, revoked, read_only)| RemoteDeviceRow {
                pubkey,
                name,
                scope: decode_scope(&scope, sessions.as_deref()),
                revoked,
                read_only,
            })
            .collect())
    }

    /// Insert or replace a device's record — used on first `Register` and on any
    /// later scope/name change. Preserves the original `paired_at` on conflict.
    pub fn upsert(
        &self,
        pubkey: &str,
        name: &str,
        scope: &RemoteScope,
        revoked: bool,
    ) -> Result<(), StorageError> {
        let (scope_kind, scope_sessions) = encode_scope(scope);
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO remote_devices \
                     (pubkey, name, scope, scope_sessions, revoked, paired_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(pubkey) DO UPDATE SET \
                     name = excluded.name, \
                     scope = excluded.scope, \
                     scope_sessions = excluded.scope_sessions, \
                     revoked = excluded.revoked",
                params![pubkey, name, scope_kind, scope_sessions, revoked as i64, ts],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Set a device's read-only tier. No-op for an unknown pubkey.
    pub fn set_read_only(&self, pubkey: &str, read_only: bool) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE remote_devices SET read_only = ?2 WHERE pubkey = ?1",
                params![pubkey, read_only as i64],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Flip a device's revoked flag (revoke or explicit un-revoke). No-op for an
    /// unknown pubkey.
    pub fn set_revoked(&self, pubkey: &str, revoked: bool) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE remote_devices SET revoked = ?2 WHERE pubkey = ?1",
                params![pubkey, revoked as i64],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Record that a device just connected. No-op for an unknown pubkey.
    pub fn touch_last_seen(&self, pubkey: &str) -> Result<(), StorageError> {
        let ts = now();
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE remote_devices SET last_seen = ?2 WHERE pubkey = ?1",
                params![pubkey, ts],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Permanently forget a device (the user removed it from the paired list).
    pub fn remove(&self, pubkey: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM remote_devices WHERE pubkey = ?1", params![pubkey])
                .map(|_| ())
        })?;
        Ok(())
    }
}

/// `RemoteScope` → `(scope_kind, scope_sessions)` columns.
fn encode_scope(scope: &RemoteScope) -> (&'static str, Option<String>) {
    match scope {
        RemoteScope::Full => ("full", None),
        // Session ids are single-line tokens, so a newline join round-trips
        // without escaping.
        RemoteScope::Sessions(sessions) => ("sessions", Some(sessions.join("\n"))),
    }
}

/// `(scope_kind, scope_sessions)` columns → `RemoteScope`. Unknown kinds degrade
/// to `Full` rather than failing a load.
fn decode_scope(kind: &str, sessions: Option<&str>) -> RemoteScope {
    match kind {
        "sessions" => {
            let list = sessions
                .map(|s| s.split('\n').filter(|x| !x.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            RemoteScope::Sessions(list)
        }
        _ => RemoteScope::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[test]
    fn upsert_then_list_roundtrips_full_and_session_scopes() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);

        repo.upsert("aa", "Phone", &RemoteScope::Full, false).expect("upsert full");
        repo.upsert(
            "bb",
            "Tablet",
            &RemoteScope::Sessions(vec!["sess-1".into(), "sess-2".into()]),
            false,
        )
        .expect("upsert scoped");

        let mut all = repo.list_all().expect("list");
        all.sort_by(|a, b| a.pubkey.cmp(&b.pubkey));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], RemoteDeviceRow {
            pubkey: "aa".into(),
            name: "Phone".into(),
            scope: RemoteScope::Full,
            revoked: false,
            // A freshly paired device is read-write; read-only is an opt-down.
            read_only: false,
        });
        assert_eq!(
            all[1].scope,
            RemoteScope::Sessions(vec!["sess-1".into(), "sess-2".into()])
        );
    }

    /// The opt-down persists and is independent of the revoked flag.
    #[test]
    fn read_only_round_trips_and_defaults_off() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);
        repo.upsert("aa", "Phone", &RemoteScope::Full, false).expect("insert");
        assert!(!repo.list_all().unwrap()[0].read_only, "pairing grants read-write");

        repo.set_read_only("aa", true).expect("set read-only");
        let row = repo.list_all().unwrap().into_iter().next().unwrap();
        assert!(row.read_only, "the opt-down persists");
        assert!(!row.revoked, "read-only is not revocation");

        repo.set_read_only("aa", false).expect("restore");
        assert!(!repo.list_all().unwrap()[0].read_only, "and can be lifted again");
    }

    #[test]
    fn upsert_updates_scope_and_name_in_place() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);
        repo.upsert("aa", "Old", &RemoteScope::Full, false).expect("insert");
        repo.upsert("aa", "New", &RemoteScope::Sessions(vec!["s".into()]), false)
            .expect("update");
        let all = repo.list_all().expect("list");
        assert_eq!(all.len(), 1, "upsert replaces, not duplicates");
        assert_eq!(all[0].name, "New");
        assert_eq!(all[0].scope, RemoteScope::Sessions(vec!["s".into()]));
    }

    #[test]
    fn revoked_devices_persist_and_survive_in_the_listing() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);
        repo.upsert("aa", "Phone", &RemoteScope::Full, false).expect("insert");
        repo.set_revoked("aa", true).expect("revoke");
        let all = repo.list_all().expect("list");
        assert_eq!(all.len(), 1, "a revoked device stays recorded, not deleted");
        assert!(all[0].revoked, "revocation persisted");
    }

    #[test]
    fn remove_forgets_the_device() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);
        repo.upsert("aa", "Phone", &RemoteScope::Full, false).expect("insert");
        repo.remove("aa").expect("remove");
        assert!(repo.list_all().expect("list").is_empty());
    }

    #[test]
    fn updates_to_unknown_pubkey_are_noops() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);
        repo.set_revoked("ghost", true).expect("no-op revoke");
        repo.touch_last_seen("ghost").expect("no-op touch");
        assert!(repo.list_all().expect("list").is_empty());
    }
}
