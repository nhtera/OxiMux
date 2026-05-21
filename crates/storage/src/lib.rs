//! oximux-storage
//!
//! SQLite via `rusqlite`, with WAL + per-connection pragmas applied on
//! `open()` and a linear migration ladder that ships zero migrations in
//! Phase 0 and grows from Phase 4 step 3 onward.
//!
//! Two safeguards prevent the v0.9 failure where authored migrations were
//! never registered:
//! 1. Each `Migration` entry embeds its SQL via `include_str!` — a missing
//!    file fails the build, not the runtime.
//! 2. `migrations::tests::migration_ladder_matches_files` enforces a 1:1
//!    correspondence between `migrations/*.sql` files and `MIGRATIONS`
//!    entries — an unregistered file fails CI.
//!
//! Public surface in step 1:
//! - [`Db`] — connection wrapper, `Clone`able, `with_conn(|c| …)` accessor
//! - [`open`] — open a file-backed DB at `path`, run migrations
//! - [`open_memory`] — open an in-memory DB; useful for tests
//! - [`StorageError`] — typed errors at the boundary

pub mod db;
pub mod error;
pub mod migrations;
pub mod model;
pub mod repositories;

pub use db::{Db, open, open_memory};
pub use error::StorageError;
pub use migrations::{MIGRATIONS, Migration};
pub use repositories::{
    AgentSessionRepo, PaneBufferRepo, PaneSessionRepo, ProjectRepo, SettingsRepo, WorkspaceRepo,
};
