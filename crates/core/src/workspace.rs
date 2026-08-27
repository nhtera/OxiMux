//! Workspace domain type — one git worktree per task within a project.
//!
//! Persisted in the `workspaces` SQLite table (V001). `UNIQUE(project_id,
//! slug)` is enforced by the schema, not this type.

use serde::{Deserialize, Serialize};

// `Eq` is intentionally not derived: `sort_order: f64` is only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Sparse manual display rank within the workspace's project group (lower
    /// = higher). Only consulted in `Manual` sort mode; the primary row stays
    /// pinned first regardless. `#[serde(default)]` for back-compat with
    /// pre-field snapshots (defaults to `0.0`, treated as not-yet-ranked).
    #[serde(default)]
    pub sort_order: f64,
    /// Whether this workspace is pinned to the top of its project group. A
    /// pinned row floats above unpinned rows in *every* sort mode, with the
    /// active mode's ordering preserved within each group; the primary row
    /// stays first regardless. `#[serde(default)]` for back-compat with
    /// pre-field snapshots (defaults to `false`).
    #[serde(default)]
    pub pinned: bool,
    /// A one-line, agent-writable snapshot of what is happening here — the
    /// answer to "what is this worktree doing right now" without opening a
    /// transcript. Empty when unset.
    ///
    /// **A snapshot, not a log.** Last write wins; there is no history. The
    /// value is agent-authored prose, so treat it as untrusted display text.
    #[serde(default)]
    pub comment: String,
    /// The work phase, as the raw stored string — `""` when unset.
    ///
    /// Deliberately **not** a `WorkPhase`: a value written by a newer peer
    /// must survive a round trip through an older one rather than being
    /// dropped on read and erased on the next write. Parse with
    /// [`WorkPhase::parse`] at the point of display, which yields `None` for
    /// anything unrecognised.
    #[serde(default)]
    pub phase: String,
}

/// The closed vocabulary a worktree's [`Workspace::phase`] is written with.
///
/// Writers validate against this; readers do not. [`parse`](Self::parse)
/// returning `None` is the documented forward-compat posture — an unknown
/// value from a newer peer renders as *no phase*, never as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkPhase {
    Todo,
    InProgress,
    InReview,
    Done,
}

impl WorkPhase {
    /// Every phase, in the order work moves through them — the order a picker
    /// or a help string should list them in.
    pub const ALL: [WorkPhase; 4] =
        [WorkPhase::Todo, WorkPhase::InProgress, WorkPhase::InReview, WorkPhase::Done];

    /// The stored (and wire, and CLI) spelling. Kebab-case, matching the serde
    /// representation so a serde-encoded row and a hand-written CLI argument
    /// are the same string.
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkPhase::Todo => "todo",
            WorkPhase::InProgress => "in-progress",
            WorkPhase::InReview => "in-review",
            WorkPhase::Done => "done",
        }
    }

    /// Parse a stored or wire value. `None` for the empty string (unset) and
    /// for anything this build does not know — see the type docs.
    ///
    /// Case- and whitespace-insensitive: a phase typed at a shell prompt is
    /// the same phase whether or not the shift key was involved.
    pub fn parse(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|p| p.as_str() == norm)
    }

    /// A short human label for a card or a column.
    pub const fn label(self) -> &'static str {
        match self {
            WorkPhase::Todo => "To do",
            WorkPhase::InProgress => "In progress",
            WorkPhase::InReview => "In review",
            WorkPhase::Done => "Done",
        }
    }
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
    fn every_phase_round_trips_through_its_stored_spelling() {
        for phase in WorkPhase::ALL {
            assert_eq!(
                WorkPhase::parse(phase.as_str()),
                Some(phase),
                "`{}` must parse back to itself",
                phase.as_str()
            );
        }
    }

    #[test]
    fn an_unknown_phase_is_none_rather_than_an_error() {
        // The forward-compat contract: a phase a newer peer knows and this
        // build does not renders as *no phase*. If this ever becomes an error
        // or a default, an older desktop starts lying about newer worktrees.
        assert_eq!(WorkPhase::parse("shipped"), None);
        assert_eq!(WorkPhase::parse(""), None);
        assert_eq!(WorkPhase::parse("in progress"), None, "the separator is a hyphen");
    }

    #[test]
    fn phase_parsing_ignores_case_and_surrounding_space() {
        // These come off a shell prompt, where neither is meaningful.
        assert_eq!(WorkPhase::parse("  In-Progress "), Some(WorkPhase::InProgress));
        assert_eq!(WorkPhase::parse("DONE"), Some(WorkPhase::Done));
    }

    #[test]
    fn phase_wire_spelling_matches_its_serde_form() {
        // A row serialized by serde and a phase typed as a CLI argument must be
        // the same string, or a snapshot written by one path fails to parse on
        // the other.
        for phase in WorkPhase::ALL {
            let json = serde_json::to_string(&phase).expect("serialize");
            assert_eq!(json, format!("\"{}\"", phase.as_str()));
        }
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
