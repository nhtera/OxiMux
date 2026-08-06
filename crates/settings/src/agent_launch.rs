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

#[cfg(feature = "gpui")]
use gpui::Global;
use serde::{Deserialize, Serialize};

/// Which backend a Chat-mode launch of an adapter speaks to. `StreamJson` is
/// the native subprocess path (Claude's stream-json protocol) and the default,
/// so an existing TOML that names no transport keeps Claude's behavior.
/// `AppServer` drives Codex over its native `codex app-server` JSON-RPC. `Acp`
/// drives an external agent over the Agent Client Protocol, spawned from the
/// adapter's [`PerAgentLaunch::acp_command`]. Only consulted for Chat launches;
/// a Terminal launch ignores it. Serde `lowercase` → `"streamjson"` /
/// `"appserver"` / `"acp"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    StreamJson,
    AppServer,
    Acp,
    /// Pi's own newline-JSON protocol (`pi --mode rpc`). Pi speaks neither ACP
    /// nor app-server, so it needs its own transport rather than reusing one.
    Rpc,
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
    /// How a launch of THIS agent opens by default, overriding the global
    /// [`AgentLaunchSettings::default_open_mode`]. `None` = inherit the global.
    /// The ACP presets set this to `Chat` so Cursor/Amp open as a structured chat
    /// even when the global default is `Terminal`; any agent can set it too.
    pub default_open_mode: Option<OpenMode>,
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

/// A built-in ACP agent preset: a ready-to-launch chat agent that needs no
/// hand-written `[agents.<id>]` block. Resolution (`chat_capable`, backend
/// command/args, open mode) falls back to a matching preset when the user hasn't
/// configured that id — so the agent is one-click without any TOML, while a user
/// entry for the same id still wins (see the resolution accessors). Presets are
/// **data, not new adapters**: they drive the generic ACP chat path.
// No `PartialEq`/`Eq`: the `interactive_resume` fn-pointer field has no
// meaningful equality (addresses aren't stable across codegen units), and no
// caller compares presets — they're matched by `id`/`command` instead.
#[derive(Debug, Clone, Copy)]
pub struct AcpPreset {
    /// Stable id (used as the adapter id in resolution + the launcher row).
    pub id: &'static str,
    /// Human label for the launcher row.
    pub title: &'static str,
    /// The program that speaks ACP (also the `which`-detection target).
    pub command: &'static str,
    /// Space-separated argv fragment after `command` (e.g. `acp`).
    pub args: &'static str,
    /// How to resume THIS agent's session in an interactive terminal, so a chat
    /// can offer a companion "Terminal view" the way Claude/Codex do with
    /// `--resume`. `None` = no known interactive-resume TUI for this agent, so
    /// the toggle stays disabled. `Some(f)` = `f(session_id)` returns the FULL
    /// argv (replacing `args`) run as `command` + that argv. The session id is
    /// the same id the ACP `session/new` minted — safe to resume with because
    /// it IS the agent's own native session id (verified for opencode: an ACP
    /// `sessionId` round-trips through `opencode export <id>` and resumes via
    /// `opencode --session <id>`). Only populated for presets whose ACP binary
    /// and interactive-resume binary are the same program.
    pub interactive_resume: Option<fn(session_id: &str) -> Vec<String>>,
}

/// Interactive resume argv for an opencode session: `opencode --session <id>`.
/// The `<id>` is the `ses_…` id opencode's ACP `session/new` returns, which is
/// its native session id (verified resumable via `opencode export <id>`);
/// transcript replay is on by default, mirroring the Claude/Codex `--resume`
/// companion. opencode's on-disk store is append-only file-per-message, so a
/// live headless ACP connection and this interactive resume writing the same
/// session don't corrupt history (only the session-metadata blob is
/// last-writer-wins).
fn opencode_interactive_resume(session_id: &str) -> Vec<String> {
    vec!["--session".to_string(), session_id.to_string()]
}

