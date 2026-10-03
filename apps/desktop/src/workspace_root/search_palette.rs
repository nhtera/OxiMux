//! Root glue for the search palette (left-rail Search row, ⌘J) and the
//! session-history picker's search context: open over a fresh snapshot, hand
//! focus back when closed, and resolve activations against live state — the
//! snapshot may be stale by then, so a missing target is a toast, never a
//! panic.

use gpui::{App, Context, Window};

use crate::shell::search_palette::SearchPaletteEvent;
use crate::shell::search_palette::model::Target;
use crate::shell::search_palette::sources;
use crate::shell::session_history::{HistoryContext, SessionHistoryEvent};
use crate::shell::settings_modal::SettingsPane;
use crate::shell::toast::{ToastKind, toast};

use super::WorkspaceRoot;

impl WorkspaceRoot {
    /// Open the palette (a second press while open closes it).
    pub(crate) fn open_search_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_pending(cx) {
            return;
        }
        if self.search_palette.read(cx).is_open() {
            self.search_palette.update(cx, |p, cx| p.close(cx));
            return;
        }
        // Esc hands focus back to where the user was — unless that was
        // another overlay, which is about to close: its input would be a dead
        // focus target, so fall back to the root instead.
        let restore = if self.any_modal_overlay_open(cx) { None } else { window.focused(cx) };
        // Mutex with every other full-window overlay (close-then-open).
        self.close_modal_overlays(cx);
        let snapshot = sources::snapshot(self, cx);
        self.search_palette.update(cx, |p, cx| p.open(snapshot, restore, window, cx));
    }

    pub(crate) fn on_search_palette_event(
        &mut self,
        event: &SearchPaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SearchPaletteEvent::Closed { restore: Some(handle) } => window.focus(handle, cx),
            // Activation lands its own (deferred) focus; holding the root in
            // between keeps global shortcuts dispatching.
            SearchPaletteEvent::Closed { restore: None } => self.focus_handle.focus(window, cx),
            SearchPaletteEvent::Activate(target) => self.activate_search_target(target.clone(), window, cx),
        }
    }

    /// Also the session-history picker's "jump to open tab".
    pub(crate) fn activate_search_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Tab { project_id, uid } => {
                let found = self.switch_to_project(&project_id, window, cx)
                    && self
                        .project_panes_by_project
                        .get(&project_id)
                        .cloned()
                        .is_some_and(|panes| panes.update(cx, |p, cx| p.activate_tab_by_uid(uid, window, cx)));
                if !found {
                    toast(cx, ToastKind::Info, "That tab no longer exists");
                }
            }
            Target::Worktree { workspace_id, project_id, worktree_path } => {
                // The snapshot may predate an archive or delete in another
                // window: activate only a row the rail still lists.
                let live = self
                    .rail_workspaces_by_project
                    .get(&project_id)
                    .is_some_and(|rows| rows.iter().any(|w| w.id == workspace_id && w.archived_at.is_none()));
                if live {
                    self.activate_workspace_by_ref(workspace_id, project_id, worktree_path, window, cx);
                } else {
                    toast(cx, ToastKind::Info, "That worktree no longer exists");
                }
            }
            Target::Project { project_id } => {
                if !self.switch_to_project(&project_id, window, cx) {
                    toast(cx, ToastKind::Info, "That project is no longer open");
                }
            }
            Target::Setting(pane) => {
                self.settings_modal.update(cx, |m, cx| m.open_to_pane(pane, window, cx));
            }
            // Focus was handed back to the surface the user came from (see
            // `SearchPalette::run`), so pane-scoped actions resolve there.
            Target::Action(make) => window.dispatch_action(make(), cx),
            Target::CreateWorktree(name) => self.open_workspace_create(Some(&name), window, cx),
        }
        cx.notify();
    }

    /// What the session-history picker opens with: the project's launch
    /// dirs, the active tab's worktree, and the open agent tabs.
    pub(crate) fn session_history_context(&self, cx: &App) -> HistoryContext {
        let worktree_path = self
            .active_project_panes()
            .and_then(|panes| panes.read(cx).active_group())
            .map(|group| group.read(cx).active_tab_worktree().display().to_string());
        HistoryContext {
            project_paths: self.active_project_scope_paths(),
            worktree_path,
            live_tabs: crate::shell::session_history::jump::live_tabs(self, cx),
        }
    }

    pub(crate) fn on_session_history_event(
        &mut self,
        event: &SessionHistoryEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionHistoryEvent::Closed => self.focus_handle.focus(window, cx),
            SessionHistoryEvent::JumpToTab { project_id, uid } => {
                let target = Target::Tab { project_id: project_id.clone(), uid: *uid };
                self.activate_search_target(target, window, cx);
            }
            SessionHistoryEvent::OpenSessionSearchSettings => {
                self.settings_modal.update(cx, |m, cx| m.open_to_pane(SettingsPane::Agents, window, cx));
            }
        }
    }

    /// Make `project_id` the active project. `false` when it is no longer in
    /// the recent list.
    fn switch_to_project(&mut self, project_id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.active_project.as_ref().is_some_and(|p| p.id == project_id) {
            return true;
        }
        let Some(project) = self.app_state.recent_projects.iter().find(|p| p.id == project_id).cloned() else {
            return false;
        };
        self.set_active_project(project, window, cx);
        true
    }
}
