//! Per-agent launch defaults, loaded from `agent_launch.toml` in the app
//! data dir and held as a GPUI [`Global`] so the launch picker and the
//! agent runtime read one source of truth.
//!
//! These are the knobs the one-click launcher applies when the user picks
//! an agent: extra CLI arguments (e.g. a skip-permissions flag), a default
//! `--model`, and whether the agent is hidden from the picker. A separate
//! `default_agent` marks which adapter the picker surfaces first.
//!
//! Defaults are intentionally empty — a fresh install launches every agent
//! with no extra flags and no model override, exactly as before this file
//! existed. An absent or empty TOML is a no-op.
//!
//! Live-reload: a file watcher (in the app crate) reparses on change and
//! swaps the global, so an edit takes effect on the next launch without a
//! restart. Keys are agent slugs (`claude-code`, `codex`, `aider`) matching
//! [`oximux_core::AgentAdapter`]'s adapter id.

use std::collections::BTreeMap;

use gpui::Global;
use serde::{Deserialize, Serialize};

/// Which backend a Chat-mode launch of an adapter speaks to. `StreamJson` is
/// the native subprocess path (Claude's stream-json protocol) and the default,
/// so an existing TOML that names no transport keeps Claude's behavior. `Acp`
/// drives an external agent over the Agent Client Protocol, spawned from the
/// adapter's [`PerAgentLaunch::acp_command`]. Only consulted for Chat launches;
/// a Terminal launch ignores it. Serde `lowercase` → `"streamjson"` / `"acp"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    StreamJson,
    Acp,
}

/// One agent's launch defaults. All fields optional via `#[serde(default)]`
/// so a partial table only sets what it cares about.
///
/// `args` is a free-text CLI fragment (shell-split at launch, honouring
/// single/double quotes) appended after the binary's own model/effort flags
/// and before any positional prompt. The settings UI flips a known
/// skip-permissions flag in and out of this string; power users can hand-edit
/// the TOML for arbitrary flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerAgentLaunch {
    /// Extra CLI arguments appended at launch. Empty = none.
    pub args: String,
    /// Default `--model` selector. Empty = let the CLI choose.
    pub model: String,
    /// Hide this agent from the launch picker. Detection still runs.
    pub disabled: bool,
    /// Which backend a Chat-mode launch uses. Default `StreamJson` (Claude's
    /// native path); set `Acp` to open this adapter as a chat over the Agent
    /// Client Protocol. Ignored for Terminal launches.
    pub transport: Transport,
    /// The command that speaks ACP (e.g. `gemini`), spawned when
    /// `transport == Acp`. Empty = no ACP backend configured, so the adapter is
    /// not chat-capable over ACP. Read only when `transport == Acp`.
    pub acp_command: String,
    /// Free-text argv fragment appended after `acp_command` (shell-split like
    /// `args`) — typically the protocol flag, e.g. `--acp`. Read only when
    /// `transport == Acp`.
    pub acp_args: String,
}

/// How a new agent launch opens by default. `Terminal` = the classic raw-PTY
/// agent; `Chat` opens the adapter as a structured chat thread. Chat is offered
/// only for chat-capable adapters (see [`AgentLaunchSettings::chat_capable`]);
/// every other adapter always opens as a terminal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenMode {
    #[default]
    Terminal,
    Chat,
}

/// The skip-permissions ("YOLO") flag seeded for each built-in agent on a
/// fresh install, so a one-click launch starts the agent in full-autonomy
/// mode out of the box (matching the reference cockpit's default). The user
/// can clear these in the settings pane afterward; the migration runs once.
pub const DEFAULT_AGENT_ARGS: &[(&str, &str)] = &[
    ("claude-code", "--dangerously-skip-permissions"),
    ("codex", "--dangerously-bypass-approvals-and-sandbox"),
    ("aider", "--yes-always"),
];

