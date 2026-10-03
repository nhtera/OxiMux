//! Opening the index: pragmas, schema check (mismatch → delete + rebuild),
//! and owner-only permissions on the file and its WAL/SHM siblings.
//!
//! One writer connection lives on the indexer thread; every reader opens its
//! own short-lived read-only connection (WAL lets them run concurrently).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::schema::{SCHEMA, SCHEMA_VERSION};

// `busy_timeout` first, so the pragmas after it wait out a reader's lock
// instead of failing.
const PRAGMAS: &str = "PRAGMA busy_timeout=5000;
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA journal_size_limit=8388608;";

/// Open (creating or rebuilding as needed) the writer connection.
pub fn open_writer(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        oximux_owner_only::prepare_owner_only_dir(dir)
            .with_context(|| format!("preparing {}", dir.display()))?;
    }
    let conn = match open_checked(path).context("opening the session index")? {
        Some(conn) => conn,
        None => {
            // Missing, foreign, or an older schema: the index is derived
            // data, so start over.
            delete_index_files(path)?;
            let conn = Connection::open(path)?;
            // `auto_vacuum` only takes effect before the first table exists.
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
            conn.execute_batch(PRAGMAS)?;
            conn.execute_batch(SCHEMA).context("creating the session index (FTS5)")?;
            conn.execute("INSERT INTO meta(key, value) VALUES ('schema_version', ?1)", [SCHEMA_VERSION])?;
            conn
        }
    };
    for p in index_files(path) {
        if p.exists() {
            oximux_owner_only::restrict_file(&p).with_context(|| format!("restricting {}", p.display()))?;
        }
    }
    Ok(conn)
}

/// The existing index at `path` when its schema matches; `None` when it is
/// missing, not an index, corrupt, or another schema version — all reasons
/// to rebuild. Any other failure (a lock, an I/O error) is returned, so a
/// transient problem never deletes a good index.
fn open_checked(path: &Path) -> Result<Option<Connection>> {
    use rusqlite::ErrorCode::{DatabaseCorrupt, NotADatabase};
    if !path.exists() {
        return Ok(None);
    }
    let rebuild = |e: &rusqlite::Error| matches!(e.sqlite_error_code(), Some(NotADatabase | DatabaseCorrupt));
    let conn = Connection::open(path)?;
    if let Err(e) = conn.execute_batch(PRAGMAS) {
        return if rebuild(&e) { Ok(None) } else { Err(e.into()) };
    }
    let has_meta: bool = match conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(n) => n > 0,
        Err(e) if rebuild(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !has_meta {
        return Ok(None);
    }
    let version: Option<String> =
        conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).optional()?;
    Ok((version.as_deref() == Some(SCHEMA_VERSION)).then_some(conn))
}

/// A read-only connection for queries and status. Fails when the index does
/// not exist yet.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// The database file and its WAL/SHM siblings.
pub fn index_files(path: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut s = path.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    };
    [path.to_path_buf(), with("-wal"), with("-shm")]
}

/// Delete the index (all three files). Missing files are fine.
pub fn delete_index_files(path: &Path) -> Result<()> {
    for p in index_files(path) {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
        }
    }
    Ok(())
}

/// Bytes the index occupies on disk (database + WAL).
pub fn index_size_bytes(path: &Path) -> u64 {
    index_files(path).iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum()
}
