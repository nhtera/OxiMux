//! User-tunable AI commit-message generation settings, loaded from
//! `commit_message_ai.toml` in the app data dir and held as a GPUI
//! [`Global`] so the commit composer reads one source of truth.
//!
//! Defaults to heuristic mode — same behaviour a fresh install
//! shipped before agent mode existed, so an absent or empty file is
//! a no-op visual diff.
//!
//! Live-reload: a file watcher (in the app crate) reparses on change
//! and swaps the global. The commit composer reads the mode at click
//! time, so a mid-session edit takes effect on the next sparkles
//! click without restarting OxiMux.

#[cfg(feature = "gpui")]
use gpui::Global;
use serde::{Deserialize, Serialize};

/// Top-level mode discriminator. Drives the sparkles button gate +
/// dispatch path.
///
/// `lowercase` rename so the TOML reads `mode = "heuristic"` rather
/// than the variant's case-sensitive form. Unknown values fail the
/// parse — better to fail loudly than silently fall back to a
/// different mode than the user expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CommitMessageAiMode {
    /// Sparkles button hidden. Use when AI generation isn't wanted.
    Off,
    /// Pure local heuristic. Default for fresh installs — works
    /// offline, no agent CLI required.
    #[default]
    Heuristic,
    /// Spawn a configured agent CLI (claude / codex / custom).
    /// `agent` block in the TOML supplies which one + which model.
    Agent,
}

/// One concrete agent configuration. Loaded under `[agent]` in the
/// TOML when [`CommitMessageAiSettings::mode`] is [`CommitMessageAiMode::Agent`];
/// ignored otherwise.
///
/// `agent_id` picks which CLI: `"claude"`, `"codex"`, or `"custom"`.
/// For `"custom"`, `custom_command` MUST be set (a shell-like
/// template; `{prompt}` substitutes the prompt into argv, absence
/// pipes via stdin).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentSettings {
    /// `"claude"` | `"codex"` | `"custom"`. Default `"claude"` —
    /// the most likely available CLI on a typical dev box.
    pub agent_id: String,
    /// Model id passed via the agent CLI's `--model` flag.
    /// Default `"sonnet"` (Claude's middle tier).
    pub model: String,
    /// Optional thinking-effort level. Use the empty string to
    /// omit. Supported values depend on the model (see
    /// `oximux_agents::commit_message::spec`).
    pub thinking_level: String,
    /// Optional user-supplied prompt suffix appended verbatim to
    /// the base prompt (style overrides like Conventional Commits,
    /// gitmoji, internal copy rules).
    pub custom_prompt: String,
    /// Required when `agent_id == "custom"`; ignored otherwise.
    /// Shell-like command template. `{prompt}` substitution selects
    /// argv delivery; absence selects stdin delivery.
    pub custom_command: String,
    /// Optional override for the built-in agent binary. Example:
    /// `"tsx /path/to/dev-claude"` to run a local dev build of
    /// claude through a runner. Ignored when `agent_id == "custom"`.
    pub agent_command_override: String,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            agent_id: "claude".to_string(),
            model: "sonnet".to_string(),
            thinking_level: String::new(),
            custom_prompt: String::new(),
            custom_command: String::new(),
            agent_command_override: String::new(),
        }
    }
}

/// All user-tunable AI commit-message knobs. `#[serde(default)]` so a
/// partial TOML override only sets the keys it cares about; unknown
/// keys are ignored for forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CommitMessageAiSettings {
    pub mode: CommitMessageAiMode,
    pub agent: AgentSettings,
}

impl Default for CommitMessageAiSettings {
    fn default() -> Self {
        Self {
            mode: CommitMessageAiMode::Heuristic,
            agent: AgentSettings::default(),
        }
    }
}

#[cfg(feature = "gpui")]
impl Global for CommitMessageAiSettings {}

impl CommitMessageAiSettings {
    /// File name within the app data dir.
    pub const FILE_NAME: &'static str = "commit_message_ai.toml";

    /// Parse a TOML document into settings; missing keys fall back
    /// to default.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a pretty TOML document, used to seed a default
    /// file so the user has every key in front of them to edit.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Normalise out-of-range / empty-string fields. Called after
    /// load so a hand-edited file with an invalid `mode = "wat"` or
    /// `agent_id = "claudeXX"` fails the parse instead of silently
    /// falling back to a mode the user didn't pick. Whitespace-only
    /// strings get coerced to empty strings for consistent
    /// `is_empty()` checks downstream.
    pub fn sanitized(mut self) -> Self {
        self.agent.agent_id = self.agent.agent_id.trim().to_string();
        self.agent.model = self.agent.model.trim().to_string();
        self.agent.thinking_level = self.agent.thinking_level.trim().to_string();
        self.agent.custom_prompt = self.agent.custom_prompt.trim().to_string();
        self.agent.custom_command = self.agent.custom_command.trim().to_string();
        self.agent.agent_command_override =
            self.agent.agent_command_override.trim().to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_heuristic() {
        let s = CommitMessageAiSettings::default();
        assert_eq!(s.mode, CommitMessageAiMode::Heuristic);
    }

    #[test]
    fn default_round_trips_through_toml() {
        let original = CommitMessageAiSettings::default();
        let toml = original.to_toml_string();
        let parsed = CommitMessageAiSettings::from_toml_str(&toml).expect("round-trip parse");
        assert_eq!(original, parsed);
    }

    #[test]
    fn partial_toml_overrides_only_named_keys() {
        let toml = r#"
mode = "agent"
[agent]
agent_id = "codex"
model = "gpt-5.5"
"#;
        let s = CommitMessageAiSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.mode, CommitMessageAiMode::Agent);
        assert_eq!(s.agent.agent_id, "codex");
        assert_eq!(s.agent.model, "gpt-5.5");
        // Untouched keys retain defaults.
        assert_eq!(s.agent.thinking_level, "");
        assert_eq!(s.agent.custom_command, "");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let toml = r#"
mode = "heuristic"
future_unknown_key = 42
[agent]
agent_id = "claude"
also_unknown = "x"
"#;
        let s = CommitMessageAiSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.mode, CommitMessageAiMode::Heuristic);
        assert_eq!(s.agent.agent_id, "claude");
    }

    #[test]
    fn invalid_mode_fails_to_parse() {
        let toml = r#"mode = "totally-invalid""#;
        let result = CommitMessageAiSettings::from_toml_str(toml);
        assert!(result.is_err(), "invalid mode value should fail loud");
    }

    #[test]
    fn sanitize_trims_whitespace_in_string_fields() {
        let s = CommitMessageAiSettings {
            mode: CommitMessageAiMode::Agent,
            agent: AgentSettings {
                agent_id: "  claude  ".into(),
                model: " sonnet ".into(),
                thinking_level: "   ".into(),
                custom_prompt: "  ".into(),
                custom_command: "".into(),
                agent_command_override: "".into(),
            },
        }
        .sanitized();
        assert_eq!(s.agent.agent_id, "claude");
        assert_eq!(s.agent.model, "sonnet");
        assert_eq!(s.agent.thinking_level, "");
        assert_eq!(s.agent.custom_prompt, "");
    }

    #[test]
    fn off_mode_round_trips() {
        let s = CommitMessageAiSettings {
            mode: CommitMessageAiMode::Off,
            ..Default::default()
        };
        let parsed =
            CommitMessageAiSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed.mode, CommitMessageAiMode::Off);
    }
}
