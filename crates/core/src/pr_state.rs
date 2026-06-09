//! Pull-request lifecycle state for the current branch, as reported by the
//! forge CLI (`gh pr view --json state`).
//!
//! Plain data (no IO) so both the git layer that produces it and the UI
//! layers that consume it can hold it without a dependency cycle — mirrors
//! the placement of [`crate::git_ops::GitOperation`].

use serde::{Deserialize, Serialize};

/// The current branch's PR state. `None` covers both "no PR exists" and
/// "can't tell" (forge CLI absent, parse failure) — callers treat the
/// absence of a known state as "no PR", keeping PR controls usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PrState {
    /// No PR for this branch, or the forge couldn't be queried.
    #[default]
    None,
    /// An open PR exists — Create-PR is suppressed, Merge-PR is offered.
    Open,
    /// The PR was merged. Create-PR must stay suppressed so the user can't
    /// open a duplicate PR for already-merged work.
    Merged,
    /// The PR was closed without merging. Treated like `None` for Create-PR
    /// (a new PR is legitimately allowed) but distinguished for clarity.
    Closed,
}

impl PrState {
    /// Parse the `state` value from `gh pr view --json state`
    /// (`OPEN` / `MERGED` / `CLOSED`). Unknown / empty → `None`.
    pub fn from_gh_state(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::None,
        }
    }

    /// True only for an open PR — the existing `has_open_pr` semantics.
    pub fn is_open(self) -> bool {
        matches!(self, PrState::Open)
    }

    /// True only for a merged PR.
    pub fn is_merged(self) -> bool {
        matches!(self, PrState::Merged)
    }
}

#[cfg(test)]
mod tests {
    use super::PrState;

    #[test]
    fn parses_known_states_case_insensitively() {
        assert_eq!(PrState::from_gh_state("OPEN"), PrState::Open);
        assert_eq!(PrState::from_gh_state("merged"), PrState::Merged);
        assert_eq!(PrState::from_gh_state(" Closed "), PrState::Closed);
    }

    #[test]
    fn unknown_or_empty_is_none() {
        assert_eq!(PrState::from_gh_state(""), PrState::None);
        assert_eq!(PrState::from_gh_state("DRAFT"), PrState::None);
    }

    #[test]
    fn open_and_merged_predicates() {
        assert!(PrState::Open.is_open());
        assert!(!PrState::Open.is_merged());
        assert!(PrState::Merged.is_merged());
        assert!(!PrState::Merged.is_open());
        assert!(!PrState::None.is_open());
    }
}