/// All per-agent launch settings plus the picker's default agent.
/// `BTreeMap` (not `HashMap`) so TOML serialization is key-sorted and
/// round-trips deterministically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLaunchSettings {
    /// Adapter id surfaced first in the picker (with a "Default" badge).
    /// Empty = no default; the picker keeps its registration order.
    pub default_agent: String,
    /// One-shot guard for the first-run skip-permissions seed. Once `true`,
    /// `seed_yolo_defaults` is a no-op, so a user who clears a default flag
    /// is never re-seeded on the next launch.
    pub yolo_defaults_migrated: bool,
    /// Enable OSC-9999 status hooks for Claude Code launches: inject the
    /// `--settings` hooks block so each agent reports the user's prompt, the
    /// tool it is running, and its lifecycle — surfaced as the rail agent
    /// title + status. **On by default**: this is the whole point of the
    /// agent cockpit (the reference cockpit keeps its status hooks always on);
    /// without it the rail can only show a generic "Ready" per agent. A user
    /// can disable it in Settings → Agents. The env var `OXIMUX_STATUS_HOOKS=1`
    /// force-enables regardless of this flag (a debug escape hatch). A missing
    /// key in an existing `agent_launch.toml` picks up this `true` default via
    /// the container-level `#[serde(default)]`, so the feature lights up on
    /// upgrade without a manual toggle.
    pub status_hooks_enabled: bool,
    /// Default surface a new agent launch opens as (`Terminal` or `Chat`).
    /// `Chat` reroutes a Claude launch to the structured chat view; every other
    /// adapter is unaffected. Serde-default `Terminal`, so existing configs and
    /// the classic new-agent flow are unchanged on upgrade.
    pub default_open_mode: OpenMode,
    /// Per-agent overrides keyed by adapter id.
    pub agents: BTreeMap<String, PerAgentLaunch>,
}

impl Default for AgentLaunchSettings {
    fn default() -> Self {
        Self {
            default_agent: String::new(),
            yolo_defaults_migrated: false,
            // On by default — see the field docs. An explicit `false` in the
            // TOML still wins (a present value overrides this default).
            status_hooks_enabled: true,
            default_open_mode: OpenMode::Terminal,
            agents: BTreeMap::new(),
        }
    }
}

impl Global for AgentLaunchSettings {}

impl AgentLaunchSettings {
    /// File name within the app data dir.
    pub const FILE_NAME: &'static str = "agent_launch.toml";

    /// Parse a TOML document; missing keys fall back to default.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a pretty TOML document, used to seed a default file.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Trim whitespace on all string fields so `is_empty()` checks
    /// downstream behave for a hand-edited file with stray spaces.
    pub fn sanitized(mut self) -> Self {
        self.default_agent = self.default_agent.trim().to_string();
        for v in self.agents.values_mut() {
            v.args = v.args.trim().to_string();
            v.model = v.model.trim().to_string();
            v.acp_command = v.acp_command.trim().to_string();
            v.acp_args = v.acp_args.trim().to_string();
        }
        self
    }

    /// The Chat-mode backend transport for `adapter_id`. Defaults to
    /// `StreamJson` (Claude's native path) when the adapter has no entry or
    /// names no transport.
    pub fn transport_for(&self, adapter_id: &str) -> Transport {
        self.for_agent(adapter_id).map(|a| a.transport).unwrap_or_default()
    }

    /// Whether `adapter_id` can open as a structured chat (vs a raw terminal).
    /// This is the capability seam the chat-routing gate consults instead of a
    /// hard-coded provider name. An adapter qualifies when it is either:
    /// - the built-in Claude adapter (`claude-code`), which chats over native
    ///   stream-json; or
    /// - configured with `transport = "acp"` and a non-empty `acp_command`
    ///   (an external ACP agent to spawn).
    pub fn chat_capable(&self, adapter_id: &str) -> bool {
        if adapter_id == "claude-code" {
            return true;
        }
        matches!(
            self.for_agent(adapter_id),
            Some(PerAgentLaunch { transport: Transport::Acp, acp_command, .. })
                if !acp_command.trim().is_empty()
        )
    }

