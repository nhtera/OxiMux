//! Project domain type — one root directory the user has opened.
//!
//! Persisted in the `projects` SQLite table (V001). Carries opaque string
//! ids and string timestamps so this crate stays free of `chrono` and
//! `uuid` deps; UI and storage do any parsing they need at their layer.

use serde::{Deserialize, Serialize};

// `Eq` is intentionally not derived: `sort_order: f64` is only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub default_branch: String,
    pub created_at: String,
    pub last_opened_at: Option<String>,
    /// Sparse manual display rank for the left-rail project list (lower =
    /// higher). Independent of `last_opened_at`: opening a project does not
    /// change its rank, so a hand-ordered list stays put. `#[serde(default)]`
    /// so snapshots written before this field still deserialize (defaults to
    /// `0.0`, treated as not-yet-ranked by the backfill).
    #[serde(default)]
    pub sort_order: f64,
}
