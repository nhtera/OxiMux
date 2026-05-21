//! Integration tests: file-backed DB round-trip via the public API.
//!
//! Unit tests in `db.rs` cover the in-memory path; these exercise the
//! create-file + re-open paths and the bookkeeping table contract.

use oximux_storage::{MIGRATIONS, open, open_memory};

#[test]
fn open_creates_db_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("oximux.db");
    assert!(!path.exists());

    let db = open(&path).expect("open");
    assert!(path.exists(), "expected SQLite file at {}", path.display());

    let bookkeeping_present: i64 = db
        .with_conn(|c| {
            c.query_row(
                "SELECT EXISTS(\
                     SELECT 1 FROM sqlite_master \
                     WHERE type='table' AND name='__oximux_migrations'\
                 )",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .expect("query bookkeeping");
    assert_eq!(bookkeeping_present, 1, "__oximux_migrations not created");
}

#[test]
fn open_twice_is_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("oximux.db");

    {
        let _first = open(&path).expect("first open");
    }
    let second = open(&path).expect("second open");

    // Every registered migration recorded exactly once; second open is a
    // no-op regardless of how many migrations the ladder grows to.
    let row_count: i64 = second
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM __oximux_migrations", [], |row| {
                row.get(0)
            })
        })
        .expect("count");
    assert_eq!(row_count, MIGRATIONS.len() as i64);
}

#[test]
fn open_memory_and_open_file_both_record_v001() {
    let mem = open_memory().expect("memory");
    let tmp = tempfile::tempdir().expect("tempdir");
    let file = open(&tmp.path().join("oximux.db")).expect("file");

    for db in [&mem, &file] {
        let (version, name): (i64, String) = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT version, name FROM __oximux_migrations ORDER BY version",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
            })
            .expect("query v001");
        assert_eq!(version, 1);
        assert_eq!(name, "init");
    }
}

#[test]
fn v001_creates_all_five_tables() {
    let db = open_memory().expect("memory");
    for table in [
        "projects",
        "workspaces",
        "agent_sessions",
        "pane_sessions",
        "settings",
    ] {
        let exists: i64 = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT EXISTS(\
                         SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1\
                     )",
                    [table],
                    |row| row.get(0),
                )
            })
            .expect("query sqlite_master");
        assert_eq!(exists, 1, "missing table: {table}");
    }
}
