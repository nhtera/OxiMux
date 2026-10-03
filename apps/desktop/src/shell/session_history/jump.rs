//! "Jump to open tab" for a full-text hit: the agent tabs open right now,
//! with the conversation id each agent has reported, gathered when the picker
//! opens. Activation goes back through `WorkspaceRoot` on the same path the
//! search palette uses.

use gpui::{App, Context, Window};

use super::fulltext::LiveTab;
use super::{SessionHistoryEvent, SessionHistoryModal};
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::PaneGroupTabKind;
use crate::workspace_root::WorkspaceRoot;

/// Every agent tab (cockpit or chat) in the mounted projects.
pub fn live_tabs(root: &WorkspaceRoot, cx: &App) -> Vec<LiveTab> {
    let mut out = Vec::new();
    for (project_id, panes) in &root.project_panes_by_project {
        for group in panes.read(cx).group_entities() {
            for (_, tab) in group.read(cx).visible_tabs() {
                let (provider_session, cwd) = match (&tab.kind, &tab.content) {
                    (PaneGroupTabKind::Agent { status_rx, worktree_path, .. }, _) => (
                        crate::session_restore::agent_resume::provider_session_from_snapshot(&status_rx.borrow()),
                        worktree_path.display().to_string(),
                    ),
                    (PaneGroupTabKind::AgentChat { cwd, .. }, PaneContent::AgentChat(view)) => {
                        (view.read(cx).session_id().map(str::to_string), cwd.display().to_string())
                    }
                    _ => continue,
                };
                out.push(LiveTab {
                    project_id: project_id.clone(),
                    uid: tab.uid,
                    provider_session,
                    cwd,
                    title: tab.custom_title.clone().unwrap_or_else(|| tab.label.clone()).to_string(),
                });
            }
        }
    }
    out
}

impl SessionHistoryModal {
    /// Close and ask the workspace to focus the open tab running the selected
    /// hit's session. Returns `false` (doing nothing) when there is none.
    pub(super) fn jump_to_live_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.selected_live_tab() else { return false };
        self.close(cx);
        cx.emit(SessionHistoryEvent::JumpToTab { project_id: tab.project_id, uid: tab.uid });
        true
    }
}