/// The built-in ACP presets, surfaced one-click in the launcher.
///
/// - **Cursor** — `cursor-agent acp` is native ACP (confirmed against the live
///   ACP registry).
/// - **Amp** — Sourcegraph's ACP wrapper binary (`amp-acp`). The `which amp-acp`
///   detection greys it when absent; its exact invocation is pinned from research
///   and should be re-confirmed live.
/// - **OpenCode** — `opencode acp` is a built-in ACP server (verified live end
///   to end: handshake, streamed chunks, tool cards, usage, slash commands).
///   The `which opencode` detection greys it when absent. Its interactive
///   resume (`opencode --session <id>`) is wired for the chat's Terminal-view
///   companion — same binary, and the ACP session id IS an opencode session id.
///
/// `interactive_resume` is `Some` only where the ACP session id is a verified
/// resumable native id AND the resume TUI is the same binary as the ACP server.
/// Cursor/Amp stay `None`: amp's resume uses a *different* binary
/// (`amp threads continue`) than its ACP wrapper, and neither's ACP-id↔resume-id
/// equivalence is confirmed — an unconfirmed row must not offer a broken toggle.
pub const ACP_PRESETS: &[AcpPreset] = &[
    AcpPreset { id: "cursor", title: "Cursor", command: "cursor-agent", args: "acp", interactive_resume: None },
    AcpPreset { id: "amp", title: "Amp", command: "amp-acp", args: "", interactive_resume: None },
    AcpPreset {
        id: "opencode",
        title: "OpenCode",
        command: "opencode",
        args: "acp",
        interactive_resume: Some(opencode_interactive_resume),
    },
];

/// The preset for `id`, if one is built in.
pub fn acp_preset(id: &str) -> Option<&'static AcpPreset> {
    ACP_PRESETS.iter().find(|p| p.id == id)
}

/// The interactive terminal resume invocation for an import-provider session
/// surfaced in the history picker — `(program, argv)` spawned as a `Custom` PTY.
/// `handle` is the provider's native resume handle: the session id for OpenCode
/// (`opencode --session <id>`) and Copilot (`copilot --resume=<id>`). `None` for
/// an unrecognized id. Keeps every provider's resume argv in one place so the
/// index, picker, and spawn layer can't drift.
///
/// Pi (`pi --session <handle>`) accepts **either** a session id or a rollout file
/// path, and the two are not equivalent: an id is looked up in the project's
/// store and a miss exits 1 with a message, while a path is taken literally with
/// no existence check — a stale one makes pi create an empty session there and
/// start as though it had resumed. **Prefer the id.** The history picker passes a
/// path (it has one, and it exists — it was just read to build the row); the chat's
/// companion terminal passes the id.
pub fn import_resume_command(preset_id: &str, handle: &str) -> Option<(String, Vec<String>)> {
    match preset_id {
        "opencode" => Some(("opencode".to_string(), vec!["--session".to_string(), handle.to_string()])),
        "copilot" => Some(("copilot".to_string(), vec![format!("--resume={handle}")])),
        "pi" => Some(("pi".to_string(), vec!["--session".to_string(), handle.to_string()])),
        _ => None,
    }
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
    /// Auto-generate a short LLM tab title after the first message on a
    /// Claude/Codex chat (a one-shot `claude -p --model haiku` call, ~10s-capped,
    /// haiku-priced). **On by default**; a user can disable it in Settings →
    /// Agents to avoid the per-chat billed spawn. ACP chats are always skipped
    /// (their agents push a native title). A missing key picks up this `true`
    /// default via the container-level `#[serde(default)]`.
    pub auto_title_enabled: bool,
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
            // On by default — see the field docs. An explicit `false` still wins.
            auto_title_enabled: true,
            agents: BTreeMap::new(),
        }
    }
}

