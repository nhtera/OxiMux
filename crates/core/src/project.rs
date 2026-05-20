//! Project domain type — one root directory the user has opened.
//!
//! Persisted in the `projects` SQLite table (V001). Carries opaque string
//! ids and string timestamps so this crate stays free of `chrono` and
//! `uuid` deps; UI and storage do any parsing they need at their layer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root_path: String,
    pub default_branch: String,
    pub created_at: String,
    pub last_opened_at: Option<String>,
}
