//! SQLite connection wrapper.
//!
//! Every `Db` instance corresponds to one `rusqlite::Connection` wrapped in
//! `Arc<Mutex<…>>`. WAL allows concurrent readers at the SQLite layer; the
//! mutex serialises Rust-side access to keep `Connection`'s `!Sync` API
//! sound. v1 is a single-writer app — if profiling shows lock contention,
//! the next step is a split (write conn + read pool), not r2d2.
//!
//! Callers reach the connection through `with_conn(|c| …)`. Holding the
//! returned `MutexGuard` across an `.await` point is impossible — the
//! closure-only API enforces it at the type level.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::error::StorageError;
use crate::migrations::{MIGRATIONS, run_migrations};

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Avoid locking on Debug — useful inside panic handlers / tracing
        // where a poisoned mutex would otherwise cascade.
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Run a synchronous block against the underlying connection. The mutex
    /// is held for the duration of `f`. Caller is responsible for keeping
    /// `f` short or wrapping the whole call in `tokio::task::spawn_blocking`.
    pub fn with_conn<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T>,
    {
        let guard = self.conn.lock().expect("Db mutex poisoned");
        f(&guard).map_err(StorageError::Query)
    }
}

/// Open a SQLite database at `path`, applying WAL + FK + busy-timeout
/// pragmas and running every registered migration. Creates the file if it
/// does not exist.
pub fn open(path: &Path) -> Result<Db, StorageError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(path, flags).map_err(StorageError::Open)?;
    set_pragmas(&conn, /* expect_wal = */ true)?;
    run_migrations(&mut conn, MIGRATIONS)?;
    Ok(Db {
        conn: Arc::new(Mutex::new(conn)),
    })
}

/// Open an in-memory database. **Test helper — do not use in production.**
/// Data is discarded the moment the `Db` is dropped. WAL is a no-op for
/// `:memory:`; the pragma is issued for parity but its result is not
/// asserted. Each call yields a fresh isolated DB.
#[doc(hidden)]
pub fn open_memory() -> Result<Db, StorageError> {
    let mut conn = Connection::open_in_memory().map_err(StorageError::Open)?;
    set_pragmas(&conn, /* expect_wal = */ false)?;
    run_migrations(&mut conn, MIGRATIONS)?;
    Ok(Db {
        conn: Arc::new(Mutex::new(conn)),
    })
}

const BUSY_TIMEOUT_MS: i64 = 5_000;
const WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;

/// Set every connection-scoped pragma the app relies on. `foreign_keys` is
/// per-connection so it must run before any transaction.
fn set_pragmas(conn: &Connection, expect_wal: bool) -> Result<(), StorageError> {
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(StorageError::Pragma)?;
    if expect_wal && !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            ?mode,
            "journal_mode did not switch to WAL — possibly read-only fs"
        );
    }

    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(StorageError::Pragma)?;
    conn.pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)
        .map_err(StorageError::Pragma)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(StorageError::Pragma)?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)
        .map_err(StorageError::Pragma)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_pragma_i64(db: &Db, name: &str) -> i64 {
        db.with_conn(|c| c.pragma_query_value(None, name, |row| row.get::<_, i64>(0)))
            .expect("pragma read")
    }

    fn read_pragma_str(db: &Db, name: &str) -> String {
        db.with_conn(|c| c.pragma_query_value(None, name, |row| row.get::<_, String>(0)))
            .expect("pragma read")
    }

    #[test]
    fn open_memory_succeeds() {
        let db = open_memory().expect("memory open");
        // Trivial round-trip through with_conn.
        let one: i64 = db
            .with_conn(|c| c.query_row("SELECT 1", [], |row| row.get(0)))
            .expect("select 1");
        assert_eq!(one, 1);
    }

    #[test]
    fn pragma_foreign_keys_on_memory() {
        let db = open_memory().expect("memory open");
        assert_eq!(read_pragma_i64(&db, "foreign_keys"), 1);
    }

    #[test]
    fn pragma_busy_timeout_memory() {
        let db = open_memory().expect("memory open");
        assert_eq!(read_pragma_i64(&db, "busy_timeout"), BUSY_TIMEOUT_MS);
    }

    #[test]
    fn pragma_synchronous_normal_memory() {
        let db = open_memory().expect("memory open");
        // synchronous: 0=OFF, 1=NORMAL, 2=FULL, 3=EXTRA
        assert_eq!(read_pragma_i64(&db, "synchronous"), 1);
    }

    #[test]
    fn pragma_wal_autocheckpoint_memory() {
        let db = open_memory().expect("memory open");
        assert_eq!(
            read_pragma_i64(&db, "wal_autocheckpoint"),
            WAL_AUTOCHECKPOINT_PAGES
        );
    }

    #[test]
    fn db_clone_shares_connection() {
        let db = open_memory().expect("memory open");
        db.with_conn(|c| c.execute("CREATE TABLE shared (id INTEGER)", []))
            .expect("create");
        let cloned = db.clone();
        cloned
            .with_conn(|c| c.execute("INSERT INTO shared (id) VALUES (1)", []))
            .expect("insert via clone");
        let count: i64 = db
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM shared", [], |row| row.get(0)))
            .expect("count via original");
        assert_eq!(count, 1);
    }

    #[test]
    fn open_file_uses_wal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("oximux-test.db");
        let db = open(&path).expect("file open");
        let mode = read_pragma_str(&db, "journal_mode");
        assert!(
            mode.eq_ignore_ascii_case("wal"),
            "expected wal, got {mode:?}"
        );
    }
}
