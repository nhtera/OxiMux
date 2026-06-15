//! Migration ladder.
//!
//! Each entry is a `(version, name, sql)` triple. New migrations MUST be
//! appended to `MIGRATIONS` AND have a matching `migrations/V<NNN>__*.sql`
//! file referenced via `include_str!`. Two guards in tandem:
//!
//! 1. `include_str!` is evaluated at compile time — a missing file fails the
//!    build, not the runtime.
//! 2. The CI test `migration_ladder_matches_files` counts `.sql` files in
//!    `migrations/` against `MIGRATIONS.len()` — an unregistered file fails
//!    CI, not the user's machine.
//!
//! Versions are linear, ascending, gap-free. The runner applies any version
//! not yet recorded in `__oximux_migrations`, in ascending order, one
//! transaction per migration.

use std::path::PathBuf;

use rusqlite::{Connection, params};

use crate::error::StorageError;

/// Name of the bookkeeping table that records applied migrations.
const APPLIED_TABLE: &str = "__oximux_migrations";

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    /// Bare DDL only. The runner wraps each migration in its own
    /// transaction — embedded `BEGIN` / `COMMIT` / `SAVEPOINT` in the SQL
    /// is rejected at apply time, since SQLite cannot nest transactions
    /// and the resulting error message hides the real cause.
    pub sql: &'static str,
}

/// V001 lands the five-table OxiMux schema (projects, workspaces,
/// agent_sessions, pane_sessions, settings) plus three FK-support
/// indexes. V002 adds `pane_buffers` (per-pane scrollback snapshots for
/// terminal-tab restore, Phase 4 step 16). V003 adds `pane_relay_ids`
/// (per-pane mapping to relay-side PTY ids for cross-restart attach,
/// Phase 5 step 5). V004 extends `pane_buffers` with `sub_pane_ordinal`
/// so multi-sub-pane terminal tabs (Cmd+D splits, F3.4) keep one
/// scrollback per leaf. V005 adds `window_id` to both `pane_buffers`
/// and `pane_relay_ids` primary keys so two app windows sharing a
/// project never clobber each other's saved state; legacy rows default
/// to window_id='main'. V006 adds `worktree_settings` — one row per
/// workspace holding SCM scratch state (base ref selection, in-flight
/// commit draft, view-mode override) keyed by `workspace_id`. V007
/// drops the FK on `worktree_settings.workspace_id`: the SCM panel
/// uses a worktree filesystem path as the key, not a `workspaces.id`
/// UUID, so the FK was always violated on a freshly-opened project
/// with no workspace row. V008 widens the `pane_relay_ids` primary key
/// with `sub_pane` + `tab` so every split leaf / per-pane tab persists
/// its own relay PTY id and re-attaches independently across restart;
/// legacy rows copy forward with sub_pane=0, tab=0. V009 adds
/// `agent_last_params` — one row per agent adapter remembering the
/// last-used model + reasoning-effort so the launch picker can
/// preselect. V010 adds `diff_review_notes` — per-line diff review
/// notes keyed by a `(repo, diff_ref, path, side, line)` anchor so a
/// review survives tab close + restart. V011 links a workspace to the
/// GitHub issue/PR it was created from; V012 adds an optional per-
/// workspace identifier hue; V013 drops the `agent_sessions` workspace
/// FK. V014 adds `projects.sort_order` and V015 adds
/// `workspaces.sort_order` — sparse REAL ranks driving manual
/// drag-to-reorder in the left rail; a one-shot Rust backfill seeds
/// existing rows from their current display order. Future migrations
/// append; never reorder, never rewrite.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        sql: include_str!("../migrations/V001__init.sql"),
    },
    Migration {
        version: 2,
        name: "pane_buffers",
        sql: include_str!("../migrations/V002__pane_buffers.sql"),
    },
    Migration {
        version: 3,
        name: "pane_relay_ids",
        sql: include_str!("../migrations/V003__pane_relay_ids.sql"),
    },
    Migration {
        version: 4,
        name: "pane_buffers_sub_pane",
        sql: include_str!("../migrations/V004__pane_buffers_sub_pane.sql"),
    },
    Migration {
        version: 5,
        name: "per_window_persistence",
        sql: include_str!("../migrations/V005__per_window_persistence.sql"),
    },
    Migration {
        version: 6,
        name: "worktree_settings",
        sql: include_str!("../migrations/V006__worktree_settings.sql"),
    },
    Migration {
        version: 7,
        name: "worktree_settings_drop_fk",
        sql: include_str!("../migrations/V007__worktree_settings_drop_fk.sql"),
    },
    Migration {
        version: 8,
        name: "pane_relay_ids_leaf_key",
        sql: include_str!("../migrations/V008__pane_relay_ids_leaf_key.sql"),
    },
    Migration {
        version: 9,
        name: "agent_last_params",
        sql: include_str!("../migrations/V009__agent_last_params.sql"),
    },
    Migration {
        version: 10,
        name: "diff_review_notes",
        sql: include_str!("../migrations/V010__diff_review_notes.sql"),
    },
    Migration {
        version: 11,
        name: "workspace_linked_issue",
        sql: include_str!("../migrations/V011__workspace_linked_issue.sql"),
    },
    Migration {
        version: 12,
        name: "workspace_tint",
        sql: include_str!("../migrations/V012__workspace_tint.sql"),
    },
    Migration {
        version: 13,
        name: "agent_sessions_drop_workspace_fk",
        sql: include_str!("../migrations/V013__agent_sessions_drop_workspace_fk.sql"),
    },
    Migration {
        version: 14,
        name: "projects_sort_order",
        sql: include_str!("../migrations/V014__projects_sort_order.sql"),
    },
    Migration {
        version: 15,
        name: "workspaces_sort_order",
        sql: include_str!("../migrations/V015__workspaces_sort_order.sql"),
    },
];

