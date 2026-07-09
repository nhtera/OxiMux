//! Connection-independent chat-agent roster for the unified "New Agent" composer.
//!
//! The unified composer lets the user pick a coding agent **and** a model before
//! any subprocess exists, so it cannot read the live `AgentConnection::models()`
//! (which only reports after a session is bound). This module assembles that
//! pre-bind vocabulary from the two static sources OxiMux already owns:
//!
//! - the detected adapter registry — built-in Claude/Codex, carrying their
//!   declared static model/effort lists (`RegistryEntry::models`/`::efforts`),
//! - the ACP presets (Cursor/Amp/OpenCode) from settings,
//!
//! filtered to chat-capable agents (terminal-only Aider and the Custom escape
//! hatch are dropped, matching the launcher's chat rows). Availability
//! (which-detection) and the live post-bind model list are layered on by the UI;
//! this module is a pure, connection-independent lookup.
//!
//! Placed in the app crate because `oximux-agents` (model lists) and
//! `oximux-settings` (transport + presets) are sibling crates that don't see
//! each other — the app is the one place both are visible, alongside the sibling
//! resolver `chat_backend_for`.

// A few roster fields (e.g. `efforts`) are carried for completeness and future
// phases but not yet read by the composer's pre-bind pickers (which offer agent
// + model only); allow the staged dead code rather than dropping fields that
// belong on the vocabulary.
#![allow(dead_code)]

use gpui::App;
use oximux_agents::{AdapterRegistry, RegistryEntry};
use oximux_settings::{AgentLaunchSettings, Transport, ACP_PRESETS};

/// One pickable coding agent in the unified composer, with its pre-connection
/// vocabulary. Empty `models`/`efforts` hide those composer rows until the live
/// connection reports its real vocabulary after binding (ACP presets have no
/// static list yet; Codex's static list is a stale approximation refreshed once
/// the `codex app-server` handshake returns its `model/list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatRosterEntry {
    /// Adapter id — the routing key into `chat_backend_for`/`AdapterSelection`.
    pub id: String,
    /// Human label for the agent dropdown.
    pub display: String,
    /// Which backend a first-message bind will speak.
    pub transport: Transport,
    /// Static pre-bind model selectors (`[]` = no model row until bound).
    pub models: Vec<String>,
    /// Static pre-bind reasoning-effort selectors (`[]` = no effort row).
    pub efforts: Vec<String>,
}

impl ChatRosterEntry {
    /// The default model to preselect: the first declared model, if any.
    pub fn default_model(&self) -> Option<&str> {
        self.models.first().map(String::as_str)
    }
}

/// Assemble the chat-agent roster from detected built-in adapters plus the ACP
/// presets, in stable order: built-ins first (registry order — Claude, Codex),
/// then presets (Cursor, Amp, OpenCode). Only chat-capable ids are kept, so a
/// user config that demotes a preset id below chat-capable (e.g. a bare
/// `[agents.cursor] model = "…"`) drops it here exactly as it drops from the
/// launcher — no misrouting to Claude.
///
/// `detected` is the async `AdapterRegistry::detect_available()` result; this
/// function itself is pure so it can be unit-tested without touching the FS.
pub(crate) fn chat_roster(
    detected: &[RegistryEntry],
    settings: &AgentLaunchSettings,
) -> Vec<ChatRosterEntry> {
    let mut roster: Vec<ChatRosterEntry> = Vec::new();

    // Built-in chat-capable adapters, carrying their declared static vocab.
    // `chat_capable` already excludes terminal-only Aider; `custom` is the
    // free-form escape hatch and never a chat provider.
    for entry in detected {
        if entry.adapter_id == "custom" || !settings.chat_capable(entry.adapter_id) {
            continue;
        }
        roster.push(ChatRosterEntry {
            id: entry.adapter_id.to_string(),
            display: entry.display_name.to_string(),
            transport: settings.transport_for(entry.adapter_id),
            models: entry.models.iter().map(|m| m.to_string()).collect(),
            efforts: entry.efforts.iter().map(|e| e.to_string()).collect(),
        });
    }

    // ACP presets (Cursor/Amp/OpenCode). Skip any a built-in already covered or
    // a user config demoted below chat-capable. No static model list yet — the
    // ACP session reports its models after binding.
    for preset in ACP_PRESETS {
        if roster.iter().any(|e| e.id == preset.id) || !settings.chat_capable(preset.id) {
            continue;
        }
        roster.push(ChatRosterEntry {
            id: preset.id.to_string(),
            display: preset.title.to_string(),
            transport: settings.transport_for(preset.id),
            models: Vec::new(),
            efforts: Vec::new(),
        });
    }

    roster
}

