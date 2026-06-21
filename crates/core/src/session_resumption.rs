//! How an agent session relates to a prior one at spawn time.
//!
//! Each CLI expresses resume / fork differently (Claude Code uses
//! `--resume <id>` ± `--fork-session`; Codex uses `resume <id>` / `fork <id>`
//! as the first argv token), so a single `fork: bool` flag would be ambiguous.
//! This enum carries the prior session id and the intent; each adapter's
//! `build_command` maps it to its own CLI shape.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SessionResumption {
    /// A fresh session unrelated to any prior one.
    #[default]
    None,
    /// Continue the prior session `id` in place (same conversation).
    Resume { id: String },
    /// Branch a new session off the prior `id`, leaving the original intact.
    Fork { id: String },
}

impl SessionResumption {
    /// The prior session id this spawn derives from, if any. `None` for a
    /// fresh session.
    pub fn source_id(&self) -> Option<&str> {
        match self {
            SessionResumption::None => None,
            SessionResumption::Resume { id } | SessionResumption::Fork { id } => Some(id),
        }
    }

    /// True when a new session should branch off the source rather than
    /// continue it in place.
    pub fn is_fork(&self) -> bool {
        matches!(self, SessionResumption::Fork { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_none() {
        assert_eq!(SessionResumption::default(), SessionResumption::None);
        assert_eq!(SessionResumption::None.source_id(), None);
        assert!(!SessionResumption::None.is_fork());
    }

    #[test]
    fn source_id_and_fork_flag() {
        let resume = SessionResumption::Resume { id: "abc".into() };
        assert_eq!(resume.source_id(), Some("abc"));
        assert!(!resume.is_fork());

        let fork = SessionResumption::Fork { id: "xyz".into() };
        assert_eq!(fork.source_id(), Some("xyz"));
        assert!(fork.is_fork());
    }
}
