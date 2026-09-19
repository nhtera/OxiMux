//! Keeping the rail's active workspace in step with what the user is looking at.
//!
//! The rail's selection has two sources that can disagree. A click on a rail
//! row sets it deliberately and, by design, does NOT switch tabs ("selection
//! set, spawn deferred"). Everything else — focusing a tab, clicking a card on
//! the agents dashboard — moves what is on screen without touching the rail.
//! This module reconciles the two: the selection follows focus when focus
//! moves, and a deliberate selection survives until it does.

use gpui::{Context, Window};
use oximux_core::Workspace;

use crate::workspace_root::WorkspaceRoot;

/// The rail row a focus change should select, or `None` to leave the selection
/// alone.
///
/// Two rules, both load-bearing:
///
/// 1. **Only on change.** `last` is the focus this already synced to. A rail
///    click selects a workspace *without* switching tabs ("selection set,
///    spawn deferred"), so re-deriving on every refresh would snap that
///    selection back to the focused tab's workspace and make the click look
///    broken.
/// 2. **Only when it resolves.** Returning `None` for an unknown worktree lets
///    the caller leave its baseline untouched and retry. The rail's rows are
///    filled by a background pass, so an early refresh can see the right
///    worktree and no rows yet — adopting the baseline there would mark the
///    tab synced and suppress the sync for it permanently.
fn focus_follow_target<'a>(
    last: Option<&(String, String)>,
    current: &(String, String),
    rows: &'a [Workspace],
) -> Option<&'a str> {
    if last == Some(current) {
        return None;
    }
    rows.iter()
        .find(|w| w.worktree_path == current.1)
        .map(|w| w.id.as_str())
}

