//! Auto-rename from the desktop: the host half of
//! `oximux-worktree-ops::auto_rename`.
//!
//! The crate decides *whether* a codename workspace should take the name of
//! its work and *to what*; this decides *when*, which is "after the user has
//! seen it and said yes". The signal is a chat's generated task summary,
//! routed here as [`OfferWorkspaceAutoRename`](crate::actions::OfferWorkspaceAutoRename)
//! from the pane group. The offer is a toast with `Rename` / `Keep`; a
//! timeout keeps. Nothing here ever shows a failure — the user did not ask
//! for a rename, and a refused optimisation is a debug line, not an error.
//!
//! The one exception is a rollback that itself failed, which is not a failed
//! optimisation but a repository a human has to repair by hand; that is
//! toasted exactly as the manual rename toasts it.

use std::path::PathBuf;

use gpui::Context;
use oximux_core::Workspace;
use oximux_worktree_ops::{
    AutoRenameProposal, RenameOutcome, auto_rename_with_rollback, is_generated_codename,
    preflight_auto_rename, propose_auto_rename,
};

use crate::shell::toast::{ToastAction, ToastKind, toast, toast_with_actions};
use crate::shell::workspace::workspace_ops::resolve_project_for_workspace;
use crate::workspace_root::WorkspaceRoot;

/// The offer's text: the branch that would change and what it would become.
/// Pure, so the wording is pinned by a test rather than by reading a toast.
pub fn offer_text(workspace: &Workspace, proposal: &AutoRenameProposal) -> String {
    format!(
        "Name this worktree \u{201c}{}\u{201d}? \u{201c}{}\u{201d} \u{2192} \u{201c}{}\u{201d}",
        proposal.new_name, workspace.branch, proposal.new_branch
    )
}

/// The success toast: old branch → new branch, and nothing else.
pub fn renamed_text(old_branch: &str, new_branch: &str) -> String {
    format!("Renamed \u{201c}{old_branch}\u{201d} \u{2192} \u{201c}{new_branch}\u{201d}")
}

