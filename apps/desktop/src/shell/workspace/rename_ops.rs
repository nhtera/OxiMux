//! Renaming a workspace from the desktop: the host half of
//! `oximux-worktree-ops::rename`.
//!
//! The crate owns the ordering and the undo; this owns everything the crate
//! deliberately refuses to guess at — which directories have something running
//! in them, what the new path should be called, and what to say when a rename
//! is refused.
//!
//! Lifted out of `workspace_ops.rs` when that file crossed the 3000-LOC hard
//! cap `xtask file-size-lint` enforces in CI. It is a concern, not a pile: the
//! rename dialog, the inline double-click rename and the refusal's label-only
//! escape hatch are one flow.

use std::path::{Path, PathBuf};

use gpui::{Context, WeakEntity, Window};
use oximux_core::Workspace;

use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmPrompt};
use crate::shell::workspace::workspace_ops::resolve_project_for_workspace;
use crate::workspace_root::WorkspaceRoot;

/// Rewrite every persisted pane blob for `project_id` that named `old_root`.
///
/// A free function over `&SettingsRepo` rather than a method, so the
/// quit-and-relaunch guarantee can be asserted against a real settings store
/// without a window: this is the step that makes a rename atomic *in effect*
/// rather than merely on disk, and "asserted by test" is the only way that
/// claim means anything.
///
/// Returns the number of keys rewritten.
pub fn repoint_persisted_tabs_in(
    repo: &oximux_storage::SettingsRepo,
    project_id: &str,
    old_root: &str,
    new_root: &str,
) -> usize {
    use crate::session_restore::persisted_terminals::{PersistedTabs, repoint_worktree_paths};
    // Covers every window's key AND the legacy pre-V005 `terminal_tabs:<id>`,
    // which shares this prefix — a rename in one window must not leave another
    // window's saved layout pointing at the dead path.
    let prefix = format!("terminal_tabs:{project_id}");
    let rows = match repo.list_prefixed(&prefix) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(?err, project_id, "repoint_persisted_tabs: list failed");
            return 0;
        }
    };
    let mut rewritten = 0;
    for (key, raw) in rows {
        // A string prefix is not an id match: `terminal_tabs:<id>` is also a
        // prefix of `terminal_tabs:<id>2:main`. Accept only the legacy key
        // itself or a real per-window key under it, so one project can never
        // rewrite another's layout.
        let is_this_project = key == prefix
            || key
                .strip_prefix(&prefix)
                .is_some_and(|rest| rest.starts_with(':'));
        if !is_this_project {
            continue;
        }
        let Ok(mut tabs) = serde_json::from_str::<PersistedTabs>(&raw) else {
            continue;
        };
        if repoint_worktree_paths(&mut tabs, old_root, new_root) == 0 {
            continue;
        }
        match serde_json::to_string(&tabs) {
            Ok(json) => match repo.set(&key, &json) {
                Ok(()) => rewritten += 1,
                Err(err) => tracing::warn!(?err, %key, "repoint_persisted_tabs: write failed"),
            },
            Err(err) => tracing::warn!(?err, %key, "repoint_persisted_tabs: serialize failed"),
        }
    }
    rewritten
}

/// Whether `slug` is a faithful rendering of `name`, or a degraded stand-in.
///
/// [`derive_slug`](oximux_git::derive_slug) never fails: a name with no
/// `[a-z0-9]` in it becomes the literal `"workspace"`, and a long one is
/// truncated at a word boundary. That is right for *creation*, where the user
/// is watching a slug field and can correct it — but a rename now rewrites a
/// real branch, and silently turning a label edit of `"!!!"` into
/// `oximux/workspace` is a git mutation the user never asked for and cannot see
/// coming.
///
/// Returns the reason it degraded, or `None` when the slug faithfully
/// represents the name.
fn slug_degradation(name: &str, slug: &str) -> Option<String> {
    // The literal fallback, reached only when nothing in the name survived
    // normalisation. A user who genuinely typed "workspace" is unaffected: the
    // slug is then a faithful rendering, not a stand-in.
    if slug == "workspace" && oximux_git::derive_slug("workspace") == slug {
        let has_usable = name
            .bytes()
            .any(|b| b.to_ascii_lowercase().is_ascii_lowercase() || b.is_ascii_digit());
        if !has_usable {
            return Some(format!(
                "\u{201c}{name}\u{201d} has no letters or digits to build a branch name from"
            ));
        }
    }
    // Truncation: the branch would not match what was typed.
    let full = name
        .bytes()
        .filter(|b| b.to_ascii_lowercase().is_ascii_lowercase() || b.is_ascii_digit())
        .count();
    let kept = slug.bytes().filter(|b| b.is_ascii_alphanumeric()).count();
    if full > kept {
        return Some(format!(
            "\u{201c}{name}\u{201d} is too long for a branch name; it would become              \u{201c}oximux/{slug}\u{201d}"
        ));
    }
    None
}