/// Assemble the chat roster synchronously from live app state: the built-in
/// adapters (Claude/Codex, carrying their static model vocab) plus the ACP
/// presets, filtered by the current [`AgentLaunchSettings`] global. Built
/// without async which-detection — the unbound composer needs the agent + model
/// choices immediately, and a missing binary surfaces at spawn, not here. Falls
/// back to default settings when the global isn't installed (tests / early boot).
pub(crate) fn chat_roster_from_cx(cx: &App) -> Vec<ChatRosterEntry> {
    let detected = AdapterRegistry::with_builtin_adapters().entries_without_detection();
    match cx.try_global::<AgentLaunchSettings>() {
        Some(settings) => chat_roster(&detected, settings),
        None => chat_roster(&detected, &AgentLaunchSettings::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::AgentAdapter;

    /// The built-in registry as `detect_available()` would report it (all
    /// available), so tests exercise `chat_roster` without touching the FS.
    fn builtin_entries() -> Vec<RegistryEntry> {
        vec![
            RegistryEntry {
                adapter_id: "claude-code",
                display_name: "Claude Code",
                adapter_enum: AgentAdapter::ClaudeCode,
                available: true,
                models: &["opus", "sonnet", "haiku"],
                efforts: &["high", "medium", "low"],
            },
            RegistryEntry {
                adapter_id: "codex",
                display_name: "Codex",
                adapter_enum: AgentAdapter::Codex,
                available: true,
                models: &["gpt-5-codex", "o3"],
                efforts: &[],
            },
            RegistryEntry {
                adapter_id: "aider",
                display_name: "Aider",
                adapter_enum: AgentAdapter::Aider,
                available: true,
                models: &[],
                efforts: &[],
            },
            RegistryEntry {
                adapter_id: "custom",
                display_name: "Custom",
                adapter_enum: AgentAdapter::Custom,
                available: true,
                models: &[],
                efforts: &[],
            },
        ]
    }

    #[test]
    fn roster_lists_chat_capable_builtins_then_presets() {
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&builtin_entries(), &s);
        let ids: Vec<&str> = roster.iter().map(|e| e.id.as_str()).collect();
        // Built-ins first (Claude, Codex), then the three ACP presets. Aider
        // (terminal-only) and Custom are absent.
        assert_eq!(ids, ["claude-code", "codex", "cursor", "amp", "opencode"]);
    }

    #[test]
    fn builtins_carry_their_static_transport_and_vocab() {
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&builtin_entries(), &s);
        let claude = roster.iter().find(|e| e.id == "claude-code").unwrap();
        assert_eq!(claude.transport, Transport::StreamJson);
        assert_eq!(claude.models, ["opus", "sonnet", "haiku"]);
        assert_eq!(claude.efforts, ["high", "medium", "low"]);
        assert_eq!(claude.default_model(), Some("opus"));
        let codex = roster.iter().find(|e| e.id == "codex").unwrap();
        assert_eq!(codex.transport, Transport::AppServer);
        assert_eq!(codex.models, ["gpt-5-codex", "o3"]);
        assert!(codex.efforts.is_empty());
    }

    #[test]
    fn acp_presets_are_acp_with_no_static_model_row_yet() {
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&builtin_entries(), &s);
        for id in ["cursor", "amp", "opencode"] {
            let e = roster.iter().find(|e| e.id == id).unwrap();
            assert_eq!(e.transport, Transport::Acp, "{id} should be ACP");
            assert!(e.models.is_empty(), "{id} has no static models yet");
            assert_eq!(e.default_model(), None);
        }
    }

    #[test]
    fn a_user_demoted_preset_drops_from_the_roster() {
        // A bare `[agents.cursor] model = "…"` (no transport/command) suppresses
        // the preset and is not chat-capable — it must not appear, mirroring the
        // launcher's guard against misrouting a preset click to Claude.
        let s = AgentLaunchSettings::from_toml_str("[agents.cursor]\nmodel = \"foo\"\n")
            .expect("parse");
        let roster = chat_roster(&builtin_entries(), &s);
        assert!(roster.iter().all(|e| e.id != "cursor"));
        // The other presets are unaffected.
        assert!(roster.iter().any(|e| e.id == "opencode"));
    }

    #[test]
    fn empty_detection_still_yields_the_presets() {
        // Even if the built-in binaries aren't detected, the ACP presets (their
        // own which-detection happens in the UI layer) still populate the roster.
        let s = AgentLaunchSettings::default();
        let roster = chat_roster(&[], &s);
        let ids: Vec<&str> = roster.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["cursor", "amp", "opencode"]);
    }
}
