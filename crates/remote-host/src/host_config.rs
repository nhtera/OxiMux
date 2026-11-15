//! Host settings both hosts must agree on.
//!
//! The desktop fills this from its Settings; the headless host from the shared
//! database's settings keys. Either way the dispatcher and [`super::HostCore`]
//! read one value, so a cap is the same number whichever host enforces it.
//! Held in a `watch` on the core: a change applies to the next reservation, no
//! restart needed.

use oximux_agents::lineage::SpawnCaps;
use oximux_storage::SettingsRepo;

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
    pub const VERIFY_ALLOW: &str = "agents.delegation.verify_allow";
    pub const STALL_AFTER_S: &str = "agents.health.stall_after_s";
    pub const MAX_RESUMES: &str = "agents.recovery.max_resumes";
}

/// The commands a Claude verifier may run when nothing else is configured:
/// builds, tests and lints of the common toolchains, and `git status`. No
/// `npm run`-style catch-alls, and no `git diff`/`git log` (their `--output`
/// writes a file). A rule matches a command's start: the flags after it are
/// the agent's, so a listed tool can still run code it is pointed at.
pub const DEFAULT_VERIFY_ALLOW: &[&str] = &[
    "Bash(cargo build:*)",
    "Bash(cargo check:*)",
    "Bash(cargo clippy:*)",
    "Bash(cargo test:*)",
    "Bash(go build:*)",
    "Bash(go test:*)",
    "Bash(go vet:*)",
    "Bash(make test:*)",
    "Bash(npm test:*)",
    "Bash(pnpm test:*)",
    "Bash(yarn test:*)",
    "Bash(pytest:*)",
    "Bash(git status:*)",
];

/// What kind of value a setting takes, for the one verb that writes them
/// (`oximux config set`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `true` or `false` (`1`/`0`, `on`/`off`, `yes`/`no` are taken too).
    Flag,
    /// A whole number. The caps are clamped on load, so what a host enforces
    /// is not always the number stored — which is why `set` answers with the
    /// effective value rather than echoing the request.
    Count,
    /// One `Bash(<command>)` rule per line.
    Rules,
}

impl ValueKind {
    /// The text to store for `value`, or what this kind takes instead.
    ///
    /// **Validation is here and not in the loader.** [`HostConfig::load`]
    /// keeps a default for anything it cannot read, which is right for a
    /// hand-edited row at startup and wrong for a verb: stored text the loader
    /// ignores would be reported back as the default and read as a setting
    /// that had been applied. So a value is refused before it is written.
    pub fn canonical(self, value: &str) -> Result<String, String> {
        let value = value.trim();
        match self {
            Self::Flag => match value {
                "true" | "1" | "on" | "yes" => Ok("true".into()),
                "false" | "0" | "off" | "no" => Ok("false".into()),
                _ => Err("takes true or false".into()),
            },
            Self::Count => {
                value.parse::<u32>().map(|n| n.to_string()).map_err(|_| "takes a whole number".into())
            }
            Self::Rules => {
                let rules: Vec<&str> = value.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                let Some(bad) = rules.iter().find(|rule| !is_command_rule(rule)) else {
                    return if rules.is_empty() {
                        Err("takes one Bash(<command>) rule per line".into())
                    } else {
                        Ok(rules.join("\n"))
                    };
                };
                Err(format!("{bad} is not a Bash(<command>) rule, which is all a verifier may run"))
            }
        }
    }
}

/// Every setting this host reads, and the kind of value it takes.
///
/// One list, because the verb's key set, its validation and the loader must
/// not drift: [`HostConfig::entries`] is the value side of these same keys,
/// and `settable_and_entries_name_the_same_keys` holds the two together.
pub const SETTABLE: &[(&str, ValueKind)] = &[
    (keys::DELEGATION_ENABLED, ValueKind::Flag),
    (keys::CONFINE_CHAT, ValueKind::Flag),
    (keys::EXPOSE_TOOLS, ValueKind::Flag),
    (keys::STEER_EVENTS, ValueKind::Flag),
    (keys::CAP_DEPTH, ValueKind::Count),
    (keys::CAP_ACTIVE_PER_PARENT, ValueKind::Count),
    (keys::CAP_ACTIVE_PER_ROOT, ValueKind::Count),
    (keys::CAP_OPEN_PER_ROOT, ValueKind::Count),
    (keys::VERIFY_ALLOW, ValueKind::Rules),
    (keys::STALL_AFTER_S, ValueKind::Count),
    (keys::MAX_RESUMES, ValueKind::Count),
];

