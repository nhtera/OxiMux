//! Per-project lifecycle scripts (setup / run / cleanup) loaded from TOML.
//!
//! Defined in a per-project `.oximux/scripts.toml` (git-committable so a
//! team shares them). Each entry is an optional shell snippet run against a
//! worktree via the user's shell. `auto_setup` opts into running `setup`
//! automatically as part of worktree *provisioning* (default off) — see
//! [`SetupDecision`] for how a one-off override composes with it.
//!
//! `default_tabs` names terminal tabs to open in a freshly provisioned
//! worktree, so a project can declare the shell layout a new branch starts in.
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
    /// Opt-in: run `setup` as part of worktree provisioning. Default off, so
    /// a project that never asked for it sees no behavior change.
    ///
    /// This is the *project* half of the decision — the committed team answer.
    /// A single create can override it either way via [`SetupDecision`].
    pub auto_setup: bool,
    /// Terminal tabs to open in a newly provisioned worktree, by title. Each
    /// entry opens a plain shell rooted at the worktree; an empty list (the
    /// default) opens nothing.
    #[serde(default)]
    pub default_tabs: Vec<String>,
}

/// Whether one worktree creation should run the project's `setup` script.
///
/// Three-way rather than a bool because the per-request answer and the
/// per-project answer are different facts: `auto_setup` is what the team
/// committed, and `Inherit` — the default — is a request declining to
/// second-guess it. `Run`/`Skip` are the one-off answers a create dialog gives
/// for a single throwaway or a single must-provision worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SetupDecision {
    /// Run setup for this creation regardless of `auto_setup`.
    Run,
    /// Skip setup for this creation regardless of `auto_setup`.
    Skip,
    /// Defer to the project's `auto_setup` flag.
    #[default]
    Inherit,
}

impl SetupDecision {
    /// Resolve the three-way override against the project's committed flag.
    pub fn resolve(self, auto_setup: bool) -> bool {
        match self {
            Self::Run => true,
            Self::Skip => false,
            Self::Inherit => auto_setup,
        }
    }
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

/// Load the lifecycle scripts for `project_root` (or a worktree of it — the
/// `.oximux/` dir is committed, so a worktree carries the same file). Reads
/// `<project_root>/.oximux/scripts.toml`. Never panics; returns the default
/// on a missing or malformed file.
///
/// Lives beside the type rather than in the desktop because the worktree
/// teardown path needs it too, and that path is shared with `oximux serve`.
pub fn load_for_project(project_root: &std::path::Path) -> ProjectScripts {
    let path = project_root.join(".oximux").join(FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => match ProjectScripts::from_toml_str(&text) {
            Ok(scripts) => scripts,
            Err(err) => {
                tracing::warn!(
                    ?path,
                    %err,
                    "scripts.toml parse failed; ignoring per-project lifecycle scripts"
                );
                ProjectScripts::default()
            }
        },
        Err(_) => ProjectScripts::default(), // absent file → silent default
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
    fn default_tabs_is_empty_when_absent() {
        let s = ProjectScripts::from_toml_str("setup = \"x\"").expect("parse");
        assert!(s.default_tabs.is_empty());
    }

    #[test]
    fn default_tabs_parses_a_list() {
        let s = ProjectScripts::from_toml_str("default_tabs = [\"server\", \"logs\"]")
            .expect("parse");
        assert_eq!(s.default_tabs, vec!["server".to_string(), "logs".to_string()]);
    }

    /// `Inherit` is the default so an unspecified request cannot silently
    /// override what the project committed.
    #[test]
    fn inherit_defers_to_the_project_flag() {
        assert!(SetupDecision::default() == SetupDecision::Inherit);
        assert!(SetupDecision::Inherit.resolve(true));
        assert!(!SetupDecision::Inherit.resolve(false));
    }

    #[test]
    fn an_explicit_decision_overrides_the_project_flag_both_ways() {
        assert!(SetupDecision::Run.resolve(false));
        assert!(!SetupDecision::Skip.resolve(true));
    }

    #[test]
    fn round_trip_serialization() {
        let original = ProjectScripts {
            setup: Some("a".into()),
            run: Some("b".into()),
            cleanup: None,
            auto_setup: true,
            default_tabs: vec!["server".into()],
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

    fn write_scripts(root: &std::path::Path, content: &str) {
        let dir = root.join(".oximux");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE_NAME), content).unwrap();
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load_for_project(tmp.path()), ProjectScripts::default());
    }

    #[test]
    fn valid_file_loads() {
        let tmp = tempfile::tempdir().unwrap();
        write_scripts(
            tmp.path(),
            "auto_setup = true\nsetup = \"pnpm i\"\nrun = \"pnpm dev\"\n",
        );
        let scripts = load_for_project(tmp.path());
        assert!(scripts.auto_setup);
        assert_eq!(scripts.script(ScriptKind::Setup), Some("pnpm i"));
        assert_eq!(scripts.script(ScriptKind::Run), Some("pnpm dev"));
        assert_eq!(scripts.script(ScriptKind::Cleanup), None);
    }

    #[test]
    fn malformed_file_returns_default_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        write_scripts(tmp.path(), "setup = [[[broken");
        assert_eq!(load_for_project(tmp.path()), ProjectScripts::default());
    }
}
