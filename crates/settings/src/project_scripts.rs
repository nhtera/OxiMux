//! Per-project lifecycle scripts (setup / run / cleanup) loaded from TOML.
//!
//! Defined in a per-project `.oximux/scripts.toml` (git-committable so a
//! team shares them). Each entry is an optional shell snippet run against a
//! worktree via the user's shell. `auto_setup` opts into running `setup`
//! automatically right after a worktree is created (default off).
//!
//! Do not store secrets here — `.oximux/scripts.toml` is intended to be
//! committed to git, the same trust boundary as `commands.toml`. Parsing is
//! tolerant: unknown keys are ignored for forward-compat; a malformed file
//! surfaces an error the caller logs and falls back to the empty default.

use serde::{Deserialize, Serialize};

/// File name for the per-project scripts file (inside `.oximux/`).
pub const FILE_NAME: &str = "scripts.toml";

/// The three lifecycle phases a project can define a script for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    Setup,
    Run,
    Cleanup,
}

impl ScriptKind {
    /// Lowercase identifier used in tab titles + tracing fields.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Run => "run",
            Self::Cleanup => "cleanup",
        }
    }
}

/// Lifecycle scripts for one project. An absent file deserializes to the
/// all-`None` default (no buttons surface, no errors).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectScripts {
    pub setup: Option<String>,
    pub run: Option<String>,
    pub cleanup: Option<String>,
    /// Opt-in: run `setup` automatically after a worktree is created.
    pub auto_setup: bool,
}

impl ProjectScripts {
    /// Parse a TOML document. Unknown keys are tolerated; malformed input
    /// returns an error for the caller to log and skip.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a TOML document (used to seed a default file).
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// The script string for a given kind, trimmed, only when defined and
    /// non-empty. Whitespace-only entries are treated as undefined so a
    /// stray `run = ""` doesn't surface a button that runs nothing.
    pub fn script(&self, kind: ScriptKind) -> Option<&str> {
        let raw = match kind {
            ScriptKind::Setup => self.setup.as_deref(),
            ScriptKind::Run => self.run.as_deref(),
            ScriptKind::Cleanup => self.cleanup.as_deref(),
        };
        raw.map(str::trim).filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_fields_default_to_none() {
        let s = ProjectScripts::from_toml_str("").expect("empty is valid TOML");
        assert_eq!(s, ProjectScripts::default());
        assert!(!s.auto_setup);
        assert_eq!(s.script(ScriptKind::Setup), None);
    }

    #[test]
    fn parses_all_fields() {
        let toml = r#"
auto_setup = true
setup = "pnpm install"
run = "pnpm dev"
cleanup = "docker compose down"
"#;
        let s = ProjectScripts::from_toml_str(toml).expect("parse");
        assert!(s.auto_setup);
        assert_eq!(s.script(ScriptKind::Setup), Some("pnpm install"));
        assert_eq!(s.script(ScriptKind::Run), Some("pnpm dev"));
        assert_eq!(s.script(ScriptKind::Cleanup), Some("docker compose down"));
    }

    #[test]
    fn whitespace_only_script_is_treated_as_undefined() {
        let s = ProjectScripts {
            run: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(s.script(ScriptKind::Run), None);
    }

    #[test]
    fn script_is_trimmed() {
        let s = ProjectScripts {
            setup: Some("  make build  ".to_string()),
            ..Default::default()
        };
        assert_eq!(s.script(ScriptKind::Setup), Some("make build"));
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let toml = r#"
setup = "echo hi"
future_field = "ignored"
"#;
        let s = ProjectScripts::from_toml_str(toml).expect("unknown keys ignored");
        assert_eq!(s.script(ScriptKind::Setup), Some("echo hi"));
    }

    #[test]
    fn malformed_returns_error() {
        let result = ProjectScripts::from_toml_str("auto_setup = [[[broken");
        assert!(result.is_err());
    }

    #[test]
    fn round_trip_serialization() {
        let original = ProjectScripts {
            setup: Some("a".into()),
            run: Some("b".into()),
            cleanup: None,
            auto_setup: true,
        };
        let toml = original.to_toml_string();
        let parsed = ProjectScripts::from_toml_str(&toml).expect("round-trip");
        assert_eq!(original, parsed);
    }

    #[test]
    fn kind_labels_are_lowercase() {
        assert_eq!(ScriptKind::Setup.as_str(), "setup");
        assert_eq!(ScriptKind::Run.as_str(), "run");
        assert_eq!(ScriptKind::Cleanup.as_str(), "cleanup");
    }
}