/// Returns the absolute path to the `migrations/` directory at runtime.
/// The CI guard uses this to count `.sql` files.
pub fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
}

/// Apply every migration in `migrations` whose version is not already
/// recorded in `__oximux_migrations`, in ascending version order, each
/// inside its own transaction.
///
/// Idempotent: running against an already-migrated database is a no-op.
/// Downgrade-safe: if any recorded version is greater than the highest
/// version in `migrations`, returns `StorageError::SchemaMigrationDowngrade`
/// without modifying state.
pub(crate) fn run_migrations(
    conn: &mut Connection,
    migrations: &[Migration],
) -> Result<(), StorageError> {
    conn.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {APPLIED_TABLE} (\n\
                 version INTEGER PRIMARY KEY,\n\
                 name TEXT NOT NULL,\n\
                 applied_at TEXT NOT NULL\n\
             )"
        ),
        [],
    )
    .map_err(StorageError::Query)?;

    let recorded = load_applied_versions(conn)?;
    let db_max = recorded.iter().copied().max();
    let code_max = migrations.iter().map(|m| m.version).max();

    // Downgrade-by-max takes priority: if the DB has v5 and code only knows
    // up to v2, that is a rollback and should be reported as such regardless
    // of whether the lower versions are present.
    if let (Some(db_v), Some(code_v)) = (db_max, code_max)
        && db_v > code_v
    {
        return Err(StorageError::SchemaMigrationDowngrade {
            db_version: db_v,
            code_version: code_v,
        });
    }
    // db_v > 0 with no code-side migrations also counts as downgrade.
    if let (Some(db_v), None) = (db_max, code_max) {
        return Err(StorageError::SchemaMigrationDowngrade {
            db_version: db_v,
            code_version: 0,
        });
    }

    // M2: enforce gap-free invariant *after* downgrade detection. If the DB
    // has [1, 3] and code re-issues [1, 2, 3], applying v2 after v3 is
    // already recorded would corrupt the schema. Refuse rather than corrupt.
    if let Some(missing) = first_gap(&recorded) {
        return Err(StorageError::SchemaMigrationDowngrade {
            db_version: *recorded.iter().max().expect("non-empty checked"),
            code_version: missing.saturating_sub(1),
        });
    }

    let mut sorted: Vec<&Migration> = migrations.iter().collect();
    sorted.sort_by_key(|m| m.version);

    for migration in sorted {
        if recorded.contains(&migration.version) {
            continue;
        }
        apply_one(conn, migration)?;
    }

    Ok(())
}

fn load_applied_versions(conn: &Connection) -> Result<Vec<u32>, StorageError> {
    let mut stmt = conn
        .prepare(&format!("SELECT version FROM {APPLIED_TABLE}"))
        .map_err(StorageError::Query)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(StorageError::Query)?;
    let mut out = Vec::new();
    for r in rows {
        let v = r.map_err(StorageError::Query)?;
        // M4: refuse silent wrap. A negative or > u32::MAX recorded version
        // means external tampering — surface it as a Query error rather than
        // letting the cast produce nonsense like 4_294_967_295.
        let v_u32 = u32::try_from(v)
            .map_err(|_| StorageError::Query(rusqlite::Error::IntegralValueOutOfRange(0, v)))?;
        out.push(v_u32);
    }
    Ok(out)
}