impl WorkspaceRoot {
    /// Select the workspace an agents-dashboard card belongs to and reveal it
    /// in the rail.
    ///
    /// `workspace_key` is the row's own key (a `workspaces.id`, or
    /// `primary:<project_id>` for the repo-root row), which every dashboard
    /// card carries. That matters for a HISTORY row: it has no live session,
    /// so the focus calls beside this one are no-ops for it, and selecting its
    /// workspace is the only thing the card can do — without this, clicking a
    /// finished session does nothing at all.
    ///
    /// The reveal is what makes the selection visible: the Agents page is
    /// covering the workspace list, so `scroll_to_active` returning the rail
    /// to the list is the feedback that the click landed.
    pub(crate) fn reveal_agent_row_workspace(
        &mut self,
        workspace_key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The dashboard is cross-project, so resolve the owning project from
        // the key rather than assuming the active one.
        let owner = self.rail_workspaces_by_project.iter().find_map(|(pid, rows)| {
            rows.iter()
                .any(|w| w.id == workspace_key)
                .then(|| pid.clone())
        });
        let Some(project_id) = owner else {
            return;
        };
        if self.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str()) {
            let Some(project) = self
                .app_state
                .recent_projects
                .iter()
                .find(|p| p.id == project_id)
                .cloned()
            else {
                return;
            };
            self.set_active_project(project, window, cx);
        }
        self.select_rail_workspace(&project_id, workspace_key);
        // The focused group is unchanged by this, so pin the focus-follow
        // baseline to it — otherwise the next refresh would treat the group
        // as newly focused and overwrite the selection we just made.
        self.pin_focus_baseline(cx);
        self.left_rail
            .update(cx, |rail, cx| rail.scroll_to_active(window, cx));
        cx.notify();
    }

    /// Record the focused pane group as already-synced without touching the
    /// selection, so a deliberate selection made elsewhere in this frame is
    /// not undone by [`Self::sync_rail_selection_to_focus`] on the next one.
    pub(crate) fn pin_focus_baseline(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project.as_ref().map(|p| p.id.clone()) else {
            return;
        };
        if let Some(cwd) = self
            .active_project_panes()
            .and_then(|panes| panes.read(cx).active_group())
            .map(|group| {
                group
                    .read(cx)
                    .active_tab_worktree()
                    .to_string_lossy()
                    .into_owned()
            })
        {
            self.last_focused_group = Some((project_id, cwd));
        }
    }

    /// Move the rail's active row onto the workspace owning the focused pane
    /// group, so the highlight follows the terminal the user is actually
    /// looking at instead of whatever was last clicked in the rail.
    ///
    /// Fires on CHANGE of the focused group only. A rail click selects a
    /// workspace *without* switching tabs ("selection set, spawn deferred"),
    /// so re-deriving the selection on every refresh would snap it straight
    /// back to the focused group's workspace and make those clicks look
    /// broken. Tracking the last-synced group means a deliberate selection
    /// survives until the user actually moves focus.
    ///
    /// Deliberately does NOT `record_nav`: following focus is not a workspace
    /// switch the user asked for, and recording it would fill the Cmd+Alt+←/→
    /// history with an entry per tab click.
    ///
    /// Matching is by the active tab's worktree against the rail rows' worktree
    /// paths — the same best-effort mapping the bell-banner click uses. An
    /// agent tab names its own worktree; any other tab falls back to its
    /// group's cwd. A terminal can `cd` anywhere, but the group cwd is its
    /// workspace.
    pub(crate) fn sync_rail_selection_to_focus(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project.as_ref().map(|p| p.id.clone()) else {
            self.last_focused_group = None;
            return;
        };
        let Some(cwd) = self
            .active_project_panes()
            .and_then(|panes| panes.read(cx).active_group())
            .map(|group| {
                group
                    .read(cx)
                    .active_tab_worktree()
                    .to_string_lossy()
                    .into_owned()
            })
        else {
            return;
        };
        let focused = (project_id.clone(), cwd);
        let rows = self
            .rail_workspaces_by_project
            .get(&project_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let Some(id) =
            focus_follow_target(self.last_focused_group.as_ref(), &focused, rows).map(str::to_owned)
        else {
            return;
        };
        self.last_focused_group = Some(focused);
        self.active_workspace_id = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::focus_follow_target;
    use oximux_core::Workspace;

    fn workspace(id: &str, path: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: "p".to_string(),
            branch_minted: false,
            name: id.to_string(),
            slug: id.to_string(),
            branch: "main".to_string(),
            worktree_path: path.to_string(),
            status: "active".to_string(),
            created_at: "2026-06-24T00:00:00Z".to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    fn focus(project: &str, path: &str) -> (String, String) {
        (project.to_string(), path.to_string())
    }

    /// A deliberate rail click selects a workspace without switching tabs, so
    /// an unchanged focus must not re-derive the selection over the top of it.
    #[test]
    fn unchanged_focus_selects_nothing() {
        let rows = vec![workspace("w1", "/wt/one")];
        let here = focus("p", "/wt/one");
        assert_eq!(focus_follow_target(Some(&here), &here, &rows), None);
    }

    #[test]
    fn a_moved_focus_selects_the_row_owning_the_new_worktree() {
        let rows = vec![workspace("w1", "/wt/one"), workspace("w2", "/wt/two")];
        let was = focus("p", "/wt/one");
        let now = focus("p", "/wt/two");
        assert_eq!(focus_follow_target(Some(&was), &now, &rows), Some("w2"));
    }

    /// The rail's rows arrive from a background pass, so an early refresh can
    /// see the right worktree and no rows yet. Answering `None` is what lets
    /// the caller keep its baseline and retry — answering with a baseline and
    /// no row would suppress the sync for that tab permanently.
    #[test]
    fn an_unresolvable_worktree_selects_nothing_so_the_caller_can_retry() {
        assert_eq!(focus_follow_target(None, &focus("p", "/wt/one"), &[]), None);
        let rows = vec![workspace("w1", "/wt/one")];
        assert_eq!(
            focus_follow_target(None, &focus("p", "/elsewhere"), &rows),
            None
        );
    }

    /// First sync of a window: no baseline yet, so the focused tab's workspace
    /// is adopted rather than leaving the rail with nothing selected.
    #[test]
    fn the_first_sync_adopts_the_focused_worktree() {
        let rows = vec![workspace("w1", "/wt/one")];
        assert_eq!(
            focus_follow_target(None, &focus("p", "/wt/one"), &rows),
            Some("w1")
        );
    }

    /// Same worktree string, different project: still a change. Two projects
    /// can hold checkouts at paths that compare equal only by accident.
    #[test]
    fn a_project_switch_counts_as_a_focus_change() {
        let rows = vec![workspace("w1", "/wt/one")];
        let was = focus("p-old", "/wt/one");
        let now = focus("p-new", "/wt/one");
        assert_eq!(focus_follow_target(Some(&was), &now, &rows), Some("w1"));
    }
}