    /// The launch entry for `adapter_id`, if the user has configured one.
    pub fn for_agent(&self, adapter_id: &str) -> Option<&PerAgentLaunch> {
        self.agents.get(adapter_id)
    }

    /// Mutable launch entry for `adapter_id`, inserting a default if absent.
    /// Used by the settings UI to flip toggles.
    pub fn entry_mut(&mut self, adapter_id: &str) -> &mut PerAgentLaunch {
        self.agents.entry(adapter_id.to_string()).or_default()
    }

    /// Extra CLI args for `adapter_id`, shell-split into argv tokens. Empty
    /// when the agent has no configured args.
    pub fn args_for(&self, adapter_id: &str) -> Vec<String> {
        self.for_agent(adapter_id)
            .map(|a| split_args(&a.args))
            .unwrap_or_default()
    }

    /// Default model for `adapter_id`, or `None` when unset/blank.
    pub fn model_for(&self, adapter_id: &str) -> Option<String> {
        self.for_agent(adapter_id)
            .map(|a| a.model.trim())
            .filter(|m| !m.is_empty())
            .map(str::to_string)
    }

    /// Whether `adapter_id` is hidden from the picker.
    pub fn is_disabled(&self, adapter_id: &str) -> bool {
        self.for_agent(adapter_id).map(|a| a.disabled).unwrap_or(false)
    }

    /// First-run back-fill of the skip-permissions defaults. Runs once: when
    /// `yolo_defaults_migrated` is false, seed every built-in agent that has
    /// no entry yet with its [`DEFAULT_AGENT_ARGS`] flag, then mark migrated.
    /// An agent the user already configured is left untouched. Returns `true`
    /// when something changed so the caller knows to persist the file.
    pub fn seed_yolo_defaults(&mut self) -> bool {
        if self.yolo_defaults_migrated {
            return false;
        }
        for (id, args) in DEFAULT_AGENT_ARGS {
            // Only seed agents the user hasn't touched (no entry yet) — an
            // existing entry, even an empty one, means a deliberate choice.
            self.agents.entry((*id).to_string()).or_insert_with(|| PerAgentLaunch {
                args: (*args).to_string(),
                ..Default::default()
            });
        }
        self.yolo_defaults_migrated = true;
        true
    }
}

