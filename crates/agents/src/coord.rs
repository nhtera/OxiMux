//! The coordination blackboard: a small versioned key/value store agents share
//! without routing through anyone's transcript.
//!
//! **Why not just message each other.** Two agents on the same task need a
//! handful of shared facts — which files are claimed, what the schema settled
//! on, whether the migration already ran. Putting those in a transcript means
//! every reader has to parse prose and every writer has to hope it was read.
//! A key with a version is the smaller, honest primitive.
//!
//! **Why versions.** Without them, two agents that read the same value and both
//! write lose one write silently, which is exactly the bug a blackboard is
//! meant to prevent. [`CoordStore::set`] takes the version the caller read and
//! refuses if it has moved, handing back the entry it lost to so the caller can
//! merge rather than re-read.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, params};

/// One stored entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordEntry {
    pub key: String,
    pub value_json: String,
    /// Incremented on every write. First write is version 1, so `if_version: 0`
    /// unambiguously means "only if this key does not exist".
    pub version: u64,
    pub updated_at: DateTime<Local>,
}

/// What a refused conditional write hands back.
#[derive(Debug)]
pub struct VersionConflict {
    /// The entry as it actually stands, or `None` when the caller expected a
    /// value and the key is absent.
    pub current: Option<CoordEntry>,
}

/// The coordination blackboard, backed by the app's SQLite database.
#[derive(Clone)]
pub struct CoordStore {
    conn: Arc<Mutex<Connection>>,
}

impl CoordStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// One key, or `None`.
    pub fn get(&self, key: &str) -> Result<Option<CoordEntry>> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT key, value_json, version, updated_at FROM coord_state WHERE key = ?1",
                params![key],
                |row| {
                    Ok(CoordEntry {
                        key: row.get(0)?,
                        value_json: row.get(1)?,
                        version: row.get::<_, i64>(2)? as u64,
                        updated_at: parse_time(&row.get::<_, String>(3)?),
                    })
                },
            )
            .optional()
            .context("read coordination key")
    }

    /// Every key under `prefix` (or all of them), key order.
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<CoordEntry>> {
        let conn = self.conn.lock().unwrap();
        // `GLOB` rather than `LIKE`: `LIKE` would treat `%` and `_` in a
        // caller-supplied prefix as wildcards, so a key named `a_b` would match
        // `axb`. Escaping the prefix instead would work but puts the
        // correctness of every read in the escaping.
        let mut stmt = conn
            .prepare(
                "SELECT key, value_json, version, updated_at FROM coord_state \
                 WHERE ?1 IS NULL OR key GLOB ?2 ORDER BY key",
            )
            .context("prepare coordination list")?;
        let pattern = prefix.map(|p| format!("{}*", glob_escape(p)));
        let rows = stmt
            .query_map(params![prefix, pattern], |row| {
                Ok(CoordEntry {
                    key: row.get(0)?,
                    value_json: row.get(1)?,
                    version: row.get::<_, i64>(2)? as u64,
                    updated_at: parse_time(&row.get::<_, String>(3)?),
                })
            })
            .context("query coordination state")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read coordination state")?;
        Ok(rows)
    }

    /// Write one key.
    ///
    /// `if_version` is optimistic concurrency: `Some(v)` writes only if the
    /// stored version is exactly `v` (and `Some(0)` only if the key is absent);
    /// `None` overwrites whatever is there. A refused write returns the entry
    /// the caller lost to, never a bare error — a caller that had to re-read to
    /// learn what it collided with would need a second round trip to do
    /// anything with the answer.
    ///
    /// Read and write happen in one transaction, or two callers passing the
    /// same `if_version` could both see it match before either wrote.
    pub fn set(
        &self,
        key: &str,
        value_json: &str,
        if_version: Option<u64>,
        now: DateTime<Local>,
    ) -> Result<Result<CoordEntry, VersionConflict>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("begin coordination write")?;
        let current: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT value_json, version, updated_at FROM coord_state WHERE key = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .context("read coordination key")?;
        let stored_version = current.as_ref().map(|(_, v, _)| *v as u64).unwrap_or(0);
        if let Some(expected) = if_version
            && expected != stored_version
        {
            return Ok(Err(VersionConflict {
                current: current.map(|(value_json, version, updated_at)| CoordEntry {
                    key: key.to_string(),
                    value_json,
                    version: version as u64,
                    updated_at: parse_time(&updated_at),
                }),
            }));
        }
        let version = stored_version + 1;
        tx.execute(
            "INSERT INTO coord_state (key, value_json, version, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(key) DO UPDATE SET value_json = ?2, version = ?3, updated_at = ?4",
            params![key, value_json, version as i64, now.to_rfc3339()],
        )
        .context("write coordination key")?;
        tx.commit().context("commit coordination write")?;
        Ok(Ok(CoordEntry {
            key: key.to_string(),
            value_json: value_json.to_string(),
            version,
            updated_at: now,
        }))
    }

    /// Remove one key. Idempotent — deleting one already gone is success.
    pub fn delete(&self, key: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM coord_state WHERE key = ?1", params![key])
            .context("delete coordination key")?;
        Ok(())
    }
}

