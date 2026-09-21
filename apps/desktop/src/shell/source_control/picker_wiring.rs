//! Branch-picker coordination glue for the Source Control panel.
//!
//! Two surfaces (the toolbar branch chip and the settings-2 button)
//! both open the SAME `BranchPicker` entity. The picker's on-pick
//! callback reads its live mode at fire time and routes the outcome
//! here through [`SourceControlPanel::apply_picker_outcome`], which
//! dispatches to:
//!
//! - Switch mode → [`switch_to_branch`] | [`create_branch_and_switch`]
//! - BaseRef mode → [`set_base_ref`] (via [`merge_base_ref_into_settings`])
//!
//! `switch_to_branch` and `create_branch_and_switch` write their result
//! to the commit area's status row via the file-local
//! [`write_branch_op_status`] helper, which guards against clobbering
//! an in-flight commit/push/pull/sync status owned by `commit_ops`.
//!
//! [`refresh_after_branch_change`] lives here too, beside the two callers
//! that move HEAD. It is the fan-out this panel never had: the poller covers
//! the branch name, the file list and "Committed on Branch", but nothing
//! covered the commit graph, which kept painting the previous branch's
//! history until something else poked it.
//!
//! [`switch_to_branch`]: SourceControlPanel::switch_to_branch
//! [`refresh_after_branch_change`]: SourceControlPanel::refresh_after_branch_change
//! [`create_branch_and_switch`]: SourceControlPanel::create_branch_and_switch
//! [`set_base_ref`]: SourceControlPanel::set_base_ref
//! [`merge_base_ref_into_settings`]: super::settings_persistence::merge_base_ref_into_settings

use gpui::{Context, Window};
use std::sync::Arc;

use crate::shell::source_control::SourceControlPanel;
use crate::shell::source_control::branch_picker::{PickerMode, PickerOutcome};
use crate::shell::source_control::commit_area::{CommitArea, CommitStatus};
use crate::shell::source_control::settings_persistence::merge_base_ref_into_settings;

