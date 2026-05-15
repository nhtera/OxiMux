//! Migration ladder.
//!
//! Each entry is a `(version, sql)` pair. New migrations MUST be appended to
//! `MIGRATIONS` AND have a matching `migrations/V<NNN>__*.sql` file. The CI
//! guard `migration_ladder_check` enforces both directions.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
}

/// Phase 0 ships an empty ladder. Phase 4 appends V001 (projects, workspaces,
/// panes) and onward.
pub const MIGRATIONS: &[Migration] = &[];

/// Returns the absolute path to the `migrations/` directory at runtime.
/// The CI guard uses this to count `.sql` files.
pub fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI guard: number of `.sql` files in `migrations/` MUST equal
    /// `MIGRATIONS.len()`. This prevents the v0.9 failure where authored
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
}