/// Where a renamed worktree's directory should go.
///
/// Keeps the directory's existing naming SHAPE rather than replacing the whole
/// final component with the new slug: the two creation paths in the app name
/// their directories differently (`<slug>` under the data dir, `oximux-wt-<slug>`
/// beside the repo), and a rename should not silently re-shape one into the
/// other. Only the slug inside the name is swapped; a directory whose name does
/// not contain the old slug falls back to the plain new slug.
fn renamed_worktree_dir(old_path: &Path, old_slug: &str, new_slug: &str) -> PathBuf {
    let parent = old_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let current = old_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let next = if !old_slug.is_empty() && current.contains(old_slug) {
        current.replacen(old_slug, new_slug, 1)
    } else {
        new_slug.to_string()
    };
    parent.join(next)
}

impl WorkspaceRoot {
    /// Every directory something is running in — the set a rename must refuse
    /// against.
    ///
    /// Deliberately wider than [`Self::live_worktree_paths`], which answers the
    /// rail's question ("is this worktree live?") rather than this one ("would
    /// moving this directory orphan anything?"). It adds:
    ///
    /// - **hand-launched (ambient) agents**, which have no tracked session;
    /// - **each terminal's LIVE cwd**, because `live_worktree_paths` reports a
    ///   group's static root, so a shell that `cd`-ed into the worktree from a
    ///   group rooted elsewhere would otherwise be invisible;
    /// - **agent-chat tabs**, whose headless subprocess and companion PTY are
    ///   both rooted at the tab's cwd and neither of which is a `PaneGroupTab`.
    ///
    /// Callers match by containment, not equality, so a cwd anywhere beneath a
    /// worktree counts as holding it.
    ///
    /// **Known limit:** this sees only THIS window's panes.
    /// `project_panes_by_project` is per-window, so the same project open in a
    /// second window can hold a worktree without appearing here. The rail has
    /// the identical blind spot, so the two agree; closing it needs
    /// cross-window aggregation that does not exist yet.
    pub(crate) fn rename_holders(
        &self,
        cx: &mut Context<Self>,
    ) -> std::collections::HashSet<PathBuf> {
        let mut holders: std::collections::HashSet<PathBuf> = self
            .live_worktree_paths(cx)
            .into_iter()
            .map(PathBuf::from)
            .collect();
        for panes in self.project_panes_by_project.values() {
            let panes = panes.read(cx);
            for entry in panes.ambient_agents(cx) {
                holders.insert(entry.cwd.clone());
            }
            holders.extend(panes.live_terminal_cwds(cx));
        }
        holders
    }