/// Neutralize GLOB's own metacharacters in a caller-supplied prefix, so a key
/// containing `*`, `?` or `[` is matched literally rather than as a pattern.
fn glob_escape(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for ch in prefix.chars() {
        match ch {
            '*' | '?' | '[' | ']' => {
                out.push('[');
                out.push(ch);
                out.push(']');
            }
            other => out.push(other),
        }
    }
    out
}

/// An unparseable stored instant reads as *now*: the timestamp is display
/// metadata, and dropping a whole entry over it would be the worse answer.
fn parse_time(text: &str) -> DateTime<Local> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Local))
        .unwrap_or_else(|_| Local::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn store() -> CoordStore {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        CoordStore::new(db.conn())
    }

    fn at(s: &str) -> DateTime<Local> {
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").expect("parse");
        Local.from_local_datetime(&naive).single().expect("unambiguous")
    }

    #[test]
    fn a_written_key_reads_back_at_version_one() {
        let store = store();
        let written = store
            .set("plan", r#"{"step":1}"#, None, at("2026-08-04 09:00:00"))
            .expect("write")
            .expect("no conflict");
        assert_eq!(written.version, 1, "the first write is version 1");
        assert_eq!(store.get("plan").unwrap().unwrap().value_json, r#"{"step":1}"#);
    }

    #[test]
    fn a_missing_key_is_absent_not_an_error() {
        assert_eq!(store().get("nothing").expect("read"), None);
    }

    /// THE property: two writers that both read version 1 cannot both commit.
    #[test]
    fn a_stale_conditional_write_is_refused_with_the_current_entry() {
        let store = store();
        store.set("plan", "1", None, at("2026-08-04 09:00:00")).unwrap().unwrap();
        // Writer A moves it to version 2.
        store.set("plan", "2", Some(1), at("2026-08-04 09:01:00")).unwrap().unwrap();
        // Writer B still holds version 1 and must lose.
        let conflict = store
            .set("plan", "3", Some(1), at("2026-08-04 09:02:00"))
            .unwrap()
            .expect_err("stale write must be refused");
        let current = conflict.current.expect("the winner is handed back");
        assert_eq!(current.version, 2);
        assert_eq!(current.value_json, "2", "so the loser can merge without re-reading");
    }

    /// `if_version: 0` is create-only — the compare-and-swap a claim needs.
    #[test]
    fn version_zero_means_only_if_absent() {
        let store = store();
        store.set("lock", "mine", Some(0), at("2026-08-04 09:00:00")).unwrap().unwrap();
        let conflict = store
            .set("lock", "yours", Some(0), at("2026-08-04 09:01:00"))
            .unwrap()
            .expect_err("the key exists now");
        assert_eq!(conflict.current.expect("current").value_json, "mine");
    }

    #[test]
    fn an_unconditional_write_overwrites_and_bumps_the_version() {
        let store = store();
        store.set("k", "a", None, at("2026-08-04 09:00:00")).unwrap().unwrap();
        let second = store.set("k", "b", None, at("2026-08-04 09:01:00")).unwrap().unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(store.get("k").unwrap().unwrap().value_json, "b");
    }

    #[test]
    fn delete_is_idempotent_and_resets_the_version() {
        let store = store();
        store.set("k", "a", None, at("2026-08-04 09:00:00")).unwrap().unwrap();
        store.delete("k").expect("delete");
        store.delete("k").expect("deleting again is fine");
        assert_eq!(store.get("k").unwrap(), None);
        let fresh = store.set("k", "b", Some(0), at("2026-08-04 09:02:00")).unwrap().unwrap();
        assert_eq!(fresh.version, 1, "a deleted key is absent again, not version 3");
    }

    #[test]
    fn listing_narrows_by_prefix_and_is_key_ordered() {
        let store = store();
        for key in ["team/b", "team/a", "other"] {
            store.set(key, "x", None, at("2026-08-04 09:00:00")).unwrap().unwrap();
        }
        let all = store.list(None).expect("list all");
        assert_eq!(all.len(), 3);
        let team: Vec<String> =
            store.list(Some("team/")).expect("list team").into_iter().map(|e| e.key).collect();
        assert_eq!(team, vec!["team/a", "team/b"]);
    }

    /// A prefix is a literal, not a pattern: GLOB metacharacters in a key must
    /// not let one watcher's prefix match keys it did not name.
    #[test]
    fn a_prefix_with_glob_metacharacters_matches_literally() {
        let store = store();
        store.set("a*b/one", "x", None, at("2026-08-04 09:00:00")).unwrap().unwrap();
        store.set("axb/two", "x", None, at("2026-08-04 09:00:00")).unwrap().unwrap();
        let matched: Vec<String> =
            store.list(Some("a*b/")).expect("list").into_iter().map(|e| e.key).collect();
        assert_eq!(matched, vec!["a*b/one"], "the asterisk is part of the key, not a wildcard");
    }
}
