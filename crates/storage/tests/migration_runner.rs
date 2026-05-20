//! Integration tests: file-backed DB round-trip via the public API.
//!
//! Unit tests in `db.rs` cover the in-memory path; these exercise the
//! create-file + re-open paths and the bookkeeping table contract.

use oximux_storage::{open, open_memory};

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

    // Empty ladder → no rows recorded; row count stable across reopens.
    let row_count: i64 = second
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM __oximux_migrations", [], |row| {
                row.get(0)
            })
        })
        .expect("count");
    assert_eq!(row_count, 0);
}

#[test]
fn open_memory_and_open_file_both_have_bookkeeping() {
    let mem = open_memory().expect("memory");
    let tmp = tempfile::tempdir().expect("tempdir");
    let file = open(&tmp.path().join("oximux.db")).expect("file");

    for db in [&mem, &file] {
        let count: i64 = db
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM __oximux_migrations", [], |row| {
                    row.get(0)
                })
            })
            .expect("count");
        assert_eq!(count, 0, "empty ladder should record 0 rows");
    }
}
