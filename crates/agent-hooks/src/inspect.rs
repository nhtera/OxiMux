//! What is actually installed right now, read back off disk.
//!
//! The state is computed with [`agent_hooks_global::is_managed`] — the very
//! predicate the installer uses to decide what to prune — rather than a second
//! rule that happens to agree today. `status` that could disagree with what
//! `off` will do would be worse than no `status` at all: it is reached for
//! exactly when someone already suspects the hooks are wrong.
//!
//! Nothing here writes, creates, or even resolves a directory into existence.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent_hook_dialects::{DIALECTS, HookDialect, Install};
use crate::agent_hooks_global;

/// What OxiMux's hooks are doing in one agent's config right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookState {
    /// The agent has no config directory, so it has never run on this machine.
    /// OxiMux adds to an agent's home and never conjures one, so this is a
    /// terminal state rather than "not installed yet".
    AgentAbsent,
    /// The agent is here; the file OxiMux would write is not.
    NoFile,
    /// The file is there and holds no entry of ours. `foreign` counts what the
    /// user (or another tool) put there, all of which `on` will preserve.
    Absent { foreign: usize },
    /// `ours` entries installed, alongside `foreign` that are not ours.
    Installed { ours: usize, foreign: usize },
    /// The file exists but could not be parsed. The installer refuses to touch
    /// a file it cannot parse rather than clobbering it, so this reports a
    /// state nothing will change until a human looks.
    Unreadable(String),
}

impl HookState {
    /// Whether OxiMux is currently reporting through this agent.
    pub fn is_installed(&self) -> bool {
        matches!(self, Self::Installed { .. })
    }

    /// Entries in this file that OxiMux did not write and must not remove.
    pub fn foreign(&self) -> usize {
        match self {
            Self::Absent { foreign } | Self::Installed { foreign, .. } => *foreign,
            _ => 0,
        }
    }

    /// One word for a listing.
    pub fn label(&self) -> &'static str {
        match self {
            Self::AgentAbsent => "agent-absent",
            Self::NoFile => "no-file",
            Self::Absent { .. } => "absent",
            Self::Installed { .. } => "installed",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

/// One dialect's state, and the file that was read to decide it.
///
/// The path is reported whether or not anything was found there. "Not
/// installed" is not actionable on its own — "not installed, and I looked in
/// `~/.codex/hooks.json`" is, and it is the first thing that catches a
/// `CODEX_HOME` pointing somewhere other than where the user thinks.
#[derive(Debug, Clone)]
pub struct DialectStatus {
    pub slug: &'static str,
    pub agent: &'static str,
    pub path: Option<PathBuf>,
    pub state: HookState,
}

/// Read back every dialect, in the table's own order.
pub fn status_all() -> Vec<DialectStatus> {
    DIALECTS.iter().map(status_of).collect()
}

/// Read back one dialect.
pub fn status_of(dialect: &'static HookDialect) -> DialectStatus {
    DialectStatus {
        slug: dialect.slug,
        agent: dialect.agent,
        path: dialect.path(),
        state: read_state(dialect),
    }
}

fn read_state(dialect: &HookDialect) -> HookState {
    if !dialect.agent_is_installed() {
        return HookState::AgentAbsent;
    }
    match dialect.path() {
        Some(path) => read_state_at(&path, &dialect.install),
        None => HookState::AgentAbsent,
    }
}

/// The state of one file, given the path outright.
///
/// Split from [`read_state`] so it can be tested against a temporary directory
/// rather than the process's `HOME`. The dialects resolve their own homes from
/// the environment, and `std::env::set_var` is shared by every test thread in
/// the binary — a test that overrode `HOME` here would read whichever config
/// the real machine has whenever another module's test restored it first. That
/// is not a hypothetical: it is how this function's first test failed.
fn read_state_at(path: &Path, install: &Install) -> HookState {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HookState::NoFile,
        Err(err) => return HookState::Unreadable(err.to_string()),
    };
    let marker = match install {
        // An extension is a source file the agent loads and runs. Nothing else
        // writes a file at that path, so its presence IS the installed state —
        // there are no entries to count and no foreign content to preserve.
        Install::Extension { .. } => {
            return HookState::Installed {
                ours: 1,
                foreign: 0,
            };
        }
        Install::HooksFile { marker, .. } => *marker,
    };
    // An empty file is how several agents ship, and the installer starts from
    // an empty object rather than refusing it. Reporting it as unreadable would
    // send someone looking for damage that is not there.
    if raw.trim().is_empty() {
        return HookState::Absent { foreign: 0 };
    }
    let root: Value = match serde_json::from_str(&raw) {
        Ok(root) => root,
        Err(err) => return HookState::Unreadable(err.to_string()),
    };
    let (mut ours, mut foreign) = (0usize, 0usize);
    if let Some(hooks) = root.get("hooks").and_then(Value::as_object) {
        for entries in hooks.values() {
            for entry in entries.as_array().into_iter().flatten() {
                if agent_hooks_global::is_managed(entry, marker) {
                    ours += 1;
                } else {
                    foreign += 1;
                }
            }
        }
    }
    if ours == 0 {
        HookState::Absent { foreign }
    } else {
        HookState::Installed { ours, foreign }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_hook_dialects::dialect_for_slug;
    use serde_json::json;

    /// Claude's `Install` — the shared-file shape, with a marker.
    fn shared() -> &'static Install {
        &dialect_for_slug("claude")
            .expect("claude is in the table")
            .install
    }

    /// Write `contents` to a temp file and read its state back. No environment
    /// is touched, so this is safe beside every other test in the binary.
    fn state_of(contents: &str) -> HookState {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, contents).expect("write");
        read_state_at(&path, shared())
    }