impl SourceControlPanel {
    /// Open the branch picker in Switch mode anchored under the toolbar
    /// branch chip. Branch list is fetched async; the popover opens
    /// immediately (empty list with the placeholder text), then populates
    /// when `list_branches` returns. `list_branches` is local and
    /// typically resolves in tens of milliseconds.
    pub(super) fn open_switch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let current = self.git_state.as_ref().and_then(|s| s.branch.clone());
        let picker = self.branch_picker.clone();
        // Snapshot before the spawn: the anchor is measured in the panel's
        // current spacing, and the async body outlives this borrow.
        let style = self.style();
        cx.spawn_in(window, async move |_panel_weak, cx| {
            let candidates: Vec<String> = match repo.list_branches().await {
                Ok(bs) => {
                    let names: Vec<String> = bs.into_iter().map(|b| b.name).collect();
                    promote_current_branch(names, current.as_deref())
                }
                Err(err) => {
                    tracing::warn!(
                        target: "oximux_app::source_control",
                        error = %err,
                        "list_branches failed; opening picker with empty list",
                    );
                    Vec::new()
                }
            };
            let _ = picker.update_in(cx, |p, window, cx| {
                p.set_mode(PickerMode::Switch);
                p.open(
                    candidates,
                    current.clone(),
                    style.pad_h,
                    style.tab_h + style.toolbar_h,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// Branch-picker callback router. Lives on the panel so the spawned
    /// closure stays tiny (just `panel.apply_picker_outcome(...)`) and
    /// the actual dispatch is testable without GPUI plumbing. `mode`
    /// is the picker's live mode at fire time — the same `Branch`
    /// outcome means "switch to it" in Switch mode but "use it as the
    /// diff base" in BaseRef mode.
    pub(super) fn apply_picker_outcome(
        &mut self,
        outcome: PickerOutcome,
        mode: PickerMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (mode, outcome) {
            (PickerMode::Switch, PickerOutcome::Branch(name)) => self.switch_to_branch(name, cx),
            (PickerMode::Switch, PickerOutcome::CreateFromHead(name)) => {
                self.create_branch_and_switch(name, cx)
            }
            (PickerMode::BaseRef, PickerOutcome::Branch(name)) => {
                self.set_base_ref(Some(name), cx)
            }
            (PickerMode::BaseRef, PickerOutcome::UseRepoDefault) => self.set_base_ref(None, cx),
            // Impossible combinations per `build_rows` invariants:
            // Switch mode never emits `UseRepoDefault`; BaseRef mode
            // never emits `CreateFromHead`. Log defensively so a future
            // refactor that loosens those invariants is loud.
            (mode, outcome) => {
                tracing::warn!(
                    target: "oximux_app::source_control",
                    ?mode,
                    ?outcome,
                    "branch picker emitted an outcome that's invalid for the active mode; ignoring",
                );
            }
        }
    }

    /// Persist a new base ref selection — both in memory (so the
    /// dropdown updates on the next render) and on disk (so it survives
    /// app restart). Persistence errors are logged but never raised:
    /// the in-memory change still wins so the picker UX isn't held
    /// hostage to SQLite hiccups.
    pub(super) fn set_base_ref(&mut self, value: Option<String>, cx: &mut Context<Self>) {
        self.base_ref = value.clone();
        cx.notify();
        let Some(ref settings_repo) = self.worktree_settings_repo else {
            return;
        };
        let workspace_id = self.repo.workdir().to_string_lossy().to_string();
        if let Err(err) = merge_base_ref_into_settings(settings_repo, &workspace_id, value) {
            tracing::warn!(
                target: "oximux_app::source_control",
                error = %err,
                workspace_id = %workspace_id,
                "worktree_settings.upsert failed; base ref change won't survive restart",
            );
        }
    }

    /// Open the branch picker in BaseRef mode anchored under the
    /// toolbar. Populates from `list_remote_branches` (5 s cache
    /// absorbs spam clicks). The picker's "(repo default)" row maps
    /// to `set_base_ref(None)`; any other row maps to
    /// `set_base_ref(Some(name))`.
    ///
    /// Anchor uses the same left offset as the Switch picker rather
    /// than the settings button's right-edge position — the picker
    /// card is wide enough (`CARD_WIDTH = 280`) to cover the button
    /// regardless, but the precise anchor wants the source button's
    /// measured bounds, which GPUI doesn't expose synchronously from
    /// the click handler. Worth revisiting once `Bounds` capture from
    /// the toolbar's interactive elements becomes ergonomic.
    pub(super) fn open_base_ref_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let picker = self.branch_picker.clone();
        let current_base = self.base_ref.clone();
        let style = self.style();
        cx.spawn_in(window, async move |_panel_weak, cx| {
            let candidates: Vec<String> = match repo.list_remote_branches(false).await {
                Ok(bs) => bs.into_iter().map(|b| b.name).collect(),
                Err(err) => {
                    tracing::warn!(
                        target: "oximux_app::source_control",
                        error = %err,
                        "list_remote_branches failed; opening BaseRef picker with empty list",
                    );
                    Vec::new()
                }
            };
            let _ = picker.update_in(cx, |p, window, cx| {
                p.set_mode(PickerMode::BaseRef);
                p.open(
                    candidates,
                    current_base,
                    style.pad_h,
                    style.tab_h + style.toolbar_h,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// Hand over the status poller. Called by `RightSidebar` right after it
    /// builds this panel; see the field's note.
    pub(crate) fn set_poller(&mut self, poller: Arc<oximux_git::StatusPoller>) {
        self.poller = Some(poller);
    }

    /// Re-sync everything that describes HEAD after the branch has changed
    /// under the panel.
    ///
    /// # This did not exist, and its absence was a live bug
    ///
    /// The plan for this phase first said to "route through the path a branch
    /// switch already uses". There is no such path. `switch_to_branch` spawns
    /// the git call and writes a status string; its own doc notes that the
    /// poller's next tick reflects the change. That is true of the branch
    /// name, the file list and the "Committed on Branch" section, which all
    /// arrive on the poll channel — but **not** of the commit graph, whose
    /// only two refresh sites are a completed commit op and the two manual
    /// refresh buttons. Neither fires on a branch switch, so the graph kept
    /// painting the previous branch's history indefinitely.
    ///
    /// So this is built here and called from both sites: branch-from-stash,
    /// and the plain branch switch that had the same gap all along.
    ///
    /// PR/CI state is dropped rather than refreshed. It belongs to the branch
    /// the user just left, and the 30 s throttle would otherwise keep showing
    /// it against the new one.
    pub(crate) fn refresh_after_branch_change(&mut self, cx: &mut Context<Self>) {
        self.commit_graph.update(cx, |g, cx| g.refresh(cx));
        self.has_open_pr = false;
        self.pr_merged = false;
        self.ci_checks.clear();
        self.pr_status_checked_branch = None;
        self.refresh_checks(cx);
        if let Some(poller) = &self.poller {
            poller.kick();
        }
        cx.notify();
    }

    /// Dispatch `repo.switch_branch(name)`. Failure surfaces via
    /// `CommitStatus::Failed("switch", err)` so the existing status row in the
    /// commit area shows the error without needing a separate toast surface.
    ///
    /// On success, `refresh_after_branch_change` runs. The poller's next tick
    /// does cover the branch name, the file list and the "Committed on
    /// Branch" section — but it has never covered the **commit graph**, which
    /// went on painting the previous branch's history until something else
    /// poked it. That was a pre-existing gap, found while wiring
    /// branch-from-stash; the fix belongs on both callers, not just the new
    /// one.
    pub(super) fn switch_to_branch(&mut self, name: String, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let commit_area = self.commit_area.clone();
        cx.spawn(async move |panel_weak, cx| {
            let result = repo.switch_branch(&name).await;
            let switched = result.is_ok();
            commit_area.update(cx, |area, cx| {
                write_branch_op_status(area, "switch", &name, result, cx);
            });
            if switched {
                let _ = panel_weak.update(cx, |panel, cx| panel.refresh_after_branch_change(cx));
            }
        })
        .detach();
    }

    /// Chain `create_branch(name, Some("HEAD"))` followed by
    /// `switch_branch(name)`. `create_branch` does NOT check out by
    /// itself; the second call is mandatory for the user-visible "Create
    /// branch from HEAD" semantics. If creation fails (name collision,
    /// invalid characters), we bail before the switch so we don't leave
    /// the user on an unintended branch.
    pub(super) fn create_branch_and_switch(&mut self, name: String, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let commit_area = self.commit_area.clone();
        cx.spawn(async move |_panel_weak, cx| {
            let create_result = repo.create_branch(&name, Some("HEAD")).await;
            if create_result.is_err() {
                commit_area.update(cx, |area, cx| {
                    write_branch_op_status(area, "create branch", &name, create_result, cx);
                });
                return;
            }
            let switch_result = repo.switch_branch(&name).await;
            commit_area.update(cx, |area, cx| {
                write_branch_op_status(area, "switch", &name, switch_result, cx);
            });
        })
        .detach();
    }
}

/// Write the result of a branch-level operation (switch / create-branch)
/// to the commit area's status row WITHOUT clobbering an in-flight
/// commit / push / pull / sync / fetch op's own status.
///
/// The commit area's `in_flight` AtomicBool is owned by `commit_ops`
/// for the lifetime of those ops. Reading it lets the branch-level path
/// detect "someone else's status row" and step around — writing
/// `CommitStatus::Idle` on a successful switch while a push is racing
/// would silently swallow whatever the push reports next. The error
/// case is also guarded so a slow switch failing AFTER a commit failed
/// doesn't overwrite the commit error.
///
/// When the lane is busy we still log the result at warn level so the
/// outcome isn't completely invisible to operators inspecting traces.
fn write_branch_op_status(
    area: &mut CommitArea,
    verb: &'static str,
    branch: &str,
    result: oximux_git::Result<()>,
    cx: &mut Context<CommitArea>,
) {
    let busy = area
        .in_flight
        .load(std::sync::atomic::Ordering::Relaxed);
    if busy {
        if let Err(ref err) = result {
            tracing::warn!(
                target: "oximux_app::source_control",
                verb = %verb,
                branch = %branch,
                error = %err,
                "branch op failed while commit-area was in flight — status row owned elsewhere",
            );
        }
        return;
    }
    match result {
        Ok(()) => area.status = CommitStatus::Idle,
        Err(err) => area.status = CommitStatus::Failed(verb.to_string(), err.to_string()),
    }
    cx.notify();
}

/// Move `current` (if present) to position 0 of `names`. Used to pin
/// the currently-checked-out branch to the top of the Switch-picker
/// list so the user can immediately see where they already are.
/// Pure for unit-test coverage; the side-effect-free shape also makes
/// it cheap to reuse from future call sites (e.g. a "recent branches"
/// pre-filter pass). `pub(crate)` rather than `pub` — the only callers
/// are the sibling `open_switch_picker` and the `#[cfg(test)]` block
/// below; nothing outside this crate should reach for it.
pub(crate) fn promote_current_branch(mut names: Vec<String>, current: Option<&str>) -> Vec<String> {
    if let Some(c) = current
        && let Some(pos) = names.iter().position(|n| n == c)
        && pos > 0
    {
        let cur = names.remove(pos);
        names.insert(0, cur);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::promote_current_branch;

    #[test]
    fn promote_current_to_front_when_present() {
        let names = vec!["main".into(), "feat-a".into(), "feat-b".into()];
        let out = promote_current_branch(names, Some("feat-a"));
        assert_eq!(out, vec!["feat-a", "main", "feat-b"]);
    }

    #[test]
    fn promote_no_op_when_current_already_at_front() {
        let names = vec!["main".into(), "feat-a".into()];
        let out = promote_current_branch(names, Some("main"));
        assert_eq!(out, vec!["main", "feat-a"]);
    }

    #[test]
    fn promote_no_op_when_current_not_in_list() {
        let names = vec!["main".into(), "feat-a".into()];
        let out = promote_current_branch(names, Some("missing"));
        assert_eq!(out, vec!["main", "feat-a"]);
    }

    #[test]
    fn promote_no_op_when_current_is_none() {
        let names = vec!["main".into(), "feat-a".into()];
        let out = promote_current_branch(names, None);
        assert_eq!(out, vec!["main", "feat-a"]);
    }

    #[test]
    fn promote_empty_input_returns_empty() {
        let out = promote_current_branch(Vec::<String>::new(), Some("main"));
        assert!(out.is_empty());
    }
}