/// One setting as a host enforces it now: what `oximux config` prints, and
/// what the wire entry is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: &'static str,
    /// In force now — after loading and clamping, not as it was asked for.
    pub value: String,
    /// Nothing is stored for this key: the value is the shipped default.
    pub is_default: bool,
}

/// The config either side of one [`crate::HostCore::edit_config`] call.
///
/// Both are read under the settings-write lock, so a caller can tell what its
/// own edit changed without racing another writer: comparing against a config
/// it read beforehand could attribute an `oximux config set` to itself, or
/// miss its own change entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEdit {
    /// In force before the edit.
    pub before: HostConfig,
    /// In force now — sanitized, and already stored.
    pub after: HostConfig,
}

/// Why a settings read or write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// This host has no setting by that name.
    UnknownKey,
    /// Not what the key takes; the text says what it does take.
    BadValue(String),
    /// This host keeps no settings store, so there is nothing to read back or
    /// persist into — a core built without one answers rather than pretending.
    NoStore,
    /// The store refused the write.
    Write(String),
    /// The store could not be read back, so nothing was applied: a missing
    /// value and an unreadable one are the same to [`HostConfig::load`], and
    /// applying the defaults it would substitute would silently undo settings
    /// this host is still holding.
    Read(String),
}

/// Refuse text the settings store must not be given.
///
/// [`SettingsRepo`] stores whatever it is handed and leaves the limit to its
/// callers, so **every** config write path has to hold it: an oversized value
/// would be stored, read back at the next start as the posture to enforce, and
/// carried to every surface that lists the config. Only `Rules` can plausibly
/// reach the limit — a flag or a count cannot — but it is checked per stored
/// value, so a key that grows a longer kind later is covered without a second
/// rule to keep in step.
pub(crate) fn within_store_limit(key: &str, text: &str) -> Result<(), ConfigError> {
    if text.len() > SettingsRepo::MAX_VALUE_BYTES {
        return Err(ConfigError::BadValue(format!(
            "takes at most {} KiB, and {key} was given {} KiB",
            SettingsRepo::MAX_VALUE_BYTES / 1024,
            text.len() / 1024,
        )));
    }
    Ok(())
}

/// The kind `key` takes, or `None` when this host has no such setting.
pub fn kind_of(key: &str) -> Option<ValueKind> {
    SETTABLE.iter().find(|(k, _)| *k == key).map(|(_, kind)| *kind)
}

/// What the host reads to decide spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The Bash rules a Claude verifier runs under (`permissions.allow`). Only
    /// `Bash(<command>)` rules survive sanitizing: an edit tool, an MCP tool
    /// or a bare `Bash(*)` would widen the posture past "builds and tests".
    pub verify_allow: Vec<String>,
    /// Seconds a working session may go without a sign of life before it is
    /// marked stalled (informational only). 0 turns the signal off.
    pub stall_after_s: u32,
    /// Children a restart's recovery turns may run at once (each spawns an
    /// agent and is billed); the rest wait in line. 0 sends none: the
    /// children stay parked and each gets the note with its next message.
    pub max_resumes: u32,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            delegation_enabled: true,
            confine_chat: true,
            expose_tools: true,
            steer_events: true,
            caps: SpawnCaps::default(),
            verify_allow: DEFAULT_VERIFY_ALLOW.iter().map(|r| r.to_string()).collect(),
            stall_after_s: (oximux_agents::health::DEFAULT_STALL_AFTER_MS / 1000) as u32,
            max_resumes: oximux_agents::resume_governor::DEFAULT_MAX_RESUMES,
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
            verify_allow: get(keys::VERIFY_ALLOW)
                .map(|v| v.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>())
                .filter(|rules| !rules.is_empty())
                .unwrap_or(d.verify_allow),
            stall_after_s: num(keys::STALL_AFTER_S, d.stall_after_s),
            max_resumes: num(keys::MAX_RESUMES, d.max_resumes),
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
            (keys::VERIFY_ALLOW, self.verify_allow.join("\n")),
            (keys::STALL_AFTER_S, self.stall_after_s.to_string()),
            (keys::MAX_RESUMES, self.max_resumes.to_string()),
        ]
    }

    /// The health thresholds this config sets.
    pub fn health(&self) -> oximux_agents::health::HealthConfig {
        oximux_agents::health::HealthConfig {
            stall_after_ms: i64::from(self.stall_after_s) * 1000,
            ..Default::default()
        }
    }

    /// The config as enforced: the depth never exceeds the ceiling, every
    /// count cap stays between 1 and its settable maximum (recovery turns
    /// between 0, off, and theirs), and the verifier
    /// keeps only command rules, whatever was stored.
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
            verify_allow: Some(self.verify_allow.into_iter().filter(|r| is_command_rule(r)).collect::<Vec<_>>())
                .filter(|rules| !rules.is_empty())
                .unwrap_or_else(|| Self::default().verify_allow),
            max_resumes: self.max_resumes.min(10),
            ..self
        }
    }
}

