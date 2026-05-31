//! SQLite-backed per-worktree settings persistence helpers for the
//! Source Control panel.
//!
//! Free functions only — no `impl SourceControlPanel` here. The panel
//! call sites (`new` for load, `set_base_ref` for save) hold the
//! `WorktreeSettingsRepo` clone themselves and pass it in. This keeps
//! the panel struct unaware of the persistence layer's failure modes:
//! a missing repo, a missing row, a SQLite hiccup, and a stale
//! `updated_at` all collapse into `Option<String>` or
//! `Result<(), StorageError>` at this boundary.
//!
//! Both functions log at warn but never raise — base-ref persistence is
//! a UX nicety, not a panel invariant. Restart will reset to the repo
//! default rather than wedge the panel.

use oximux_git::Repository;
use oximux_storage::WorktreeSettingsRepo;

/// Write the per-workspace base ref into the V006 `worktree_settings`
/// row, preserving sibling fields (`commit_draft`, `view_mode_override`)
/// that other surfaces own. Delegates to
/// [`WorktreeSettingsRepo::modify`], which fuses the read+write under
/// one connection lock so concurrent writers (view-mode toggle,
/// commit-draft debounce) cannot interleave a stale read.
pub fn merge_base_ref_into_settings(
    settings_repo: &WorktreeSettingsRepo,
    workspace_id: &str,
    value: Option<String>,
) -> Result<(), oximux_storage::StorageError> {
    settings_repo.modify(workspace_id, |s| s.base_ref = value)
}

/// Read the persisted base ref for this workspace from the V006
/// `worktree_settings` table. Falls through to `None` (= use the repo
/// default) whenever the read fails, the row is absent, or no settings
/// repo was provided (test wiring). Errors are logged at warn but
/// never raised — a missing initial value is not a startup-blocker.
pub(super) fn load_initial_base_ref(
    settings_repo: Option<&WorktreeSettingsRepo>,
    repo: &Repository,
) -> Option<String> {
    let sr = settings_repo?;
    let workspace_id = repo.workdir().to_string_lossy().to_string();
    match sr.get(&workspace_id) {
        Ok(Some(settings)) => settings.base_ref,
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                target: "oximux_app::source_control",
                error = %err,
                workspace_id = %workspace_id,
                "worktree_settings.get failed; base ref defaults to repo default",
            );
            None
        }
    }
}
