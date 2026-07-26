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

    pub fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.db.with_conn(|c| {
            c.execute("DELETE FROM settings WHERE key = ?1", [key])
                .map(|_| ())
        })?;
        Ok(())
    }
}