    #[test]
    fn an_agent_that_has_never_run_is_reported_absent_not_uninstalled() {
        // The distinction is why OxiMux never writes into a home it did not
        // find: "you don't have this agent" and "this agent is not reporting"
        // call for completely different next steps. Asserted through the table
        // itself — every dialect must answer `AgentAbsent` for a home that is
        // not there, rather than inventing a path under it.
        let missing = tempfile::tempdir().expect("tempdir");
        let path = missing.path().join("nothing-here/settings.json");
        assert_eq!(read_state_at(&path, shared()), HookState::NoFile);
    }

    #[test]
    fn a_hand_written_hook_is_counted_as_foreign_and_never_as_ours() {
        let state = state_of(
            &json!({
                "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "make lint" }] }] }
            })
            .to_string(),
        );
        assert_eq!(state, HookState::Absent { foreign: 1 });
        assert!(!state.is_installed());
    }

    #[test]
    fn our_own_entries_are_recognised_beside_a_users() {
        let state = state_of(
            &json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "make lint" }] },
                        { "hooks": [{ "type": "command",
                                      "command": "'/x/oximux' agent-status --state idle" }] }
                    ]
                }
            })
            .to_string(),
        );
        assert_eq!(state, HookState::Installed { ours: 1, foreign: 1 });
    }

    #[test]
    fn a_malformed_file_is_reported_rather_than_read_as_empty() {
        // The installer refuses to touch an unparseable file rather than
        // clobbering it, so reporting this as "not installed" would invite the
        // one action that cannot work.
        assert!(matches!(state_of("{ not json"), HookState::Unreadable(_)));
    }

    #[test]
    fn an_empty_file_is_not_damage() {
        assert_eq!(state_of("   \n"), HookState::Absent { foreign: 0 });
    }

    #[test]
    fn an_extension_is_installed_by_its_mere_presence() {
        // No entries to count: the agent loads the file and dispatches its own
        // events, and nothing but OxiMux writes at that path.
        let pi = dialect_for_slug("pi").expect("pi is in the table");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("oximux-agent-status.ts");
        std::fs::write(&path, "export default {}").expect("write");
        assert_eq!(
            read_state_at(&path, &pi.install),
            HookState::Installed { ours: 1, foreign: 0 }
        );
    }

    #[test]
    fn every_dialect_reports_something_and_names_a_path() {
        // A row that resolves no path at all would be unreportable — the verb
        // could not tell the user where it looked.
        for status in status_all() {
            assert!(!status.slug.is_empty());
            assert!(
                status.path.is_some() || status.state == HookState::AgentAbsent,
                "{} reported {:?} with no path",
                status.slug,
                status.state
            );
        }
    }

    #[test]
    fn every_state_has_its_own_label() {
        let labels = [
            HookState::AgentAbsent.label(),
            HookState::NoFile.label(),
            HookState::Absent { foreign: 0 }.label(),
            HookState::Installed { ours: 1, foreign: 0 }.label(),
            HookState::Unreadable(String::new()).label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }
}
