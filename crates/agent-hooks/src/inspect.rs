//! What is actually installed right now, read back off disk.
//!
//! The state is computed with [`agent_hooks_global::is_managed`] — the very
//! predicate the installer uses to decide what to prune — rather than a second
//! rule that happens to agree today. `status` that could disagree with what
//! `off` will do would be worse than no `status` at all: it is reached for
//! exactly when someone already suspects the hooks are wrong.
//!
//! Nothing here writes, creates, or even resolves a directory into existence.

use std::path::PathBuf;

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
    let Some(path) = dialect.path() else {
        return HookState::AgentAbsent;
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HookState::NoFile,
        Err(err) => return HookState::Unreadable(err.to_string()),
    };
    let marker = match &dialect.install {
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

    fn claude() -> &'static HookDialect {
        dialect_for_slug("claude").expect("claude is in the table")
    }

    /// Write `contents` where a dialect's file goes, inside a fake home.
    ///
    /// The dialect resolves its own home from the environment, so this drives
    /// the real resolution rather than a path handed in — which is the part
    /// worth testing. Serialized because process env is shared; see the same
    /// note in the dialect tests.
    fn with_claude_file<T>(contents: Option<&str>, body: impl FnOnce() -> T) -> T {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock();
        let home = tempfile::tempdir().expect("tempdir");
        let prior = std::env::var_os("HOME");
        // SAFETY: serialized by the lock above; restored before returning.
        unsafe { std::env::set_var("HOME", home.path()) };
        std::fs::create_dir_all(home.path().join(".claude")).expect("mkdir");
        if let Some(contents) = contents {
            std::fs::write(home.path().join(".claude/settings.json"), contents).expect("write");
        }
        let out = body();
        unsafe {
            match prior {
                Some(p) => std::env::set_var("HOME", p),
                None => std::env::remove_var("HOME"),
            }
        }
        out
    }

    #[test]
    fn an_agent_that_has_never_run_is_reported_absent_not_uninstalled() {
        // The distinction is the whole reason OxiMux never writes into a home
        // it did not find: "you don't have this agent" and "this agent is not
        // reporting" call for completely different next steps.
        let home = tempfile::tempdir().expect("tempdir");
        let prior = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let state = read_state(claude());
        unsafe {
            match prior {
                Some(p) => std::env::set_var("HOME", p),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(state, HookState::AgentAbsent);
    }

    #[test]
    fn a_hand_written_hook_is_counted_as_foreign_and_never_as_ours() {
        let user_hook = json!({
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "make lint" }] }] }
        })
        .to_string();
        let state = with_claude_file(Some(&user_hook), || read_state(claude()));
        assert_eq!(state, HookState::Absent { foreign: 1 });
        assert!(!state.is_installed());
    }

    #[test]
    fn our_own_entries_are_recognised_beside_a_users() {
        let mixed = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "make lint" }] },
                    { "hooks": [{ "type": "command",
                                  "command": "'/x/oximux' agent-status --state idle" }] }
                ]
            }
        })
        .to_string();
        let state = with_claude_file(Some(&mixed), || read_state(claude()));
        assert_eq!(state, HookState::Installed { ours: 1, foreign: 1 });
    }

    #[test]
    fn a_malformed_file_is_reported_rather_than_read_as_empty() {
        // The installer refuses to touch an unparseable file rather than
        // clobbering it, so reporting this as "not installed" would invite the
        // one action that cannot work.
        let state = with_claude_file(Some("{ not json"), || read_state(claude()));
        assert!(matches!(state, HookState::Unreadable(_)), "{state:?}");
    }

    #[test]
    fn an_empty_file_is_not_damage() {
        let state = with_claude_file(Some("   \n"), || read_state(claude()));
        assert_eq!(state, HookState::Absent { foreign: 0 });
    }

    #[test]
    fn a_missing_file_under_a_real_home_is_distinct_from_a_missing_agent() {
        let state = with_claude_file(None, || read_state(claude()));
        assert_eq!(state, HookState::NoFile);
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
}
