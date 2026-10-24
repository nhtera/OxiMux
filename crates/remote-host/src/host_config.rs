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
    /// The spawn caps. The depth is clamped to the hard ceiling on every read.
    pub caps: SpawnCaps,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self { delegation_enabled: true, confine_chat: true, caps: SpawnCaps::default() }
    }
}

impl HostConfig {
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
    fn defaults_match_the_plan() {
        let c = HostConfig::default();
        assert!(c.delegation_enabled && c.confine_chat);
        assert_eq!(
            (c.caps.depth, c.caps.active_per_parent, c.caps.active_per_root, c.caps.open_per_root),
            (1, 8, 8, 16)
        );
    }
}
