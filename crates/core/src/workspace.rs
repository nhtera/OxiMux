//! Workspace domain type — one git worktree per task within a project.
//!
//! Persisted in the `workspaces` SQLite table (V001). `UNIQUE(project_id,
//! slug)` is enforced by the schema, not this type.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub slug: String,
    pub branch: String,
    pub worktree_path: String,
    /// `"active"` or `"archived"`. String column for forward extensibility
    /// without a schema migration.
    pub status: String,
    pub created_at: String,
    pub archived_at: Option<String>,
    /// GitHub issue/PR reference this workspace was created from (e.g. `"#42"`),
    /// or `None` for a manually-created workspace. `#[serde(default)]` so
    /// snapshots written before this field still deserialize.
    #[serde(default)]
    pub linked_issue: Option<String>,
    /// Optional identifier hue — a tab-color swatch slug (e.g. `"blue"`), or
    /// `None` for the default (pure charcoal). `#[serde(default)]` for
    /// back-compat with pre-field snapshots.
    #[serde(default)]
    pub tint: Option<String>,
}

/// Per-worktree SCM scratch state, persisted in the V006
/// `worktree_settings` SQLite table keyed by `workspace_id`.
///
/// Every field is optional — `None` means "fall back to the global
/// default" (e.g. the SCM panel's `view_mode` setting, the repo's
/// default branch as the diff base, an empty composer textarea). The
/// row only exists once at least one field has been set; readers MUST
/// treat a missing row as `WorktreeSettings::default()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSettings {
    /// Base ref the diff view / dropdown / graph compare against. `None`
    /// = the repo's default branch as resolved by `git symbolic-ref`.
    pub base_ref: Option<String>,
    /// Inline-composer textarea contents persisted across panel re-mounts
    /// and app restarts. Cleared by the commit completion hook (Phase 07).
    pub commit_draft: Option<String>,
    /// Per-worktree override of the global view-mode default (`"flat"` /
    /// `"tree"`). `None` = inherit the global default. Round-tripped
    /// through [`ViewMode::as_str`] / [`ViewMode::from_str`].
    pub view_mode_override: Option<String>,
}

/// Source-control panel render mode.
///
/// `Flat` lists every changed file in section-by-status flow. `Tree`
/// groups files under their directory ancestors with collapsible folder
/// nodes. Default = `Flat`.
///
/// Round-trips through the `worktree_settings.view_mode_override` TEXT
/// column as `"flat"` / `"tree"`. Unknown / null strings parse back to
/// `Flat` via [`ViewMode::from_str`] so a malformed row can never panic
/// the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Flat,
    Tree,
}

impl ViewMode {
    /// Stable string form for storage round-trip. Stays lowercase so it
    /// matches the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            ViewMode::Flat => "flat",
            ViewMode::Tree => "tree",
        }
    }

    /// Inverse of [`ViewMode::as_str`]. Unknown strings (including the
    /// empty string and legacy / pre-enum string values) decode to
    /// `Flat`.
    ///
    /// Intentionally inherent (not a `FromStr` impl) so storage callers
    /// don't have to thread a `Result` through every load — the
    /// "garbage → Flat" fallback is the correct contract for a UI
    /// preference round-tripped through a free-form TEXT column.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "tree" => ViewMode::Tree,
            _ => ViewMode::Flat,
        }
    }

    /// Cycle to the other mode. Used by the toolbar toggle.
    pub fn toggled(self) -> Self {
        match self {
            ViewMode::Flat => ViewMode::Tree,
            ViewMode::Tree => ViewMode::Flat,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_mode_as_str_round_trips_via_from_str() {
        for &mode in &[ViewMode::Flat, ViewMode::Tree] {
            assert_eq!(ViewMode::from_str(mode.as_str()), mode);
        }
    }

    #[test]
    fn view_mode_from_str_defaults_to_flat_on_garbage() {
        assert_eq!(ViewMode::from_str(""), ViewMode::Flat);
        assert_eq!(ViewMode::from_str("list"), ViewMode::Flat);
        assert_eq!(ViewMode::from_str("TREE"), ViewMode::Flat);
        assert_eq!(ViewMode::from_str("flatten"), ViewMode::Flat);
    }

    #[test]
    fn view_mode_toggled_flips_both_directions() {
        assert_eq!(ViewMode::Flat.toggled(), ViewMode::Tree);
        assert_eq!(ViewMode::Tree.toggled(), ViewMode::Flat);
    }

    #[test]
    fn view_mode_default_is_flat() {
        assert_eq!(ViewMode::default(), ViewMode::Flat);
    }

    #[test]
    fn view_mode_as_str_matches_storage_lowercase_form() {
        // Storage round-trips via these strings (TEXT column
        // `worktree_settings.view_mode_override`). They MUST stay
        // lowercase to match the serde `rename_all = "lowercase"`
        // representation in case a future caller serde-encodes a row.
        assert_eq!(ViewMode::Flat.as_str(), "flat");
        assert_eq!(ViewMode::Tree.as_str(), "tree");
    }
}