#[cfg(feature = "gpui")]
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

    /// The Chat-mode backend transport for `adapter_id`. An explicitly
    /// configured non-default transport on the entry wins (e.g. an ACP adapter);
    /// then a built-in ACP preset (Cursor/Amp → `Acp`); otherwise the built-in
    /// default per adapter applies — Codex speaks `app-server`, everything else
    /// stream-json.
    pub fn transport_for(&self, adapter_id: &str) -> Transport {
        if let Some(a) = self.for_agent(adapter_id)
            && a.transport != Transport::StreamJson
        {
            return a.transport;
        }
        // A preset resolves to ACP only when the user hasn't configured the id.
        if self.for_agent(adapter_id).is_none() && acp_preset(adapter_id).is_some() {
            return Transport::Acp;
        }
        match adapter_id {
            "codex" => Transport::AppServer,
            "pi" => Transport::Rpc,
            _ => Transport::StreamJson,
        }
    }

    /// Whether `adapter_id` can open as a structured chat (vs a raw terminal).
    /// This is the capability seam the chat-routing gate consults instead of a
    /// hard-coded provider name. An adapter qualifies when it is either:
    /// - a built-in chat adapter — Claude (`claude-code`, native stream-json),
    ///   Codex (`codex`, native app-server), or Pi (`pi`, native `--mode rpc`); or
    /// - configured with `transport = "acp"` and a non-empty `acp_command`
    ///   (an external ACP agent to spawn); or
    /// - a built-in ACP preset (Cursor/Amp) the user hasn't overridden.
    ///
    /// The built-in check must stay ahead of the `for_agent` branch below: that
    /// branch only recognises ACP, so a built-in that merely *has* a configured
    /// entry (a model, some args) would otherwise fall into it and report
    /// `false` — silently un-chat-able for exactly the users who customised it.
    pub fn chat_capable(&self, adapter_id: &str) -> bool {
        if matches!(adapter_id, "claude-code" | "codex" | "pi") {
            return true;
        }
        if let Some(a) = self.for_agent(adapter_id) {
            return matches!(a.transport, Transport::Acp) && !a.acp_command.trim().is_empty();
        }
        // No user entry → a matching preset makes it chat-capable.
        acp_preset(adapter_id).is_some()
    }

    /// The launch entry for `adapter_id`, if the user has configured one.
    pub fn for_agent(&self, adapter_id: &str) -> Option<&PerAgentLaunch> {
        self.agents.get(adapter_id)
    }

    /// The adapter a launch that names **no** agent should use: the configured
    /// [`default_agent`](Self::default_agent) when it is set and chat-capable, else
    /// the first chat-capable, non-disabled agent — built-in chat agents first,
    /// then any configured ACP one. `None` only when the host has no chat-capable
    /// agent at all.
    ///
    /// Exists so a remote quick-add (which names no agent, unlike the local picker
    /// where the user chooses one) can still start a session before the user has
    /// picked a default — rather than failing with a bare "could not start".
    pub fn default_chat_agent(&self) -> Option<String> {
        let configured = self.default_agent.trim();
        if !configured.is_empty() && self.chat_capable(configured) {
            return Some(configured.to_string());
        }
        const BUILTINS: [&str; 3] = ["claude-code", "codex", "pi"];
        BUILTINS
            .into_iter()
            .find(|id| self.chat_capable(id) && !self.is_disabled(id))
            .map(str::to_string)
            .or_else(|| {
                self.agents
                    .iter()
                    .find(|(id, a)| !a.disabled && self.chat_capable(id))
                    .map(|(id, _)| id.clone())
            })
    }

    /// Mutable launch entry for `adapter_id`, inserting a default if absent.
    /// Used by the settings UI to flip toggles.
    ///
    /// Minting an entry hides the ACP-preset fallback from every resolution
    /// accessor (`chat_capable`, `transport_for`, `acp_command_for`, …), so a
    /// fresh entry for a preset id is seeded with the preset's ACP wiring —
    /// otherwise the first UI toggle (skip-perms, model, enabled) would
    /// silently flip Cursor/Amp/OpenCode terminal-only. A hand-written TOML
    /// entry never passes through here, so writing a bare `[agents.cursor]`
    /// block still suppresses the preset deliberately.
    pub fn entry_mut(&mut self, adapter_id: &str) -> &mut PerAgentLaunch {
        self.agents.entry(adapter_id.to_string()).or_insert_with(|| {
            match acp_preset(adapter_id) {
                Some(p) => PerAgentLaunch {
                    transport: Transport::Acp,
                    acp_command: p.command.to_string(),
                    acp_args: p.args.to_string(),
                    ..PerAgentLaunch::default()
                },
                None => PerAgentLaunch::default(),
            }
        })
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

    /// The ACP command for `adapter_id` (trimmed; `None` when unset/blank).
    /// Only meaningful when the adapter's transport is [`Transport::Acp`] —
    /// it's the program the chat factory spawns to speak ACP. Falls back to a
    /// built-in preset's command when the user hasn't configured the id.
    pub fn acp_command_for(&self, adapter_id: &str) -> Option<String> {
        if let Some(a) = self.for_agent(adapter_id) {
            let cmd = a.acp_command.trim();
            return (!cmd.is_empty()).then(|| cmd.to_string());
        }
        acp_preset(adapter_id).map(|p| p.command.to_string())
    }

    /// The ACP args for `adapter_id`, shell-split into argv tokens (e.g.
    /// `--experimental-acp`). Empty when unset. Appended after `acp_command`.
    /// Falls back to a built-in preset's args when the user hasn't configured it.
    pub fn acp_args_for(&self, adapter_id: &str) -> Vec<String> {
        if let Some(a) = self.for_agent(adapter_id) {
            return split_args(&a.acp_args);
        }
        acp_preset(adapter_id).map(|p| split_args(p.args)).unwrap_or_default()
    }

    /// How a launch of `adapter_id` opens, resolving the precedence:
    /// per-agent override (from the user's TOML) → a preset's mode (Cursor/Amp =
    /// `Chat`) → the global [`Self::default_open_mode`]. This is what the routing
    /// gate consults so a chat-default agent can coexist with a terminal-default
    /// global.
    pub fn open_mode_for(&self, adapter_id: &str) -> OpenMode {
        if let Some(m) = self.for_agent(adapter_id).and_then(|a| a.default_open_mode) {
            return m;
        }
        // A preset opens as chat unless the user overrode the id above.
        if self.for_agent(adapter_id).is_none() && acp_preset(adapter_id).is_some() {
            return OpenMode::Chat;
        }
        self.default_open_mode
    }

    /// The routing gate shared by the new-agent launcher and session import:
    /// a launch of `adapter_id` opens as a structured chat when its resolved
    /// open mode is `Chat` AND the adapter is chat-capable. Every terminal-only
    /// adapter (and every Terminal-mode agent) takes the classic terminal path.
    /// Single source of truth so the two call sites can never drift apart.
    pub fn opens_as_chat(&self, adapter_id: &str) -> bool {
        self.open_mode_for(adapter_id) == OpenMode::Chat && self.chat_capable(adapter_id)
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
    fn acp_command_and_args_accessors() {
        let toml = r#"
[agents.gemini]
transport = "acp"
acp_command = "  gemini  "
acp_args = "--experimental-acp --foo"
"#;
        let s = AgentLaunchSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.acp_command_for("gemini").as_deref(), Some("gemini"));
        assert_eq!(s.acp_args_for("gemini"), vec!["--experimental-acp", "--foo"]);
        // An unconfigured adapter → no command, no args.
        assert_eq!(s.acp_command_for("aider"), None);
        assert!(s.acp_args_for("aider").is_empty());
        // A blank command is treated as unset.
        let blank =
            AgentLaunchSettings::from_toml_str("[agents.x]\nacp_command = \"   \"\n").expect("parse");
        assert_eq!(blank.acp_command_for("x"), None);
    }

    #[test]
    fn chat_capable_builtins_and_acp_when_configured() {
        let mut s = AgentLaunchSettings::default();
        // Built-in chat adapters, no config needed.
        assert!(s.chat_capable("claude-code"));
        assert_eq!(s.transport_for("claude-code"), Transport::StreamJson);
        assert!(s.chat_capable("codex"));
        assert_eq!(s.transport_for("codex"), Transport::AppServer, "codex speaks app-server by default");
        // A plain non-built-in adapter with no ACP config is terminal-only.
        assert!(!s.chat_capable("aider"));
        // transport=acp but an empty command → NOT chat-capable (nothing to spawn).
        s.entry_mut("gemini").transport = Transport::Acp;
        assert!(!s.chat_capable("gemini"), "acp without a command is not chat-capable");
        // With a command → chat-capable, and the explicit transport wins.
        s.entry_mut("gemini").acp_command = "gemini".into();
        assert!(s.chat_capable("gemini"));
        assert_eq!(s.transport_for("gemini"), Transport::Acp);
    }

    #[test]
    fn default_chat_agent_falls_back_when_no_default_is_set() {
        // Empty default (the fresh-install state): a remote quick-add still
        // resolves to a built-in chat agent rather than failing.
        let s = AgentLaunchSettings::default();
        assert!(s.default_agent.is_empty());
        assert_eq!(s.default_chat_agent().as_deref(), Some("claude-code"));
    }

    #[test]
    fn default_chat_agent_prefers_the_configured_default() {
        let s = AgentLaunchSettings {
            default_agent: "codex".into(),
            ..Default::default()
        };
        assert_eq!(s.default_chat_agent().as_deref(), Some("codex"));
    }

    #[test]
    fn default_chat_agent_skips_a_disabled_builtin() {
        // claude-code hidden from the picker → fall through to the next chat agent.
        let mut s = AgentLaunchSettings::default();
        s.entry_mut("claude-code").disabled = true;
        assert_eq!(s.default_chat_agent().as_deref(), Some("codex"));
    }

    #[test]
    fn pi_is_a_builtin_rpc_chat_adapter() {
        let s = AgentLaunchSettings::default();
        assert!(s.chat_capable("pi"), "pi opens as chat with zero config");
        assert_eq!(
            s.transport_for("pi"),
            Transport::Rpc,
            "pi speaks its own newline-JSON rpc protocol, not stream-json"
        );
    }

    #[test]
    fn pi_stays_chat_capable_once_configured() {
        // Configuring any field creates an entry. The ACP-only `for_agent`
        // branch must not claim a built-in and report it terminal-only.
        let mut s = AgentLaunchSettings::default();
        s.entry_mut("pi").model = "gpt-5.5".into();
        assert!(s.chat_capable("pi"), "a configured model must not disable pi chat");
        assert_eq!(s.transport_for("pi"), Transport::Rpc);
    }

    #[test]
    fn an_unknown_adapter_never_silently_becomes_pi() {
        // The `_` arm defaults to Claude's transport. That is the documented
        // fallback and the reason a new adapter needs an explicit arm here.
        let s = AgentLaunchSettings::default();
        assert_eq!(s.transport_for("totally-unknown"), Transport::StreamJson);
        assert!(!s.chat_capable("totally-unknown"));
    }

    #[test]
    fn acp_presets_resolve_without_config() {
        // Cursor/Amp are one-click with zero TOML: chat-capable, ACP transport,
        // command/args from the preset, and they open as chat.
        let s = AgentLaunchSettings::default();
        assert!(s.chat_capable("cursor"));
        assert_eq!(s.transport_for("cursor"), Transport::Acp);
        assert_eq!(s.acp_command_for("cursor").as_deref(), Some("cursor-agent"));
        assert_eq!(s.acp_args_for("cursor"), vec!["acp"]);
        assert_eq!(s.open_mode_for("cursor"), OpenMode::Chat);
        // Amp: its wrapper binary, no args.
        assert_eq!(s.acp_command_for("amp").as_deref(), Some("amp-acp"));
        assert!(s.acp_args_for("amp").is_empty());
        assert_eq!(s.open_mode_for("amp"), OpenMode::Chat);
        // OpenCode: `opencode acp`, one-click chat like the others.
        assert!(s.chat_capable("opencode"));
        assert_eq!(s.transport_for("opencode"), Transport::Acp);
        assert_eq!(s.acp_command_for("opencode").as_deref(), Some("opencode"));
        assert_eq!(s.acp_args_for("opencode"), vec!["acp"]);
        assert_eq!(s.open_mode_for("opencode"), OpenMode::Chat);
        // A non-preset, non-builtin adapter is still terminal-only.
        assert!(!s.chat_capable("aider"));
        assert_eq!(s.open_mode_for("aider"), OpenMode::Terminal);
    }

    #[test]
    fn only_opencode_has_interactive_resume_wired() {
        // opencode is the only preset with a confirmed interactive-resume TUI on
        // the same binary; its argv is `--session <id>` (replacing `acp`).
        let oc = acp_preset("opencode").expect("opencode preset");
        let f = oc.interactive_resume.expect("opencode has interactive resume");
        assert_eq!(f("ses_0aea7d2e3ffeBk"), vec!["--session", "ses_0aea7d2e3ffeBk"]);
        // Cursor/Amp are not confirmed → no toggle offered (see field docs).
        assert!(acp_preset("cursor").unwrap().interactive_resume.is_none());
        assert!(acp_preset("amp").unwrap().interactive_resume.is_none());
    }

    #[test]
    fn user_config_wins_over_preset() {
        // A hand-written [agents.cursor] block overrides the preset entirely.
        let toml = r#"
[agents.cursor]
transport = "acp"
acp_command = "my-cursor"
acp_args = "--acp"
"#;
        let s = AgentLaunchSettings::from_toml_str(toml).expect("parse");
        assert_eq!(s.acp_command_for("cursor").as_deref(), Some("my-cursor"));
        assert_eq!(s.acp_args_for("cursor"), vec!["--acp"]);
        assert!(s.chat_capable("cursor"));
        // The user didn't set a per-agent open mode, and a configured entry
        // suppresses the preset's Chat default → the global applies.
        assert_eq!(s.open_mode_for("cursor"), OpenMode::Terminal);
    }

    #[test]
    fn non_acp_entry_on_a_preset_id_suppresses_the_preset() {
        // A bare `[agents.cursor] model = "..."` (no transport/command) takes the
        // id over: the preset fallback is suppressed AND the id is NOT chat-capable
        // (transport defaults to stream-json), so the launcher must not surface it
        // as a working preset — otherwise a click would misroute to Claude.
        let s = AgentLaunchSettings::from_toml_str("[agents.cursor]\nmodel = \"foo\"\n").expect("parse");
        assert!(!s.chat_capable("cursor"), "a non-ACP override is not chat-capable");
        assert_eq!(s.transport_for("cursor"), Transport::StreamJson);
        assert_eq!(s.acp_command_for("cursor"), None, "no ACP command resolves");
    }

    #[test]
    fn open_mode_precedence_per_agent_then_preset_then_global() {
        // Unset agent inherits the global.
        let mut s = AgentLaunchSettings { default_open_mode: OpenMode::Chat, ..Default::default() };
        assert_eq!(s.open_mode_for("aider"), OpenMode::Chat, "inherits global chat");
        // A per-agent override wins over the global.
        s.entry_mut("aider").default_open_mode = Some(OpenMode::Terminal);
        assert_eq!(s.open_mode_for("aider"), OpenMode::Terminal, "per-agent override wins");
        // Round-trips through TOML.
        let parsed = AgentLaunchSettings::from_toml_str(&s.to_toml_string()).expect("round-trip");
        assert_eq!(parsed.open_mode_for("aider"), OpenMode::Terminal);
    }

    #[test]
    fn opens_as_chat_requires_both_chat_mode_and_chat_capable() {
        // Built-in chat adapter (Claude) with global Chat mode → opens as chat.
        let s = AgentLaunchSettings { default_open_mode: OpenMode::Chat, ..Default::default() };
        assert!(s.opens_as_chat("claude-code"));
        // Same adapter under the default Terminal mode → terminal path.
        let s = AgentLaunchSettings::default();
        assert!(!s.opens_as_chat("claude-code"));
        // A non-chat-capable adapter never opens as chat, even in Chat mode.
        let s = AgentLaunchSettings { default_open_mode: OpenMode::Chat, ..Default::default() };
        assert!(!s.opens_as_chat("aider"));
        // An ACP preset (chat-capable, preset Chat default) opens as chat even
        // when the global default is Terminal.
        let s = AgentLaunchSettings::default();
        assert!(s.opens_as_chat("opencode"));
    }

    #[test]
    fn per_agent_open_mode_defaults_none_on_old_blobs() {
        // An existing entry without the key loads with None (inherit global).
        let s = AgentLaunchSettings::from_toml_str("[agents.aider]\nargs = \"--x\"\n").expect("parse");
        assert_eq!(s.for_agent("aider").unwrap().default_open_mode, None);
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
        // Non-preset ids mint the plain default: no phantom ACP wiring.
        let e = s.for_agent("codex").unwrap();
        assert_eq!(e.transport, Transport::StreamJson);
        assert!(e.acp_command.is_empty());
    }

    #[test]
    fn entry_mut_on_preset_id_seeds_preset_wiring() {
        // Any UI chip toggle mints its entry through entry_mut. A bare default
        // entry would suppress the preset fallback (see
        // non_acp_entry_on_a_preset_id_suppresses_the_preset) and silently
        // flip the agent terminal-only — so a fresh entry for a preset id must
        // carry the preset's ACP wiring through every resolution accessor.
        for p in ACP_PRESETS {
            let mut s = AgentLaunchSettings::default();
            s.entry_mut(p.id).disabled = true; // any toggle that mints an entry
            assert!(s.chat_capable(p.id), "{} must stay chat-capable", p.id);
            assert_eq!(s.transport_for(p.id), Transport::Acp, "{}", p.id);
            assert_eq!(s.acp_command_for(p.id).as_deref(), Some(p.command));
            assert_eq!(s.acp_args_for(p.id), split_args(p.args));
        }
        // Seeding happens only at mint: an existing user entry is never touched.
        let mut s = AgentLaunchSettings::default();
        s.agents.insert("cursor".into(), PerAgentLaunch::default());
        s.entry_mut("cursor").model = "m".into();
        assert!(s.for_agent("cursor").unwrap().acp_command.is_empty());
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

    #[test]
    fn auto_title_defaults_on_and_back_compat() {
        // Default is ON.
        assert!(AgentLaunchSettings::default().auto_title_enabled);
        // A config with no `auto_title_enabled` key (pre-feature) still loads with
        // the `true` default (container-level `#[serde(default)]`).
        let old = AgentLaunchSettings::from_toml_str("default_agent = \"claude\"").expect("parse");
        assert!(old.auto_title_enabled, "missing key → default on");
        // An explicit `false` wins.
        let off = AgentLaunchSettings::from_toml_str("auto_title_enabled = false").expect("parse");
        assert!(!off.auto_title_enabled);
    }
}
