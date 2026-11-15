//! `SettingsRepo` — flat TEXT key/value store. Values MUST be ≤ 64 KiB;
//! enforcement is the caller's responsibility (Phase 4 step 3 plan Q9).

use rusqlite::{OptionalExtension, params};

use crate::db::Db;
use crate::error::StorageError;

#[derive(Clone)]
pub struct SettingsRepo {
    db: Db,
}

impl SettingsRepo {
    /// The most a stored value may hold.
    ///
    /// Enforcement stays with the callers — this store takes what it is given
    /// — but the number lives with the contract it belongs to, so the write
    /// paths that hold it cannot drift apart from each other.
    pub const MAX_VALUE_BYTES: usize = 64 * 1024;

    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Identity of the database behind this repo — see [`Db::store_id`].
    /// Repos over the same `Db` agree; repos over different ones never do.
    pub fn store_id(&self) -> u64 {
        self.db.store_id()
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
        let value: Option<String> = self.db.with_conn(|c| {
            c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()
        })?;
        Ok(value)
    }

    /// Upsert. Values MUST be ≤ 64 KiB; enforcement is the caller's
    /// responsibility (see phase-04 step 3 plan Q9).
    pub fn set(&self, key: &str, value: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Upsert every pair in **one transaction**: all of them land, or none do.
    ///
    /// A whole-config write (the desktop's Agent-delegation pane) must not be
    /// able to half-apply. `set` commits per key, so a failure part way through
    /// a loop would leave the store holding a mixture of the old and the new
    /// config — and a later host start would read that mixture back as the
    /// posture to enforce. Values MUST be <= 64 KiB, as in [`Self::set`].
    pub fn set_many(&self, entries: &[(&str, String)]) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            for (key, value) in entries {
                tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )?;
            }
            tx.commit()
        })?;
        Ok(())
    }

    /// Every `(key, value)` whose key starts with `prefix`, unordered.
    ///
    /// Exists for the headless host's session catalog, which enumerates the
    /// `agent_chat:` transcript blobs — the settings table is the only index
    /// of persisted chat sessions, and without a prefix scan a headless host
    /// would need a second bookkeeping table that could drift from the blobs.
    /// `LIKE` with the trailing `%` uses the primary-key index; `\\` escapes
    /// the two LIKE metacharacters so a literal `%`/`_` in a prefix cannot
    /// widen the scan.
    pub fn list_prefixed(&self, prefix: &str) -> Result<Vec<(String, String)>, StorageError> {
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let rows = self.db.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT key, value FROM settings WHERE key LIKE ?1 ESCAPE '\\'")?;
            let rows = stmt
                .query_map([format!("{escaped}%")], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<(String, String)>, _>>()?;
            Ok(rows)
        })?;
        Ok(rows)
    }

    pub fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM settings WHERE key = ?1", [key])
                .map(|_| ())
        })?;
        Ok(())
    }
}
