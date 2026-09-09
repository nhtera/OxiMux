//! The `Merge into <default>` row action: pre-flight, dispatch, and what the
//! outcome means on screen.
//!
//! Two halves, split so the interesting one is testable without a window:
//!
//! - [`report_for`] and the peeling below are pure. They turn a
//!   [`MergeOutcome`] into what the user is told, and they are where the
//!   nesting trap lives.
//! - The `WorkspaceRoot` methods are the GPUI half: they read holders on the
//!   main thread, run the two-phase merge on the executor, and route the result.
//!
//! **`MergeOutcome::AutoStashed` wraps another variant rather than sitting
//! beside it.** A dirty project root plus an already-merged branch is
//! `AutoStashed { inner: AlreadyUpToDate }`, so a flat five-arm match reports
//! "merged" for a merge that did nothing. Everything here peels first and maps
//! second; the real shape is a cross-product of {stash present, pop failed} ×
//! {FastForward, Merged, AlreadyUpToDate, Conflicted}.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{Context, WeakEntity, Window};
use oximux_core::{MergeOutcome, Project, StashRef, Workspace};
use oximux_git::Repository;
use oximux_worktree_ops::{MergeRefusal, MergeResult};

use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmPrompt, ConfirmSecondary};
use crate::shell::workspace::merge_notices::{
    self, NoticeReason, StashLookup, StashNotice,
};
use crate::workspace_root::WorkspaceRoot;

/// What happened, at the granularity the user cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeReportKind {
    /// The branch is in the default branch now (fast-forward or true merge).
    Landed,
    /// The default branch already had everything. Nothing was committed.
    NothingToMerge,
    /// The merge is in progress with conflicts to resolve.
    Conflicted,
}

/// The outcome, translated. One report per merge, whatever shape the outcome
/// arrived in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReport {
    pub kind: MergeReportKind,
    pub headline: String,
    /// Conflicting paths, workdir-relative. Empty unless `kind` is
    /// `Conflicted`.
    pub conflicts: Vec<PathBuf>,
    /// `Some` when a stash is still on the stack and was **not** popped. This
    /// is the field that must never be dropped: it is the only pointer to the
    /// user's uncommitted work.
    pub stranded: Option<NoticeReason>,
    /// A stash was pushed and successfully put back. Worth a clause in the
    /// headline, nothing more — there is nothing for the user to do.
    pub stash_restored: bool,
    /// The project root has unmerged paths in it right now.
    ///
    /// True for a conflicted merge, and — the case that is easy to miss — for a
    /// **failed stash pop**, where the merge itself succeeded. `git stash pop`
    /// writes conflict markers into the user's files before giving up, so
    /// reporting that as a plain success leaves them looking at a clean-sounding
    /// toast and a repository full of `<<<<<<<`.
    pub tree_has_conflicts: bool,
}

/// Peel every `AutoStashed` wrapper off, returning the stash this merge pushed,
/// whether its pop failed, and the variant that actually describes the merge.
///
/// Recursive because the type permits nesting even though `merge_branch` does
/// not currently produce it; the outer ref wins, since that is the stash this
/// call pushed.
fn peel(outcome: MergeOutcome) -> (Option<StashRef>, bool, MergeOutcome) {
    match outcome {
        MergeOutcome::AutoStashed {
            stash_ref,
            inner,
            pop_failed,
        } => {
            let (_, inner_pop_failed, core) = peel(*inner);
            (Some(stash_ref), pop_failed || inner_pop_failed, core)
        }
        other => (None, false, other),
    }
}