/// Returns the smallest missing version in a sorted-by-value set, or `None`
/// when the set is empty or contiguous starting at 1.
fn first_gap(versions: &[u32]) -> Option<u32> {
    if versions.is_empty() {
        return None;
    }
    let mut sorted: Vec<u32> = versions.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    for (i, v) in sorted.iter().enumerate() {
        let expected = (i as u32) + 1;
        if *v != expected {
            return Some(expected);
        }
    }
    None
}

fn apply_one(conn: &mut Connection, migration: &Migration) -> Result<(), StorageError> {
    // H1: defensive lint against embedded transaction directives. SQLite
    // does not nest transactions; the resulting error from execute_batch
    // would surface as `cannot start a transaction within a transaction`
    // and look like a schema error. Catch it here with the migration version
    // baked into the message.
    debug_assert!(
        !contains_transaction_keyword(migration.sql),
        "migration v{} ({}) embeds BEGIN/COMMIT/SAVEPOINT — see Migration.sql doc",
        migration.version,
        migration.name,
    );

    let tx = conn
        .transaction()
        .map_err(|source| StorageError::Migration {
            version: migration.version,
            source,
        })?;

    tx.execute_batch(migration.sql)
        .map_err(|source| StorageError::Migration {
            version: migration.version,
            source,
        })?;

    let applied_at = current_timestamp();
    tx.execute(
        &format!("INSERT INTO {APPLIED_TABLE} (version, name, applied_at) VALUES (?1, ?2, ?3)"),
        params![migration.version, migration.name, applied_at],
    )
    .map_err(|source| StorageError::Migration {
        version: migration.version,
        source,
    })?;

    tx.commit().map_err(|source| StorageError::Migration {
        version: migration.version,
        source,
    })?;

    tracing::info!(
        version = migration.version,
        name = migration.name,
        "applied migration"
    );
    Ok(())
}

/// Unix epoch seconds as a decimal string. SQLite stores it as TEXT — fine
/// for ORDER BY and `datetime(applied_at, 'unixepoch')` rendering at query
/// time. We don't parse it back in v1.
fn current_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Cheap upper-case scan for the four transaction directives after
/// stripping SQL comments. Treats the SQL as case-insensitive ASCII and
/// matches whole-word occurrences only (so a column named `committed_at`
/// doesn't trip the check, and a comment mentioning `BEGIN` is ignored).
fn contains_transaction_keyword(sql: &str) -> bool {
    let stripped = strip_sql_comments(sql);
    let upper = stripped.to_ascii_uppercase();
    for kw in ["BEGIN", "COMMIT", "SAVEPOINT", "ROLLBACK"] {
        let bytes = upper.as_bytes();
        let word_boundary = |b: u8| !b.is_ascii_alphanumeric() && b != b'_';
        let mut search_from = 0;
        while let Some(rel) = upper[search_from..].find(kw) {
            let pos = search_from + rel;
            let before = pos.checked_sub(1).map(|i| bytes[i]).unwrap_or(b' ');
            let after = bytes.get(pos + kw.len()).copied().unwrap_or(b' ');
            if word_boundary(before) && word_boundary(after) {
                return true;
            }
            search_from = pos + kw.len();
        }
    }
    false
}