    /// Rename a workspace (DB only — no git or filesystem changes). Shared by
    /// the rename dialog and the left rail's inline double-click rename, so the
    /// trim + empty-guard + error-toast contract stays in one place.
    pub(crate) fn rename_workspace_now(
        &mut self,
        workspace: Workspace,
        new_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() || new_name == workspace.name {
            return;
        }
        // A synthesized primary row is the project's own checkout and has no DB
        // row behind it; there is nothing to rename. The menu already gates
        // this, but the inline double-click is a second way in.
        if workspace.id.starts_with("primary:") {
            return;
        }
        let Some(project) = resolve_project_for_workspace(&self.app_state.recent_projects, &workspace)
        else {
            tracing::info!(
                workspace_id = %workspace.id,
                "rename_workspace_now: workspace's project not open, ignoring"
            );
            return;
        };
        let new_slug = oximux_git::derive_slug(&new_name);
        // `derive_slug` cannot fail, so without this the branch would be
        // rewritten to something the user never typed and never saw. Refuse
        // instead — the dialog still offers the label-only rename, which is
        // exactly what someone editing a display label wanted anyway.
        if let Some(reason) = slug_degradation(&new_name, &new_slug) {
            self.open_rename_refusal_dialog(
                workspace,
                new_name,
                oximux_worktree_ops::RenameRefusal::InvalidSlug { reason },
                window,
                cx,
            );
            return;
        }
        let old_path = PathBuf::from(&workspace.worktree_path);
        let new_path = renamed_worktree_dir(&old_path, &workspace.slug, &new_slug);
        // Holders are computed HERE, on the main thread, from live entity state
        // — `rename_with_rollback` cannot ask for them itself and must not
        // guess. See `rename_holders`.
        let holders = self.rename_holders(cx);
        let project_root = PathBuf::from(&project.root_path);
        let workspace_repo = self.app_state.workspace_repo.clone();
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let ws = workspace.clone();
        let name_for_task = new_name.clone();

        cx.spawn_in(window, async move |_, cx| {
            let outcome = oximux_worktree_ops::rename_with_rollback(
                &project_root,
                &ws,
                &name_for_task,
                &new_slug,
                &new_path,
                &holders,
                &workspace_repo,
            )
            .await;
            let _ = cx.update(|window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.finish_rename(ws, name_for_task, old_path, new_path, outcome, window, cx);
                });
            });
        })
        .detach();
    }

    /// Apply the result of a rename: repoint what keyed on the old path, or
    /// explain why nothing moved.
    #[allow(clippy::too_many_arguments)]
    fn finish_rename(
        &mut self,
        workspace: Workspace,
        new_name: String,
        old_path: PathBuf,
        new_path: PathBuf,
        outcome: oximux_worktree_ops::RenameOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use oximux_worktree_ops::RenameOutcome;
        match outcome {
            RenameOutcome::Renamed(_) => {
                // The directory moved, so every persisted pane row that named
                // the old path now names a path that does not exist. Rewrite
                // them before the next save can re-persist the stale value.
                self.repoint_persisted_tabs(
                    &workspace.project_id,
                    &old_path.to_string_lossy(),
                    &new_path.to_string_lossy(),
                );
                self.mark_rail_dirty(cx);
                cx.notify();
            }
            RenameOutcome::Refused(refusal) => {
                self.open_rename_refusal_dialog(workspace, new_name, refusal, window, cx);
            }
            RenameOutcome::RolledBack { error } => {
                tracing::warn!(%error, workspace_id = %workspace.id, "rename rolled back");
                crate::shell::toast::toast_op_error(
                    cx,
                    &format!("Rename workspace \u{201c}{}\u{201d}", workspace.name),
                    &error,
                );
            }
            RenameOutcome::RollbackFailed { error, rollback } => {
                // The one state a human has to repair by hand. Say both halves:
                // what failed, and what could not be put back.
                tracing::error!(%error, %rollback, workspace_id = %workspace.id, "rename rollback FAILED");
                crate::shell::toast::toast(
                    cx,
                    crate::shell::toast::ToastKind::Error,
                    format!(
                        "Rename of \u{201c}{}\u{201d} failed ({error}) and could not be undone ({rollback}) \u{2014} check the worktree by hand",
                        workspace.name,
                    ),
                );
            }
        }
    }

    /// A refused rename raises a dialog rather than a toast, because there is a
    /// choice in it: the cosmetic label-only rename is still available and is
    /// often what the user actually wanted.
    fn open_rename_refusal_dialog(
        &mut self,
        workspace: Workspace,
        new_name: String,
        refusal: oximux_worktree_ops::RenameRefusal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = refusal.message(&workspace.branch);
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let ws = workspace.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |_window, cx| {
            let weak = weak.clone();
            let ws = ws.clone();
            let name = new_name.clone();
            let _ = weak.update(cx, |this, cx| {
                this.confirm_dialog = None;
                this.rename_label_only(&ws, &name, cx);
            });
        });
        let prompt = ConfirmPrompt {
            title: "Can\u{2019}t rename this workspace".into(),
            body: format!(
                "{body}\n\nYou can still change the name shown in the sidebar \u{2014} \
                 the branch and folder keep their current names."
            )
            .into(),
            on_confirm,
            confirm_label: Some("Change label only".into()),
            on_cancel: None,
            secondary: None,
        };
        self.mount_confirm_dialog(prompt, window, cx);
    }

    /// Change only the display label, leaving branch and directory alone.
    ///
    /// This is what `rename` did for every case before the full rename existed.
    /// It stays reachable because it is sometimes exactly what is wanted — and
    /// it is the escape hatch a refusal offers.
    pub(crate) fn rename_label_only(
        &mut self,
        workspace: &Workspace,
        new_name: &str,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.app_state.workspace_repo.rename(&workspace.id, new_name) {
            tracing::warn!(?err, workspace_id = %workspace.id, "rename label failed");
            crate::shell::toast::toast_op_error(
                cx,
                &format!("Rename workspace \u{201c}{}\u{201d}", workspace.name),
                &err.to_string(),
            );
        }
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Rewrite every persisted pane blob for `project_id` that named
    /// `old_root`, so a relaunch restores tabs in the renamed directory instead
    /// of at a path that no longer exists.
    ///
    /// Covers every window's key plus the legacy pre-V005 one — a rename in one
    /// window must not leave another window's saved layout pointing at the dead
    /// path.
    fn repoint_persisted_tabs(&self, project_id: &str, old_root: &str, new_root: &str) {
        repoint_persisted_tabs_in(
            &self.app_state.settings_repo,
            project_id,
            old_root,
            new_root,
        );
    }

    /// Open the rename dialog pre-filled with the workspace's current
    /// name. Step 7's sidebar context menu will route here.
    #[allow(dead_code)]
    pub(crate) fn request_rename_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_dialog
            .update(cx, |d, cx| d.open_rename(workspace, window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::{renamed_worktree_dir, slug_degradation};

    /// The two creation paths in the app name their worktree directories
    /// differently. A rename must preserve whichever shape it finds rather than
    /// quietly re-shaping one into the other.
    #[test]
    fn a_rename_keeps_the_directorys_existing_naming_shape() {
        // Rail-created: the directory IS the slug.
        assert_eq!(
            renamed_worktree_dir(
                std::path::Path::new("/data/projects/abc/worktrees/fix-lgoin"),
                "fix-lgoin",
                "fix-login",
            ),
            std::path::PathBuf::from("/data/projects/abc/worktrees/fix-login"),
        );
        // Chat-created: the slug is embedded in a prefixed name, which survives.
        assert_eq!(
            renamed_worktree_dir(
                std::path::Path::new("/repos/oximux-wt-fix-lgoin"),
                "fix-lgoin",
                "fix-login",
            ),
            std::path::PathBuf::from("/repos/oximux-wt-fix-login"),
        );
    }

    /// A directory whose name has drifted from its slug still has to go
    /// somewhere sensible — the new slug, in the same parent.
    #[test]
    fn a_directory_that_does_not_contain_its_slug_falls_back_to_the_new_slug() {
        assert_eq!(
            renamed_worktree_dir(
                std::path::Path::new("/wt/hand-renamed-by-someone"),
                "fix-lgoin",
                "fix-login",
            ),
            std::path::PathBuf::from("/wt/fix-login"),
        );
    }

    /// Only the FIRST occurrence is swapped: a slug that also appears earlier in
    /// the name must not be rewritten twice.
    #[test]
    fn only_the_first_occurrence_of_the_slug_is_replaced() {
        assert_eq!(
            renamed_worktree_dir(std::path::Path::new("/wt/a-a"), "a", "b"),
            std::path::PathBuf::from("/wt/b-a"),
        );
    }

    /// A rename now rewrites a real branch, so a name that `derive_slug`
    /// silently turns into something else must be refused rather than applied.
    #[test]
    fn a_name_with_nothing_usable_in_it_is_reported_as_degraded() {
        let slug = oximux_git::derive_slug("!!!");
        assert_eq!(slug, "workspace", "precondition: the silent fallback");
        let reason = slug_degradation("!!!", &slug).expect("must be refused");
        assert!(reason.contains("no letters or digits"), "got {reason}");
    }

    /// Someone who genuinely types "workspace" is not degrading anything.
    #[test]
    fn the_literal_name_workspace_is_not_a_degradation() {
        let slug = oximux_git::derive_slug("workspace");
        assert_eq!(slug, "workspace");
        assert!(slug_degradation("workspace", &slug).is_none());
    }

    /// Truncation is the quieter half: the branch simply stops matching what
    /// was typed, with nothing on screen to say so.
    #[test]
    fn a_name_too_long_for_a_branch_is_reported_as_degraded() {
        let long = "a-very-long-workspace-name-that-keeps-going-and-going-well-past-the-cap";
        let slug = oximux_git::derive_slug(long);
        assert!(slug.len() < long.len(), "precondition: it truncates");
        let reason = slug_degradation(long, &slug).expect("must be refused");
        assert!(reason.contains("too long"), "got {reason}");
    }

    /// The ordinary case must pass straight through — punctuation and spaces
    /// normalising to dashes is a faithful rendering, not a degradation.
    #[test]
    fn an_ordinary_name_is_not_degraded() {
        for name in ["fix login", "Fix Login!", "issue-42", "fix_login"] {
            let slug = oximux_git::derive_slug(name);
            assert!(
                slug_degradation(name, &slug).is_none(),
                "{name} -> {slug} should be accepted"
            );
        }
    }
}
