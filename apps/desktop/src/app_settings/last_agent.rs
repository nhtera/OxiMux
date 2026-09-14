//! The create dialog's Agent default: the last agent the user chose.
//!
//! A UX preference in the global `SettingsRepo` key/value store, following
//! `scm_layout_settings`'s reasoning — it belongs to the person at this
//! keyboard, not to a repository or a team, so it is not `git.toml` state.
//!
//! Written on every successful create, read when the dialog opens. The
//! resolution is total: last used (a remembered `Skip` included) → the
//! launch settings' default agent → the first adapter in the dialog's order
//! → `Skip`. `Skip` as a *fallback* is right; `Skip` as the *default* is what
//! made "open a project, create a workspace, start an agent" three steps.

use oximux_core::AgentAdapter;
use oximux_storage::SettingsRepo;

/// Settings key holding the adapter id of the last agent chosen in the
/// create dialog, or [`SKIP`] when the user chose no agent.
pub const KEY_WORKSPACE_LAST_AGENT: &str = "workspace_last_agent";

/// The stored value for "no agent". A remembered Skip is a choice, and the
/// default follows the user rather than overriding them.
pub const SKIP: &str = "skip";

/// The built-in adapters in the dialog's order — the same order the create
/// dialog lists them. The first is the last-resort default.
pub const DIALOG_ORDER: &[AgentAdapter] = &[
    AgentAdapter::ClaudeCode,
    AgentAdapter::Codex,
    AgentAdapter::Pi,
    AgentAdapter::Omp,
    AgentAdapter::Custom,
];

/// The adapter's id as the launch picker and `agent_launch.toml` spell it.
pub fn adapter_id(kind: AgentAdapter) -> &'static str {
    match kind {
        AgentAdapter::ClaudeCode => "claude-code",
        AgentAdapter::Codex => "codex",
        AgentAdapter::Pi => "pi",
        AgentAdapter::Omp => "omp",
        AgentAdapter::Custom => "custom",
    }
}

/// The inverse of [`adapter_id`]; `None` for anything this build does not
/// know, so a value written by a newer build reads as "no preference"
/// rather than as an error.
pub fn adapter_from_id(id: &str) -> Option<AgentAdapter> {
    DIALOG_ORDER
        .iter()
        .copied()
        .find(|k| adapter_id(*k) == id.trim())
}

/// What the store remembers: `None` when nothing was ever chosen,
/// `Some(None)` for a remembered Skip, `Some(Some(_))` for an agent.
pub fn load(repo: &SettingsRepo) -> Option<Option<AgentAdapter>> {
    match repo.get(KEY_WORKSPACE_LAST_AGENT) {
        Ok(Some(raw)) if raw.trim() == SKIP => Some(None),
        Ok(Some(raw)) => adapter_from_id(&raw).map(Some),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(?err, "last-agent preference unreadable; using the fallback chain");
            None
        }
    }
}

/// Remember the choice a successful create was made with.
pub fn save(repo: &SettingsRepo, choice: Option<AgentAdapter>) {
    let value = choice.map(adapter_id).unwrap_or(SKIP);
    if let Err(err) = repo.set(KEY_WORKSPACE_LAST_AGENT, value) {
        tracing::warn!(?err, "last-agent preference not saved");
    }
}

/// The dialog's default, resolved through the whole chain. Pure, so the
/// chain is testable without a store: `last` is what [`load`] returned and
/// `launch_default` is `AgentLaunchSettings::default_agent` (empty = none).
pub fn resolve_default(
    last: Option<Option<AgentAdapter>>,
    launch_default: &str,
) -> Option<AgentAdapter> {
    if let Some(remembered) = last {
        return remembered;
    }
    if let Some(configured) = adapter_from_id(launch_default) {
        return Some(configured);
    }
    DIALOG_ORDER.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::open_memory;

    #[test]
    fn ids_round_trip_and_unknown_ids_read_as_no_preference() {
        for kind in DIALOG_ORDER {
            assert_eq!(adapter_from_id(adapter_id(*kind)), Some(*kind));
        }
        assert_eq!(adapter_from_id("from-a-newer-build"), None);
        assert_eq!(adapter_from_id(""), None);
    }

    /// The chain, link by link: last used wins (Skip included), then the
    /// launch default, then the first adapter. It never fails to answer.
    #[test]
    fn the_resolution_chain_is_total_and_ordered() {
        assert_eq!(resolve_default(Some(Some(AgentAdapter::Codex)), "pi"), Some(AgentAdapter::Codex));
        assert_eq!(resolve_default(Some(None), "pi"), None, "a remembered Skip is a choice");
        assert_eq!(resolve_default(None, "pi"), Some(AgentAdapter::Pi));
        assert_eq!(resolve_default(None, ""), Some(AgentAdapter::ClaudeCode));
        assert_eq!(resolve_default(None, "not-an-adapter"), Some(AgentAdapter::ClaudeCode));
    }

    #[test]
    fn the_store_round_trips_an_agent_and_a_skip() {
        let repo = SettingsRepo::new(open_memory().unwrap());
        assert_eq!(load(&repo), None, "nothing chosen yet");
        save(&repo, Some(AgentAdapter::Omp));
        assert_eq!(load(&repo), Some(Some(AgentAdapter::Omp)));
        save(&repo, None);
        assert_eq!(load(&repo), Some(None), "Skip is remembered, not erased");
        repo.set(KEY_WORKSPACE_LAST_AGENT, "from-a-newer-build").unwrap();
        assert_eq!(load(&repo), None, "an unknown id falls through to the chain");
    }
}