/// Split a free-text CLI fragment into argv tokens. Whitespace separates
/// tokens; single and double quotes group a run (the quotes are stripped,
/// whitespace inside is preserved). Deliberately minimal — no backslash
/// escapes or env expansion — sufficient for the flags a launch config
/// carries. An unterminated quote still flushes its accumulated token so a
/// half-typed value never silently vanishes.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    in_token = true;
                } else if ch.is_whitespace() {
                    if in_token {
                        out.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                } else {
                    cur.push(ch);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty_and_round_trips() {
        let original = AgentLaunchSettings::default();
        assert!(original.default_agent.is_empty());
        assert!(original.agents.is_empty());
        let parsed =
            AgentLaunchSettings::from_toml_str(&original.to_toml_string()).expect("round-trip");
        assert_eq!(original, parsed);
    }

    #[test]
    fn partial_toml_sets_only_named_agents() {
        let toml = r#"
default_agent = "claude-code"
[agents.claude-code]
args = "--dangerously-skip-permissions"
model = "opus"
"#;
        let s = AgentLaunchSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.default_agent, "claude-code");
        assert_eq!(s.args_for("claude-code"), vec!["--dangerously-skip-permissions"]);
        assert_eq!(s.model_for("claude-code"), Some("opus".into()));
        assert!(!s.is_disabled("claude-code"));
        // Unmentioned agent has no overrides.
        assert!(s.for_agent("codex").is_none());
        assert!(s.args_for("codex").is_empty());
        assert_eq!(s.model_for("codex"), None);
    }

    #[test]
    fn status_hooks_flag_defaults_on_and_explicit_false_is_preserved() {
        // Absent key → ON (the cockpit's status sideband, the reference app's
        // always-on model). An existing toml without the key lights up on
        // upgrade via the container `#[serde(default)]`.
        let s = AgentLaunchSettings::from_toml_str("").expect("empty parses");
        assert!(s.status_hooks_enabled, "missing key defaults to on");
        // A toml that omits the key but sets other fields still defaults on.
        let partial = AgentLaunchSettings::from_toml_str(
            "default_agent = \"\"\nyolo_defaults_migrated = true\n",
        )
        .expect("partial parses");
        assert!(partial.status_hooks_enabled, "partial toml defaults on");
        // An explicit `false` wins over the default (a present value is kept).
        let off = AgentLaunchSettings::from_toml_str("status_hooks_enabled = false\n")
            .expect("explicit off parses");
        assert!(!off.status_hooks_enabled, "explicit false is preserved");
        // Round-trips through TOML.
        let on = AgentLaunchSettings {
            status_hooks_enabled: true,
            ..Default::default()
        };
        let parsed =
            AgentLaunchSettings::from_toml_str(&on.to_toml_string()).expect("round-trip");
        assert!(parsed.status_hooks_enabled);
    }

    #[test]
    fn default_open_mode_defaults_terminal_and_round_trips() {
        // Absent key → Terminal (the classic new-agent flow is unchanged).
        let s = AgentLaunchSettings::from_toml_str("").expect("empty parses");
        assert_eq!(s.default_open_mode, OpenMode::Terminal, "missing key → Terminal");
        // Explicit chat is parsed (lowercase per serde rename) and preserved.
        let chat = AgentLaunchSettings::from_toml_str("default_open_mode = \"chat\"\n")
            .expect("explicit chat parses");
        assert_eq!(chat.default_open_mode, OpenMode::Chat);
        // Round-trips through TOML.
        let parsed = AgentLaunchSettings::from_toml_str(&chat.to_toml_string()).expect("round-trip");
        assert_eq!(parsed.default_open_mode, OpenMode::Chat);
    }

    #[test]
    fn transport_defaults_stream_json_and_acp_round_trips() {
        // Absent key → StreamJson (Claude's native path; existing configs
        // unchanged on upgrade).
        let s = AgentLaunchSettings::from_toml_str("").expect("empty parses");
        assert_eq!(s.transport_for("claude-code"), Transport::StreamJson);
        // An adapter configured for ACP round-trips transport + command + args.
        let toml = r#"
[agents.gemini]
transport = "acp"
acp_command = "gemini"
acp_args = "--acp"
"#;
        let s = AgentLaunchSettings::from_toml_str(toml).expect("parse acp");
        assert_eq!(s.transport_for("gemini"), Transport::Acp);
        assert_eq!(s.for_agent("gemini").unwrap().acp_command, "gemini");
        assert_eq!(s.for_agent("gemini").unwrap().acp_args, "--acp");
        let parsed = AgentLaunchSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed, s);
    }

    #[test]
    fn chat_capable_claude_always_and_acp_when_configured() {
        let mut s = AgentLaunchSettings::default();
        // Built-in Claude chats over stream-json, no config needed.
        assert!(s.chat_capable("claude-code"));
        // A plain adapter with no ACP config is terminal-only.
        assert!(!s.chat_capable("codex"));
        // transport=acp but an empty command → NOT chat-capable (nothing to spawn).
        s.entry_mut("gemini").transport = Transport::Acp;
        assert!(!s.chat_capable("gemini"), "acp without a command is not chat-capable");
        // With a command → chat-capable.
        s.entry_mut("gemini").acp_command = "gemini".into();
        assert!(s.chat_capable("gemini"));
    }

    #[test]
    fn disabled_flag_round_trips() {
        let toml = r#"
[agents.aider]
disabled = true
"#;
        let s = AgentLaunchSettings::from_toml_str(toml).expect("parse");
        assert!(s.is_disabled("aider"));
    }

    #[test]
    fn blank_model_is_none() {
        let mut s = AgentLaunchSettings::default();
        s.entry_mut("codex").model = "   ".into();
        assert_eq!(s.model_for("codex"), None);
    }

    #[test]
    fn entry_mut_inserts_default() {
        let mut s = AgentLaunchSettings::default();
        s.entry_mut("codex").args = "--foo".into();
        assert_eq!(s.args_for("codex"), vec!["--foo"]);
    }

    #[test]
    fn sanitize_trims_fields() {
        let mut s = AgentLaunchSettings {
            default_agent: "  claude-code  ".into(),
            ..Default::default()
        };
        s.entry_mut("claude-code").args = "  --flag  ".into();
        s.entry_mut("claude-code").model = " opus ".into();
        let s = s.sanitized();
        assert_eq!(s.default_agent, "claude-code");
        assert_eq!(s.for_agent("claude-code").unwrap().args, "--flag");
        assert_eq!(s.for_agent("claude-code").unwrap().model, "opus");
    }

    #[test]
    fn split_args_handles_plain_and_quoted() {
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("--foo"), vec!["--foo"]);
        assert_eq!(split_args("  --a   --b "), vec!["--a", "--b"]);
        assert_eq!(
            split_args(r#"--msg "hello world" --x"#),
            vec!["--msg", "hello world", "--x"]
        );
        assert_eq!(split_args("--p 'a b'"), vec!["--p", "a b"]);
    }

    #[test]
    fn split_args_flushes_unterminated_quote() {
        assert_eq!(split_args(r#"--m "half"#), vec!["--m", "half"]);
    }

    #[test]
    fn split_args_empty_quotes_make_empty_token() {
        // An explicit empty quoted string is a real (empty) argv token.
        assert_eq!(split_args(r#"--flag """#), vec!["--flag", ""]);
    }

    #[test]
    fn seed_yolo_defaults_fills_builtins_once() {
        let mut s = AgentLaunchSettings::default();
        assert!(s.seed_yolo_defaults(), "first seed should change state");
        assert_eq!(
            s.args_for("claude-code"),
            vec!["--dangerously-skip-permissions"]
        );
        assert_eq!(
            s.args_for("codex"),
            vec!["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(s.args_for("aider"), vec!["--yes-always"]);
        assert!(s.yolo_defaults_migrated);
        // Idempotent: a second run is a no-op.
        assert!(!s.seed_yolo_defaults());
    }

    #[test]
    fn seed_yolo_defaults_respects_existing_entry() {
        // A user who already configured an agent (even cleared its flag) is
        // not re-seeded.
        let mut s = AgentLaunchSettings::default();
        s.entry_mut("claude-code").args = String::new(); // deliberately empty
        assert!(s.seed_yolo_defaults());
        assert!(
            s.args_for("claude-code").is_empty(),
            "existing (empty) entry must be preserved, not re-seeded"
        );
        // Untouched agents still get their default.
        assert_eq!(s.args_for("aider"), vec!["--yes-always"]);
    }

    #[test]
    fn seed_yolo_defaults_skipped_when_already_migrated() {
        let mut s = AgentLaunchSettings {
            yolo_defaults_migrated: true,
            ..Default::default()
        };
        assert!(!s.seed_yolo_defaults());
        assert!(s.for_agent("claude-code").is_none());
    }

    #[test]
    fn unknown_keys_ignored() {
        let toml = r#"
default_agent = "codex"
future_key = 1
[agents.codex]
args = "-m gpt-5.5"
also_unknown = true
"#;
        let s = AgentLaunchSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.default_agent, "codex");
        assert_eq!(s.args_for("codex"), vec!["-m", "gpt-5.5"]);
    }
}
