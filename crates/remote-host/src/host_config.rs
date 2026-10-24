//! Host settings both hosts must agree on.
//!
//! The desktop fills this from its Settings; the headless host from the shared
//! database's settings keys. Either way the dispatcher and [`super::HostCore`]
//! read one value, so a cap is the same number whichever host enforces it.
//! Held in a `watch` on the core: a change applies to the next reservation, no
//! restart needed.

use oximux_agents::lineage::SpawnCaps;

/// Settings keys, shared by both hosts' loaders.
pub mod keys {
    pub const DELEGATION_ENABLED: &str = "agents.delegation.enabled";
    pub const CONFINE_CHAT: &str = "agents.delegation.confine_chat";
    pub const EXPOSE_TOOLS: &str = "agents.delegation.expose_tools";
    pub const STEER_EVENTS: &str = "agents.delegation.steer_events";
    pub const CAP_DEPTH: &str = "agents.delegation.cap.depth";
    pub const CAP_ACTIVE_PER_PARENT: &str = "agents.delegation.cap.active_per_parent";
    pub const CAP_ACTIVE_PER_ROOT: &str = "agents.delegation.cap.active_per_root";
    pub const CAP_OPEN_PER_ROOT: &str = "agents.delegation.cap.open_per_root";
}

/// What the host reads to decide spawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostConfig {
    /// Agents may spawn agents at all (the spawn grant).
    pub delegation_enabled: bool,
    /// Desktop chat agents get a session credential confined to their own
    /// subtree instead of the operator's.
    pub confine_chat: bool,
    /// Chat agents get the delegation tools over MCP (`oximux mcp --tools
    /// orchestrate`), beside the `oximux delegate` command line.
    pub expose_tools: bool,
    /// A child's events go into its parent's running turn when the parent's
    /// CLI takes mid-turn messages, instead of waiting for the turn to end.
    pub steer_events: bool,
    /// The spawn caps. The depth is clamped to the hard ceiling on every read.
    pub caps: SpawnCaps,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            delegation_enabled: true,
            confine_chat: true,
            expose_tools: true,
            steer_events: true,
            caps: SpawnCaps::default(),
        }
    }
}

impl HostConfig {
    /// Read the config from a settings store (`get` returns a key's stored
    /// text). A missing or unreadable value keeps its default; the result is
    /// sanitized, so a hand-edited depth of 9 reads as 2.
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        let flag = |key: &str, default: bool| match get(key).as_deref() {
            Some("true" | "1" | "on") => true,
            Some("false" | "0" | "off") => false,
            _ => default,
        };
        let num = |key: &str, default: u32| get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default);
        Self {
            delegation_enabled: flag(keys::DELEGATION_ENABLED, d.delegation_enabled),
            confine_chat: flag(keys::CONFINE_CHAT, d.confine_chat),
            expose_tools: flag(keys::EXPOSE_TOOLS, d.expose_tools),
            steer_events: flag(keys::STEER_EVENTS, d.steer_events),
            caps: SpawnCaps {
                depth: num(keys::CAP_DEPTH, d.caps.depth),
                active_per_parent: num(keys::CAP_ACTIVE_PER_PARENT, d.caps.active_per_parent),
                active_per_root: num(keys::CAP_ACTIVE_PER_ROOT, d.caps.active_per_root),
                open_per_root: num(keys::CAP_OPEN_PER_ROOT, d.caps.open_per_root),
                ..d.caps
            },
        }
        .sanitized()
    }

    /// The config as settings-store entries, for [`Self::load`] to read back.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        vec![
            (keys::DELEGATION_ENABLED, self.delegation_enabled.to_string()),
            (keys::CONFINE_CHAT, self.confine_chat.to_string()),
            (keys::EXPOSE_TOOLS, self.expose_tools.to_string()),
            (keys::STEER_EVENTS, self.steer_events.to_string()),
            (keys::CAP_DEPTH, self.caps.depth.to_string()),
            (keys::CAP_ACTIVE_PER_PARENT, self.caps.active_per_parent.to_string()),
            (keys::CAP_ACTIVE_PER_ROOT, self.caps.active_per_root.to_string()),
            (keys::CAP_OPEN_PER_ROOT, self.caps.open_per_root.to_string()),
        ]
    }

    /// The config as enforced: the depth never exceeds the ceiling, and every
    /// count cap stays between 1 and its settable maximum, whatever was stored.
    pub fn sanitized(self) -> Self {
        let clamp = |v: u32, max: u32| v.clamp(1, max);
        let caps = self.caps.clamped();
        Self {
            caps: SpawnCaps {
                depth: caps.depth.max(1),
                active_per_parent: clamp(caps.active_per_parent, 16),
                active_per_root: clamp(caps.active_per_root, 16),
                open_per_root: clamp(caps.open_per_root, 32),
                ..caps
            },
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizing_clamps_depth_and_count_caps() {
        let wild = HostConfig {
            caps: SpawnCaps {
                depth: 5,
                active_per_parent: 0,
                active_per_root: 99,
                open_per_root: 1000,
                ..SpawnCaps::default()
            },
            ..HostConfig::default()
        }
        .sanitized();
        assert_eq!(wild.caps.depth, 2);
        assert_eq!(wild.caps.active_per_parent, 1);
        assert_eq!(wild.caps.active_per_root, 16);
        assert_eq!(wild.caps.open_per_root, 32);
        assert_eq!(wild.caps.lifetime_per_root, 100, "the budget is not settable");
    }

    #[test]
    fn settings_round_trip_and_bad_values_keep_defaults() {
        let custom = HostConfig {
            delegation_enabled: false,
            confine_chat: true,
            expose_tools: false,
            steer_events: false,
            caps: SpawnCaps { depth: 2, active_per_parent: 3, ..SpawnCaps::default() },
        };
        let stored: std::collections::HashMap<&str, String> = custom.entries().into_iter().collect();
        assert_eq!(HostConfig::load(|k| stored.get(k).cloned()), custom);
        let junk = |k: &str| (k == keys::CAP_DEPTH).then(|| "nine".to_string());
        assert_eq!(HostConfig::load(junk), HostConfig::default());
        let deep = |k: &str| (k == keys::CAP_DEPTH).then(|| "9".to_string());
        assert_eq!(HostConfig::load(deep).caps.depth, 2, "clamped to the ceiling");
    }

    #[test]
    fn defaults_match_the_plan() {
        let c = HostConfig::default();
        assert!(c.delegation_enabled && c.confine_chat);
        assert_eq!(
            (c.caps.depth, c.caps.active_per_parent, c.caps.active_per_root, c.caps.open_per_root),
            (1, 8, 8, 16)
        );
    }
}