/// Strip `-- line comments` and `/* block comments */` from a SQL string.
/// Comment-stripping is the simplest way to keep the keyword check from
/// false-positiving on prose like "no embedded BEGIN/COMMIT directives"
/// that legitimately documents the constraint.
fn strip_sql_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // Skip to EOL.
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI guard: number of `.sql` files in `migrations/` MUST equal
    /// `MIGRATIONS.len()`. Prevents the v0.9 failure where authored
    /// migrations were never registered.
    #[test]
    fn migration_ladder_matches_files() {
        let dir = migrations_dir();
        let sql_count = if dir.exists() {
            std::fs::read_dir(&dir)
                .expect("read migrations dir")
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("sql"))
                .count()
        } else {
            0
        };
        assert_eq!(
            sql_count,
            MIGRATIONS.len(),
            "migrations/*.sql count ({sql_count}) != registered MIGRATIONS ({})",
            MIGRATIONS.len()
        );
    }

    fn mem_conn() -> Connection {
        Connection::open_in_memory().expect("open :memory:")
    }

    fn applied_versions(conn: &Connection) -> Vec<u32> {
        load_applied_versions(conn).expect("load applied")
    }

    #[test]
    fn run_migrations_empty_is_noop() {
        let mut conn = mem_conn();
        run_migrations(&mut conn, &[]).expect("empty run ok");
        assert!(applied_versions(&conn).is_empty());
    }

    #[test]
    fn run_migrations_applies_in_order() {
        let mut conn = mem_conn();
        let ms = &[
            Migration {
                version: 1,
                name: "create_t1",
                sql: "CREATE TABLE t1 (id INTEGER PRIMARY KEY)",
            },
            Migration {
                version: 2,
                name: "create_t2",
                sql: "CREATE TABLE t2 (id INTEGER PRIMARY KEY)",
            },
        ];
        run_migrations(&mut conn, ms).expect("apply both");
        assert_eq!(applied_versions(&conn), vec![1, 2]);
        // both side-effects present
        conn.execute("INSERT INTO t1 (id) VALUES (1)", [])
            .expect("t1 insert");
        conn.execute("INSERT INTO t2 (id) VALUES (1)", [])
            .expect("t2 insert");
    }

    #[test]
    fn run_migrations_idempotent() {
        let mut conn = mem_conn();
        let ms = &[Migration {
            version: 1,
            name: "create_t1",
            sql: "CREATE TABLE t1 (id INTEGER PRIMARY KEY)",
        }];
        run_migrations(&mut conn, ms).expect("first run");
        run_migrations(&mut conn, ms).expect("second run no-op");
        assert_eq!(applied_versions(&conn), vec![1]);
    }

    #[test]
    fn run_migrations_partial_resume() {
        let mut conn = mem_conn();
        let ms_v1 = &[Migration {
            version: 1,
            name: "v1",
            sql: "CREATE TABLE t1 (id INTEGER PRIMARY KEY)",
        }];
        run_migrations(&mut conn, ms_v1).expect("v1");

        let ms_v1_v2 = &[
            Migration {
                version: 1,
                name: "v1",
                sql: "-- unused; already applied",
            },
            Migration {
                version: 2,
                name: "v2",
                sql: "CREATE TABLE t2 (id INTEGER PRIMARY KEY)",
            },
        ];
        run_migrations(&mut conn, ms_v1_v2).expect("v2 applied");
        assert_eq!(applied_versions(&conn), vec![1, 2]);
    }

    #[test]
    fn run_migrations_downgrade_error() {
        let mut conn = mem_conn();
        let high = &[Migration {
            version: 5,
            name: "v5",
            sql: "CREATE TABLE high (id INTEGER PRIMARY KEY)",
        }];
        run_migrations(&mut conn, high).expect("v5 applied");

        let low = &[
            Migration {
                version: 1,
                name: "v1",
                sql: "CREATE TABLE low1 (id INTEGER PRIMARY KEY)",
            },
            Migration {
                version: 2,
                name: "v2",
                sql: "CREATE TABLE low2 (id INTEGER PRIMARY KEY)",
            },
        ];
        let err = run_migrations(&mut conn, low).expect_err("downgrade");
        match err {
            StorageError::SchemaMigrationDowngrade {
                db_version,
                code_version,
            } => {
                assert_eq!(db_version, 5);
                assert_eq!(code_version, 2);
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn run_migrations_downgrade_when_code_empty() {
        let mut conn = mem_conn();
        let any = &[Migration {
            version: 1,
            name: "v1",
            sql: "CREATE TABLE only_one (id INTEGER PRIMARY KEY)",
        }];
        run_migrations(&mut conn, any).expect("v1 applied");
        let err = run_migrations(&mut conn, &[]).expect_err("downgrade vs empty");
        assert!(matches!(
            err,
            StorageError::SchemaMigrationDowngrade {
                db_version: 1,
                code_version: 0,
            }
        ));
    }

    #[test]
    fn run_migrations_rejects_recorded_gap() {
        // Simulate manual tampering: insert v3 without v1/v2 ever recorded.
        let mut conn = mem_conn();
        conn.execute(
            &format!(
                "CREATE TABLE {APPLIED_TABLE} (\
                     version INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL, \
                     applied_at TEXT NOT NULL)"
            ),
            [],
        )
        .expect("seed table");
        conn.execute(
            &format!("INSERT INTO {APPLIED_TABLE} VALUES (3, 'v3_orphan', '0')"),
            [],
        )
        .expect("seed v3 orphan");

        let code = &[
            Migration {
                version: 1,
                name: "v1",
                sql: "CREATE TABLE t1 (id INTEGER PRIMARY KEY)",
            },
            Migration {
                version: 2,
                name: "v2",
                sql: "CREATE TABLE t2 (id INTEGER PRIMARY KEY)",
            },
            Migration {
                version: 3,
                name: "v3",
                sql: "CREATE TABLE t3 (id INTEGER PRIMARY KEY)",
            },
        ];
        let err = run_migrations(&mut conn, code).expect_err("gap detected");
        assert!(
            matches!(err, StorageError::SchemaMigrationDowngrade { .. }),
            "expected SchemaMigrationDowngrade, got {err:?}"
        );
    }

    #[test]
    fn load_applied_versions_rejects_negative_version() {
        let mut conn = mem_conn();
        run_migrations(&mut conn, &[]).expect("create table");
        conn.execute(
            &format!("INSERT INTO {APPLIED_TABLE} VALUES (-1, 'tampered', '0')"),
            [],
        )
        .expect("seed negative");
        let err = load_applied_versions(&conn).expect_err("negative refused");
        assert!(matches!(err, StorageError::Query(_)));
    }

    #[test]
    fn first_gap_detects_missing() {
        assert_eq!(first_gap(&[]), None);
        assert_eq!(first_gap(&[1]), None);
        assert_eq!(first_gap(&[1, 2, 3]), None);
        assert_eq!(first_gap(&[1, 3]), Some(2));
        assert_eq!(first_gap(&[2, 3]), Some(1));
        assert_eq!(first_gap(&[3, 1, 2, 5]), Some(4));
    }

    #[test]
    fn contains_transaction_keyword_matches_bare() {
        assert!(contains_transaction_keyword(
            "BEGIN; CREATE TABLE x (id INT);"
        ));
        assert!(contains_transaction_keyword(
            "CREATE TABLE x (id INT); COMMIT;"
        ));
        assert!(contains_transaction_keyword(
            "savepoint sp1; CREATE TABLE x (id INT);"
        ));
        assert!(!contains_transaction_keyword(
            "CREATE TABLE x (committed_at TEXT, beginning TEXT);"
        ));
        assert!(!contains_transaction_keyword(
            "CREATE TABLE x (id INTEGER PRIMARY KEY);"
        ));
    }

    #[test]
    fn contains_transaction_keyword_ignores_comments() {
        // V001's lead-in mentions BEGIN/COMMIT in a comment — must not trip.
        let sql = "-- script in a single transaction, so no embedded BEGIN/COMMIT directives.\n\
                   CREATE TABLE x (id INTEGER PRIMARY KEY);";
        assert!(!contains_transaction_keyword(sql));

        // Block comment with the keyword inside.
        let sql_block = "/* BEGIN here is documentation, not DDL */\n\
                         CREATE TABLE x (id INTEGER PRIMARY KEY);";
        assert!(!contains_transaction_keyword(sql_block));

        // After stripping the harmless `-- COMMIT` mention, the real BEGIN
        // remains and is detected.
        let sql_mixed = "-- harmless mention of COMMIT\n\
                         BEGIN; CREATE TABLE x (id INT);";
        assert!(contains_transaction_keyword(sql_mixed));
    }

    #[test]
    fn run_migrations_bad_sql_rolls_back() {
        let mut conn = mem_conn();
        let ms = &[
            Migration {
                version: 1,
                name: "v1",
                sql: "CREATE TABLE good (id INTEGER PRIMARY KEY)",
            },
            Migration {
                version: 2,
                name: "v2_bad",
                sql: "CREATE TABLE bad (id INTEGER PRIMARY KEY); GARBAGE_SQL not_valid",
            },
        ];
        let err = run_migrations(&mut conn, ms).expect_err("v2 fails");
        match err {
            StorageError::Migration { version, .. } => assert_eq!(version, 2),
            other => panic!("unexpected error variant: {other:?}"),
        }
        // v1 stuck; v2 rolled back; `bad` table must not exist.
        assert_eq!(applied_versions(&conn), vec![1]);
        let bad_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='bad')",
                [],
                |row| row.get::<_, i64>(0).map(|v| v == 1),
            )
            .expect("query sqlite_master");
        assert!(!bad_exists, "bad table should be rolled back");
    }
}
