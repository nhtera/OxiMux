//! Typed CRUD repositories over the V001 schema. Each repo is a thin
//! newtype around a [`Db`] clone (cheap — `Arc<Mutex<Connection>>` inside),
//! exposes only the methods Phase 4 consumers (steps 4–10) need, and
//! returns domain types from `oximux-core` — callers never see row types.
//!
//! ## Threading
//! Async callers must wrap repo calls in `tokio::task::spawn_blocking`;
//! rusqlite is synchronous and `Db::with_conn` is a blocking lock.
//!
//! ## Error mapping
//! UNIQUE-constraint violations map to [`StorageError::Conflict`] via
//! [`classify_unique`]; every other rusqlite error maps to
//! [`StorageError::Query`].
//!
//! ## Missing-row contract
//! All `update_*` / `mark_*` / `rename` / `delete` methods return
//! `Ok(())` when the target id does not exist. Phase 4 wiring (step 4+)
//! always calls these immediately after a successful `insert` / `get_by_id`
//! that returned `Some`, so "row gone" is a benign race not worth a typed
//! error. Revisit if a consumer needs the distinction.
//!
//! [`Db`]: crate::db::Db

use chrono::Utc;
use rusqlite::ffi::{self, ErrorCode as FfiErrorCode};
use uuid::Uuid;

use crate::error::StorageError;

mod agent_session;
mod pane_buffer;
mod pane_relay_id;
mod pane_session;
mod project;
mod settings;
mod workspace;
mod worktree_settings;

pub use agent_session::AgentSessionRepo;
pub use pane_buffer::PaneBufferRepo;
pub use pane_relay_id::PaneRelayIdRepo;
pub use pane_session::PaneSessionRepo;
pub use project::ProjectRepo;
pub use settings::SettingsRepo;
pub use workspace::WorkspaceRepo;
pub use worktree_settings::WorktreeSettingsRepo;

/// RFC 3339 (UTC, nanosecond precision). SQLite stores it as TEXT and
/// lexicographic ordering matches chronological ordering when every
/// timestamp carries the same trailing `Z`.
pub(crate) fn now() -> String {
    Utc::now().to_rfc3339()
}

/// UUIDv4 string for primary keys. `Uuid::new_v4()` reads from `getrandom`
/// — on macOS-arm64 this is `/dev/urandom` and never blocks.
pub(crate) fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Convert a `StorageError` returned by `Db::with_conn` into a typed
/// `StorageError::Conflict` when it is a UNIQUE / PRIMARY KEY violation.
/// Other variants pass through untouched.
///
/// SQLite extended codes covered:
/// - `2067` (`SQLITE_CONSTRAINT_UNIQUE`) — any UNIQUE-constraint failure
/// - `1555` (`SQLITE_CONSTRAINT_PRIMARYKEY`) — PRIMARY KEY collision on
///   `id TEXT PRIMARY KEY` columns
///
/// `2579` (`SQLITE_CONSTRAINT_ROWID`) is intentionally excluded — it only
/// fires on `INTEGER PRIMARY KEY` columns, which V001 does not use.
pub(crate) fn classify_unique(
    table: &'static str,
    constraint: &'static str,
    err: StorageError,
) -> StorageError {
    if let StorageError::Query(rusqlite::Error::SqliteFailure(
        ffi::Error {
            code: FfiErrorCode::ConstraintViolation,
            extended_code,
        },
        _msg,
    )) = &err
        && matches!(*extended_code, 2067 | 1555)
    {
        return StorageError::Conflict {
            table: table.into(),
            constraint: constraint.into(),
        };
    }
    err
}