/// `Bash(<command>)`, optionally ending in `:*` or ` *` (any arguments): a
/// named command, with no wildcard anywhere else (`Bash(*)`, `Bash(* *)`,
/// `Bash(*test*)` would match almost anything) and nothing outside the
/// parentheses.
fn is_command_rule(rule: &str) -> bool {
    let Some(inner) = rule.strip_prefix("Bash(").and_then(|r| r.strip_suffix(')')) else {
        return false;
    };
    let command = inner.strip_suffix(":*").or_else(|| inner.strip_suffix(" *")).unwrap_or(inner).trim();
    !command.is_empty() && !command.contains(['*', '(', ')'])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verb's key set and the loader's are the same set. Two lists of the
    /// same keys is how `oximux config list` comes to omit a setting the host
    /// obeys, or to name one it does not.
    #[test]
    fn settable_and_entries_name_the_same_keys() {
        let mut settable: Vec<&str> = SETTABLE.iter().map(|(key, _)| *key).collect();
        let mut loaded: Vec<&str> = HostConfig::default().entries().into_iter().map(|(key, _)| key).collect();
        settable.sort_unstable();
        loaded.sort_unstable();
        assert_eq!(settable, loaded);
    }

    /// Every settable key takes its own default back. A kind that disagrees
    /// with the loader — a flag listed as a count — would refuse the value the
    /// host itself writes.
    #[test]
    fn every_key_accepts_the_value_the_host_writes_for_it() {
        for (key, value) in HostConfig::default().entries() {
            let kind = kind_of(key).expect("a settable key");
            assert_eq!(kind.canonical(&value).as_deref(), Ok(value.as_str()), "{key}");
        }
    }

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
            verify_allow: vec!["Bash(just test:*)".into(), "Bash(cargo nextest run:*)".into()],
            stall_after_s: 90,
            max_resumes: 5,
        };
        let stored: std::collections::HashMap<&str, String> = custom.entries().into_iter().collect();
        assert_eq!(HostConfig::load(|k| stored.get(k).cloned()), custom);
        let junk = |k: &str| (k == keys::CAP_DEPTH).then(|| "nine".to_string());
        assert_eq!(HostConfig::load(junk), HostConfig::default());
        let deep = |k: &str| (k == keys::CAP_DEPTH).then(|| "9".to_string());
        assert_eq!(HostConfig::load(deep).caps.depth, 2, "clamped to the ceiling");
    }

    #[test]
    fn a_verifier_keeps_only_command_rules() {
        let stored = |k: &str| {
            (k == keys::VERIFY_ALLOW)
                .then(|| "Bash(cargo test:*)\nBash(*)\nBash(:*)\nBash(* *)\nBash(*test*)\nEdit\nmcp__oximux__oximux_spawn_agent\n Bash(go test *) \nBash(x) && Bash(y)".into())
        };
        assert_eq!(HostConfig::load(stored).verify_allow, ["Bash(cargo test:*)", "Bash(go test *)"]);
        assert!(HostConfig::default().verify_allow.iter().all(|r| is_command_rule(r)), "the defaults survive");
        let only_junk = |k: &str| (k == keys::VERIFY_ALLOW).then(|| "Bash(*)\nEdit".into());
        assert_eq!(HostConfig::load(only_junk).verify_allow, HostConfig::default().verify_allow, "never left with none");
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
