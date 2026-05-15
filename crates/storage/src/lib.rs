//! oximux-storage
//!
//! SQLite via `rusqlite` + a migration ladder with a CI guard:
//! `migrations/*.sql` count MUST equal `MIGRATIONS.len()`. This is the rule
//! that fixes the v0.9 failure where V012/V013 were authored but never
//! registered, so callers ran against a schema that no longer existed.
//!
//! Phase 0 = empty ladder + the lookup that the CI guard exercises.
//! Phase 4 fills in real schemas.

use anyhow::Result;
use rusqlite::Connection;

pub mod migrations;

pub use migrations::{MIGRATIONS, Migration};

/// Open a SQLite database at `path` and bring its schema up to the head of
/// `MIGRATIONS`. Phase 4 implements the actual ladder runner.
pub fn open(_path: &std::path::Path) -> Result<Connection> {
    anyhow::bail!("oximux-storage::open is not implemented until Phase 4");
}