/// Translate a merge outcome into what the user is told.
///
/// `branch` is the worktree branch that was landed; `default_branch` is where
/// it went.
pub fn report_for(outcome: MergeOutcome, branch: &str, default_branch: &str) -> MergeReport {
    let (stash, pop_failed, core) = peel(outcome);
    match core {
        // `Conflicted` carries its own un-popped stash rather than arriving
        // wrapped, so it is read from the variant, not from the peel.
        MergeOutcome::Conflicted {
            conflicts,
            auto_stash,
        } => {
            let n = conflicts.len();
            let files = if n == 1 { "file" } else { "files" };
            MergeReport {
                kind: MergeReportKind::Conflicted,
                headline: format!(
                    "\u{201c}{branch}\u{201d} conflicts with {default_branch} in {n} {files}: \
                     {}. Resolve them in Source Control.",
                    name_paths(&conflicts)
                ),
                conflicts,
                // Either source counts. The variant carries its own un-popped
                // ref, and `peel` may have found an outer one — dropping
                // whichever the match arm did not happen to read would lose the
                // pointer to the user's work.
                stranded: (auto_stash.is_some() || stash.is_some())
                    .then_some(NoticeReason::Conflicted),
                stash_restored: false,
                tree_has_conflicts: true,
            }
        }
        MergeOutcome::AlreadyUpToDate => MergeReport {
            kind: MergeReportKind::NothingToMerge,
            headline: format!(
                "Nothing to merge \u{2014} {default_branch} already has everything on \
                 \u{201c}{branch}\u{201d}."
            ),
            conflicts: Vec::new(),
            stranded: stranded_from_pop(stash.as_ref(), pop_failed),
            stash_restored: stash.is_some() && !pop_failed,
            tree_has_conflicts: pop_failed,
        },
        MergeOutcome::FastForward | MergeOutcome::Merged => {
            let stash_restored = stash.is_some() && !pop_failed;
            let mut headline =
                format!("Merged \u{201c}{branch}\u{201d} into {default_branch}.");
            if stash_restored {
                headline.push_str(" Your uncommitted changes were put back.");
            }
            if pop_failed {
                // The merge landed, but putting the stash back wrote conflict
                // markers into the working tree. Saying only "Merged" here is
                // the difference between a user who fixes it now and one who
                // finds `<<<<<<<` in a file next week.
                headline.push_str(
                    " Putting your uncommitted changes back left conflicts in the \
                     project folder \u{2014} resolve them in Source Control.",
                );
            }
            MergeReport {
                kind: MergeReportKind::Landed,
                headline,
                conflicts: Vec::new(),
                stranded: stranded_from_pop(stash.as_ref(), pop_failed),
                stash_restored,
                tree_has_conflicts: pop_failed,
            }
        }
        // Unreachable: `peel` strips every wrapper before this match runs.
        // Reported rather than ignored, because silently treating an
        // unpeelable outcome as a success is the exact bug this module is
        // shaped to prevent.
        MergeOutcome::AutoStashed { .. } => MergeReport {
            kind: MergeReportKind::NothingToMerge,
            headline: "The merge finished in a state OxiMux could not read. \
                       Check Source Control before continuing."
                .to_string(),
            conflicts: Vec::new(),
            stranded: stash.map(|_| NoticeReason::PopFailed),
            stash_restored: false,
            tree_has_conflicts: true,
        },
    }
}

/// Name the conflicting files, capped. A toast is one line: three names and a
/// count is the most that can be read at a glance, and the full list is in the
/// Source Control panel this routes to anyway.
fn name_paths(paths: &[PathBuf]) -> String {
    const SHOWN: usize = 3;
    let named: Vec<String> = paths
        .iter()
        .take(SHOWN)
        .map(|p| p.display().to_string())
        .collect();
    match paths.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!("{} and {rest} more", named.join(", ")),
        _ => named.join(", "),
    }
}

fn stranded_from_pop(stash: Option<&StashRef>, pop_failed: bool) -> Option<NoticeReason> {
    (stash.is_some() && pop_failed).then_some(NoticeReason::PopFailed)
}

impl WorkspaceRoot {
    /// Directories with a live **agent** in them, across every project whose
    /// panes this window has built.
    ///
    /// Narrower than [`Self::rename_holders`], and the difference is
    /// deliberate. A rename moves a directory, so any process whose cwd is
    /// inside it — a shell at a prompt included — is orphaned by it. A merge
    /// moves no directory; what it can destroy is *uncommitted edits*, via the
    /// auto-stash. So the refusal is scoped to things that edit: tracked agent
    /// PTYs, chat tabs, and hand-launched agents. Including plain terminals
    /// would refuse every merge, because the project root is exactly where the
    /// default terminal already sits.
    ///
    /// **Known limit:** this sees only THIS window's panes, the same blind spot
    /// `rename_holders` documents.
    pub(crate) fn agent_holders(&self, cx: &mut Context<Self>) -> HashSet<PathBuf> {
        let mut holders: HashSet<PathBuf> = HashSet::new();
        for panes in self.project_panes_by_project.values() {
            let panes = panes.read(cx);
            holders.extend(panes.agent_cwds(cx));
            for entry in panes.ambient_agents(cx) {
                // Same rule as a tracked agent: a hand-launched CLI sitting at
                // its prompt is not a reason to refuse a merge.
                if crate::shell::pane_group::turn_in_flight(&entry.agent.status) {
                    holders.insert(entry.cwd.clone());
                }
            }
        }
        holders
    }