impl WorkspaceRoot {
    /// A chat at `cwd` has its first generated summary. Decide, pre-flight,
    /// and — only if every check passes — offer.
    ///
    /// The pre-flight runs *before* the offer so the user is never shown a
    /// button that would silently do nothing: a pushed branch or a name
    /// collision is declined here, in the log, and no toast appears.
    pub(crate) fn offer_workspace_auto_rename(
        &mut self,
        cwd: PathBuf,
        summary: String,
        cx: &mut Context<Self>,
    ) {
        // Exact match on the row's path: a chat tab's cwd IS the worktree
        // root (that is what the create path spawns it at).
        let workspace = match self
            .app_state
            .workspace_repo
            .get_by_worktree_path(&cwd.to_string_lossy())
        {
            Ok(Some(ws)) => ws,
            Ok(None) => {
                tracing::debug!(cwd = %cwd.display(), "auto-rename: no workspace row at this cwd");
                return;
            }
            Err(err) => {
                tracing::debug!(?err, cwd = %cwd.display(), "auto-rename: row lookup failed");
                return;
            }
        };
        let Some(project) = resolve_project_for_workspace(&self.app_state.recent_projects, &workspace)
        else {
            tracing::debug!(workspace_id = %workspace.id, "auto-rename: project not open");
            return;
        };
        let proposal = match propose_auto_rename(&workspace, &summary) {
            Ok(p) => p,
            Err(reason) => {
                tracing::debug!(
                    workspace_id = %workspace.id,
                    slug = %workspace.slug,
                    ?reason,
                    "auto-rename: not eligible"
                );
                return;
            }
        };
        let project_root = PathBuf::from(&project.root_path);
        cx.spawn(async move |weak, cx| {
            match preflight_auto_rename(&project_root, &workspace, &proposal).await {
                Err(refusal) => {
                    tracing::debug!(
                        workspace_id = %workspace.id,
                        ?refusal,
                        "auto-rename: pre-flight refused"
                    );
                }
                Ok(_plan) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.show_auto_rename_offer(workspace, project_root, proposal, cx);
                    });
                }
            }
        })
        .detach();
    }

    /// The toast. `Rename` applies; `Keep <codename>` and the timeout both
    /// leave the codename standing. A chat raises its summary once, so a
    /// declined offer does not come back from the same chat; a *second* chat
    /// opened in the same worktree may offer again, because the only record
    /// of the decision is the slug, and the slug did not change.
    fn show_auto_rename_offer(
        &mut self,
        workspace: Workspace,
        project_root: PathBuf,
        proposal: AutoRenameProposal,
        cx: &mut Context<Self>,
    ) {
        let text = offer_text(&workspace, &proposal);
        let keep_label = format!("Keep {}", workspace.slug);
        let weak = cx.weak_entity();
        let rename = ToastAction::new("Rename", move |cx| {
            let workspace = workspace.clone();
            let project_root = project_root.clone();
            let proposal = proposal.clone();
            let _ = weak.update(cx, |this, cx| {
                this.apply_workspace_auto_rename(workspace, project_root, proposal, cx);
            });
        });
        toast_with_actions(cx, ToastKind::Info, text, vec![rename, ToastAction::dismiss(keep_label)]);
    }

    /// The user said yes. The row is re-read first: the toast may have sat
    /// for twenty seconds, during which a manual rename (or a second chat's
    /// offer) could have moved the branch — and the one-shot rule lives in
    /// the fresh row's slug, not in the stale one this offer was built from.
    ///
    /// This guards against a rename that has *finished*, not one in flight:
    /// two offers clicked in the same instant both pass and both spawn, and
    /// the second's `git branch -m` then fails on a branch that is already
    /// gone — a `RolledBack` with nothing touched, logged and not shown.
    fn apply_workspace_auto_rename(
        &mut self,
        workspace: Workspace,
        project_root: PathBuf,
        proposal: AutoRenameProposal,
        cx: &mut Context<Self>,
    ) {
        let fresh = match self.app_state.workspace_repo.get_by_id(&workspace.id) {
            Ok(Some(row)) => row,
            _ => {
                tracing::debug!(workspace_id = %workspace.id, "auto-rename: row gone before apply");
                return;
            }
        };
        if !is_generated_codename(&fresh.slug) || fresh.branch != workspace.branch {
            tracing::debug!(
                workspace_id = %fresh.id,
                slug = %fresh.slug,
                "auto-rename: row changed since the offer; leaving it"
            );
            return;
        }
        let workspace_repo = self.app_state.workspace_repo.clone();
        cx.spawn(async move |weak, cx| {
            let outcome =
                auto_rename_with_rollback(&project_root, &fresh, &proposal, &workspace_repo).await;
            let _ = weak.update(cx, |this, cx| {
                this.finish_auto_rename(&fresh, &proposal, outcome, cx);
            });
        })
        .detach();
    }

    /// Success is one toast. Every refusal and rollback is a log line and no
    /// UI. The directory did not move, so nothing keyed on the path needs
    /// repointing and the manual `Rename` still works afterwards.
    fn finish_auto_rename(
        &mut self,
        workspace: &Workspace,
        proposal: &AutoRenameProposal,
        outcome: RenameOutcome,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            RenameOutcome::Renamed(_) => {
                tracing::info!(
                    workspace_id = %workspace.id,
                    from = %workspace.branch,
                    to = %proposal.new_branch,
                    "auto-renamed workspace"
                );
                toast(
                    cx,
                    ToastKind::Success,
                    renamed_text(&workspace.branch, &proposal.new_branch),
                );
                self.mark_rail_dirty(cx);
                cx.notify();
            }
            RenameOutcome::Refused(refusal) => {
                tracing::debug!(workspace_id = %workspace.id, ?refusal, "auto-rename: refused at apply");
            }
            RenameOutcome::RolledBack { error } => {
                tracing::warn!(workspace_id = %workspace.id, %error, "auto-rename rolled back");
            }
            RenameOutcome::RollbackFailed { error, rollback } => {
                // Not a failed optimisation: a repository someone has to fix.
                tracing::error!(
                    workspace_id = %workspace.id,
                    %error,
                    %rollback,
                    "auto-rename rollback FAILED"
                );
                toast(
                    cx,
                    ToastKind::Error,
                    format!(
                        "Rename of \u{201c}{}\u{201d} failed ({error}) and could not be undone ({rollback}) \u{2014} check the worktree by hand",
                        workspace.name,
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(slug: &str, branch: &str) -> Workspace {
        Workspace {
            id: "ws-1".into(),
            project_id: "p-1".into(),
            name: slug.into(),
            slug: slug.into(),
            branch: branch.into(),
            worktree_path: format!("/wt/{slug}"),
            status: "active".into(),
            created_at: String::new(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
            branch_minted: true,
        }
    }

    #[test]
    fn the_offer_names_both_branches_and_the_new_label() {
        let ws = row("amber", "oximux/amber");
        let proposal = propose_auto_rename(&ws, "Fix login redirect").unwrap();
        let text = offer_text(&ws, &proposal);
        assert!(text.contains("Fix login redirect"), "{text}");
        assert!(text.contains("oximux/amber"), "{text}");
        assert!(text.contains("oximux/fix-login-redirect"), "{text}");
    }

    #[test]
    fn the_success_toast_is_old_arrow_new() {
        assert_eq!(
            renamed_text("oximux/amber", "oximux/fix-login-redirect"),
            "Renamed \u{201c}oximux/amber\u{201d} \u{2192} \u{201c}oximux/fix-login-redirect\u{201d}"
        );
    }
}
