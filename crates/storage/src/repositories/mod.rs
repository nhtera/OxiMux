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

mod agent_last_params;
mod agent_session;
mod diff_review_note;
mod pane_buffer;
mod pane_relay_id;
mod pane_session;
mod project;
mod settings;
mod workspace;
mod worktree_settings;

pub use agent_last_params::AgentLastParamsRepo;
pub use agent_session::AgentSessionRepo;
pub use diff_review_note::DiffReviewNoteRepo;
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

/// Smallest representable gap before two sparse ranks are considered collapsed
/// and the list must be renumbered. Far above f64 epsilon so we renumber long
/// before precision actually runs out.
const RANK_MIN_GAP: f64 = 1e-6;

/// Compute the new sparse rank for moving the item at `src` to sit adjacent to
/// the item at `target`, given the current ascending `orders` (one entry per
/// row, in display order).
///
/// Returns `None` when the move is a no-op (`src == target`) or when the
/// bounding neighbours are too close to fit a midpoint — the caller then
/// renumbers the list to integers and retries. The geometry:
/// - dragging *down* (`src < target`) lands the item just after `target`, so
///   the new rank is the midpoint of `target` and `target + 1` (or `+1.0`
///   past the end when `target` is last);
/// - dragging *up* (`src > target`) lands it just before `target`, so the new
///   rank is the midpoint of `target - 1` and `target` (or `-1.0` before the
///   start when `target` is first).
pub(crate) fn reorder_slot_value(orders: &[f64], src: usize, target: usize) -> Option<f64> {
    if src == target || src >= orders.len() || target >= orders.len() {
        return None;
    }
    let (lo, hi): (Option<f64>, Option<f64>) = if src < target {
        // Land after `target`: bounded by target and its successor.
        (Some(orders[target]), orders.get(target + 1).copied())
    } else {
        // Land before `target`: bounded by target's predecessor and target.
        (
            target.checked_sub(1).map(|i| orders[i]),
            Some(orders[target]),
        )
    };
    match (lo, hi) {
        (Some(a), Some(b)) => {
            if (b - a).abs() < RANK_MIN_GAP {
                None // collapsed — caller renumbers and retries
            } else {
                Some((a + b) / 2.0)
            }
        }
        // Open end: append past the max / prepend below the min.
        (Some(a), None) => Some(a + 1.0),
        // Prepend: halve toward zero rather than subtract. Subtracting 1.0
        // from a first-rank of 1.0 would write exactly 0.0 — the
        // not-yet-backfilled sentinel — and the next launch's backfill would
        // silently re-seed (and lose) the reorder. Halving stays strictly in
        // `(0, b)`; if it ever approaches the floor, fall back to renumber.
        (None, Some(b)) => {
            if b <= RANK_MIN_GAP {
                None
            } else {
                Some(b / 2.0)
            }
        }
        (None, None) => None,
    }
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