    /// Read the project's real default branch and correct the stored one.
    ///
    /// `projects.default_branch` is seeded with a literal `"main"` placeholder
    /// at project-add, because nothing has opened the repository at that point.
    /// Left uncorrected it makes this whole feature unreachable on a repo whose
    /// default is anything else: the row would offer "Merge into main", and the
    /// pre-flight would refuse with "switch to main first" naming a branch that
    /// does not exist.
    ///
    /// Called on project activation, so the menu label is right before any menu
    /// can be opened. A repo where nothing resolves keeps whatever was stored —
    /// a name that cannot be verified is not an improvement on a placeholder.
    pub(crate) fn heal_default_branch(
        &mut self,
        project: &Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project_root = PathBuf::from(&project.root_path);
        let project_id = project.id.clone();
        let stored = project.default_branch.clone();
        let project_repo = self.app_state.project_repo.clone();
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();

        cx.spawn_in(window, async move |_, cx| {
            let Ok(repo) = Repository::open(&project_root).await else {
                return;
            };
            let Ok(Some(detected)) = repo.default_branch().await else {
                return;
            };
            if detected == stored {
                return;
            }
            if let Err(err) = project_repo.set_default_branch(&project_id, &detected) {
                tracing::warn!(?err, %project_id, "could not record the detected default branch");
                return;
            }
            tracing::info!(
                %project_id, %stored, %detected,
                "corrected the project's recorded default branch"
            );
            let _ = cx.update(|_window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    // The in-memory copies the rail and the row menu read from,
                    // or the label keeps the placeholder until the next launch.
                    for p in &mut this.app_state.recent_projects {
                        if p.id == project_id {
                            p.default_branch = detected.clone();
                        }
                    }
                    if let Some(active) = this.active_project.as_mut()
                        && active.id == project_id
                    {
                        active.default_branch = detected.clone();
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    /// Land `workspace`'s branch in its project's default branch.    /// Land `workspace`'s branch in its project's default branch.
    ///
    /// Two-phase on purpose: the pre-flight's git round-trips are hundreds of
    /// milliseconds, and holders read before them prove nothing about the
    /// moment the merge actually runs. The second read happens here, on the
    /// main thread, between the phases.
    pub(crate) fn merge_workspace_into_default(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The ROW's project, never the active one: the rail shows every
        // project's rows at once, and merging into the wrong repository's
        // default branch is the same mistake Phase 1 had to fix for Delete.
        let Some(project) = crate::shell::workspace_ops::resolve_project_for_workspace(
            &self.app_state.recent_projects,
            &workspace,
        ) else {
            tracing::info!(
                workspace_id = %workspace.id,
                "merge: workspace's project not open, ignoring"
            );
            return;
        };
        let project_root = PathBuf::from(&project.root_path);
        let default_branch = project.default_branch.clone();
        let holders = self.agent_holders(cx);
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let ws = workspace.clone();
        let proj = project.clone();

        cx.spawn_in(window, async move |_, cx| {
            let plan = oximux_worktree_ops::preflight_merge(
                &project_root,
                &ws,
                &default_branch,
                &holders,
            )
            .await;
            let plan = match plan {
                Ok(plan) => plan,
                Err(refusal) => {
                    let _ = cx.update(|window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.finish_merge(
                                &ws,
                                &proj,
                                MergeResult::Refused(refusal),
                                window,
                                cx,
                            );
                        });
                    });
                    return;
                }
            };
            // Fresh holder read, between the phases — see `apply_merge`.
            let holders_now = cx
                .update(|_window, cx| {
                    weak.update(cx, |this, cx| this.agent_holders(cx))
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let result = oximux_worktree_ops::apply_merge(&plan, &holders_now).await;
            let _ = cx.update(|window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.finish_merge(&ws, &proj, result, window, cx);
                });
            });
        })
        .detach();
    }

    /// Apply the result of a merge: say what happened, and never drop a stash.
    fn finish_merge(
        &mut self,
        workspace: &Workspace,
        project: &Project,
        result: MergeResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            MergeResult::Refused(refusal) => {
                let body = refusal.message(&workspace.branch);
                // "Nothing to merge" is a correct answer to a fair question,
                // not a failure. Everything else went wrong.
                let kind = if matches!(refusal, MergeRefusal::NothingToMerge) {
                    crate::shell::toast::ToastKind::Info
                } else {
                    crate::shell::toast::ToastKind::Error
                };
                crate::shell::toast::toast(cx, kind, body);
            }
            MergeResult::Failed {
                error,
                stranded_auto_stash,
            } => {
                tracing::warn!(
                    %error,
                    branch = %workspace.branch,
                    stranded_auto_stash,
                    "merge failed"
                );
                if stranded_auto_stash {
                    // The one path where the merge failed AND the user's
                    // uncommitted work went missing. Write the pointer down
                    // before the toast, same rule as every other stranded case.
                    self.record_stash_notice(
                        workspace,
                        project,
                        NoticeReason::MergeFailed,
                    );
                }
                if stranded_auto_stash {
                    let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
                    let project = project.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.offer_pending_stash_notices(&project, window, cx);
                        });
                    });
                }
                // git's own text is the best explanation of WHY the merge
                // failed — except in this one case, where it ends with a
                // literal `stash@{N}`. That index renumbers, so showing it
                // invites the user to pop the wrong entry by hand. The notice
                // just recorded resolves the stash by message instead, and the
                // raw text is in the log above for anyone debugging.
                let detail = if stranded_auto_stash {
                    "Your uncommitted changes could not be put back and are still stashed \
                     \u{2014} OxiMux will offer to restore them."
                        .to_string()
                } else {
                    error
                };
                crate::shell::toast::toast_op_error(
                    cx,
                    &format!("Merge \u{201c}{}\u{201d}", workspace.branch),
                    &detail,
                );
            }
            MergeResult::Completed(outcome) => {
                let report =
                    report_for(outcome, &workspace.branch, &project.default_branch);
                self.apply_merge_report(workspace, project, report, window, cx);
            }
        }
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// Route one translated report: persist any stranded stash FIRST, then say
    /// what happened, then send the user where the work is.
    ///
    /// The order is the safety property. A stash that reaches only a toast is
    /// lost the moment the toast is dismissed, so it is written down before
    /// anything that could fail or be dismissed happens.
    fn apply_merge_report(
        &mut self,
        workspace: &Workspace,
        project: &Project,
        report: MergeReport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stranded = report.stranded;
        if let Some(reason) = stranded {
            self.record_stash_notice(workspace, project, reason);
        }
        // Severity follows the WORKING TREE, not just the merge. A landed merge
        // whose stash pop conflicted is not a success the user can walk away
        // from — their files have markers in them.
        let kind = if report.tree_has_conflicts {
            crate::shell::toast::ToastKind::Warning
        } else {
            match report.kind {
                MergeReportKind::NothingToMerge => crate::shell::toast::ToastKind::Info,
                MergeReportKind::Landed => crate::shell::toast::ToastKind::Success,
                MergeReportKind::Conflicted => crate::shell::toast::ToastKind::Warning,
            }
        };
        let mut text = report.headline.clone();
        if report.stranded.is_some() {
            text.push_str(
                " Your uncommitted changes are still stashed \u{2014} \
                 OxiMux will offer to restore them.",
            );
        }
        crate::shell::toast::toast(cx, kind, text);

        // Ordered: the working tree wins. A merge that landed but left conflict
        // markers behind is routed like a conflict, because that is what the
        // user is now looking at.
        match report.kind {
            // The SCM panel is already rooted at the project root and already
            // renders unmerged rows and the conflict banner. Route to it rather
            // than growing a second conflict surface; kick its poller so the
            // banner is up by the time the user looks.
            //
            // The guard: `right_sidebar` is the ACTIVE project's, and the rail
            // shows every project's rows at once. Routing unconditionally would
            // open a different repository's Source Control on a conflict that
            // is not in it — the same wrong-repository mistake Phase 1 had to
            // fix for Delete. The toast already names the branch; a wrong panel
            // would contradict it.
            _ if report.tree_has_conflicts
                && self
                    .active_project
                    .as_ref()
                    .is_some_and(|p| p.id == project.id) =>
            {
                self.show_source_control(cx);
            }
            // Landing and cleaning up are two decisions. Never the same one:
            // a merged branch is still where the user's reflog, notes and
            // unpushed experiments live. Not offered while the tree is
            // conflicted either — the user has one job first.
            MergeReportKind::Landed if stranded.is_none() => {
                self.offer_delete_after_merge(workspace, project, window, cx);
            }
            _ => {}
        }

        // The toast just promised an offer. Project activation alone would not
        // deliver it — the project the merge happened in is the ACTIVE one, so
        // the next activation is only whenever the user switches away and back.
        // Deferred so it mounts after the routing above has settled.
        if stranded.is_some() {
            let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
            let project = project.clone();
            window.defer(cx, move |window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.offer_pending_stash_notices(&project, window, cx);
                });
            });
        }
    }

    /// After a clean landing, offer to remove the worktree — never do it.
    ///
    /// Suppressed when the merge stranded a stash: the recovery offer is the
    /// more urgent of the two, and stacking a destructive prompt on top of
    /// "your uncommitted work is missing" is how a user clicks the wrong one.
    fn offer_delete_after_merge(
        &mut self,
        workspace: &Workspace,
        project: &Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let ws = workspace.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |window, cx| {
            let weak = weak.clone();
            let ws = ws.clone();
            // Deferred so the mount cannot race this dialog's own teardown:
            // the observer that clears `confirm_dialog` fires on the notify
            // this callback returns into. `mount_confirm_dialog` drops the old
            // subscription before installing the new one, so a synchronous
            // mount would probably survive too — but "probably" is the wrong
            // word for a path whose failure mode is a dialog that never
            // appears. The defer is correct under both interleavings.
            window.defer(cx, move |window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.request_delete_workspace(ws, window, cx);
                });
            });
        });
        let prompt = ConfirmPrompt {
            title: "Merged \u{2014} delete this workspace?".into(),
            body: format!(
                "\u{201c}{}\u{201d} is in {} now. Its worktree and branch are still on disk.",
                workspace.name, project.default_branch
            )
            .into(),
            on_confirm,
            // The ellipsis is a promise: the audited delete confirmation, which
            // names the worktree and the branch, comes next. This prompt is the
            // offer, not the consent.
            confirm_label: Some("Delete workspace\u{2026}".into()),
            on_cancel: None,
            secondary: None,
        };
        self.mount_confirm_dialog(prompt, window, cx);
    }

    /// Write down the pointer to a stash a merge left on the stack.
    ///
    /// Always called BEFORE the toast that mentions it: a toast can be
    /// dismissed, and this is the only other record that the user's
    /// uncommitted work exists.
    fn record_stash_notice(
        &self,
        workspace: &Workspace,
        project: &Project,
        reason: NoticeReason,
    ) {
        merge_notices::record(
            &self.app_state.settings_repo,
            StashNotice::new(
                &project.id,
                &project.root_path,
                &workspace.branch,
                oximux_git::AUTO_STASH_MESSAGE,
                reason,
            ),
        );
    }

    /// Open the right sidebar on Source Control and force an immediate git
    /// poll, so a freshly-conflicted tree is on screen rather than 500ms away.
    fn show_source_control(&mut self, cx: &mut Context<Self>) {
        let Some(sidebar) = self.right_sidebar.clone() else {
            return;
        };
        sidebar.update(cx, |s, cx| {
            s.select_tab(crate::shell::right_sidebar::tab::RightTab::SourceControl, cx);
            if !s.open {
                s.toggle(cx);
            }
        });
        sidebar.read(cx).set_polling_focused(true);
    }

    /// Offer to restore any stash a previous merge left behind in `project`.
    ///
    /// Called on project activation, so the offer survives everything a toast
    /// does not: dismissal, a window close, a relaunch. It re-appears until the
    /// user restores or dismisses it, which is the point — the alternative is a
    /// stash the user can only find by running `git stash list` by hand.
    ///
    /// One at a time. Two stacked dialogs would be worse than one, and
    /// restoring the first re-triggers this on the next activation anyway.
    pub(crate) fn offer_pending_stash_notices(
        &mut self,
        project: &Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Never displace a dialog the user is already looking at.
        if self.confirm_dialog.is_some() {
            return;
        }
        let notices = merge_notices::load(&self.app_state.settings_repo);
        let Some(notice) = merge_notices::for_project(&notices, &project.id)
            .into_iter()
            .next()
        else {
            return;
        };
        self.open_stash_recovery_dialog(notice, window, cx);
    }

    /// Three-way: restore it, forget it, or leave it for later.
    fn open_stash_recovery_dialog(
        &mut self,
        notice: StashNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let for_restore = notice.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |window, cx| {
            let weak = weak.clone();
            let notice = for_restore.clone();
            let _ = weak.update(cx, |this, cx| {
                this.restore_stashed_changes(notice, window, cx);
            });
        });
        let weak_dismiss: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        let dismiss_id = notice.id.clone();
        let on_dismiss: ConfirmCallback = std::rc::Rc::new(move |_window, cx| {
            let weak = weak_dismiss.clone();
            let id = dismiss_id.clone();
            let _ = weak.update(cx, |this, _cx| {
                merge_notices::acknowledge(&this.app_state.settings_repo, &id);
            });
        });
        let prompt = ConfirmPrompt {
            title: "Your uncommitted changes are stashed".into(),
            body: format!(
                "{}\n\nRestoring puts them back in the project folder. \
                 Leave this for later and the offer comes back next time you open the project.",
                notice.message()
            )
            .into(),
            on_confirm,
            confirm_label: Some("Restore them".into()),
            on_cancel: None,
            // Three-way, so "Restore" renders as the safe primary and this
            // takes the destructive styling — forgetting the pointer is the
            // only irreversible choice on offer.
            secondary: Some(ConfirmSecondary {
                label: "Forget it".into(),
                on_click: on_dismiss,
            }),
        };
        self.mount_confirm_dialog(prompt, window, cx);
    }

    /// Pop the stash a notice points at, re-resolving it by message first.
    ///
    /// The stored index is never used: by now the stack may have renumbered,
    /// and popping the wrong entry confidently is worse than admitting the
    /// pointer is stale.
    fn restore_stashed_changes(
        &mut self,
        notice: StashNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings_repo = self.app_state.settings_repo.clone();
        let weak: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        cx.spawn_in(window, async move |_, cx| {
            let repo = match Repository::open(&notice.project_root).await {
                Ok(repo) => repo,
                Err(err) => {
                    let _ = cx.update(|_window, cx| {
                        let _ = weak.update(cx, |_, cx| {
                            crate::shell::toast::toast_op_error(
                                cx,
                                "Restore stashed changes",
                                &err.to_string(),
                            );
                        });
                    });
                    return;
                }
            };
            let (text, kind, acknowledge) = match merge_notices::resolve(&repo, &notice).await {
                StashLookup::Found(stash_ref) => match repo.stash_pop(&stash_ref).await {
                    Ok(()) => (
                        "Your uncommitted changes are back in the project folder.".to_string(),
                        crate::shell::toast::ToastKind::Success,
                        true,
                    ),
                    // Still conflicting with the merged result. The stash is
                    // untouched, so the notice must stay.
                    Err(err) => (
                        format!(
                            "Those changes still conflict with the merged result, so they were \
                             left stashed: {err}"
                        ),
                        crate::shell::toast::ToastKind::Warning,
                        false,
                    ),
                },
                StashLookup::Gone => (
                    "Those changes are no longer stashed \u{2014} they were already restored or \
                     dropped."
                        .to_string(),
                    crate::shell::toast::ToastKind::Info,
                    true,
                ),
                StashLookup::Ambiguous(n) => (
                    format!(
                        "{n} stashes carry that description, so OxiMux can\u{2019}t tell which is \
                         yours. Restore it from Source Control \u{2192} Stashes."
                    ),
                    crate::shell::toast::ToastKind::Warning,
                    false,
                ),
                StashLookup::Failed(err) => (
                    format!("Couldn\u{2019}t read the stash list: {err}"),
                    crate::shell::toast::ToastKind::Error,
                    false,
                ),
            };
            let _ = cx.update(|_window, cx| {
                if acknowledge {
                    merge_notices::acknowledge(&settings_repo, &notice.id);
                }
                let _ = weak.update(cx, |this, cx| {
                    crate::shell::toast::toast(cx, kind, text);
                    // Same rule as the conflict route: only surface Source
                    // Control when it is showing the repository the stash is
                    // actually in.
                    if this
                        .active_project
                        .as_ref()
                        .is_some_and(|p| p.id == notice.project_id)
                    {
                        this.show_source_control(cx);
                    }
                });
            });
        })
        .detach();
    }
}
