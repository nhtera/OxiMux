//! `ProjectPanes` — workspace-level container for one project's pane
//! groups.
//!
//! Replaces the old `WorkspaceTabs`. The workspace owns one
//! `ProjectPanes` per open project; each holds:
//!
//! - a `PaneGroupManager` (pure-data layout tree)
//! - a `HashMap<PaneGroupId, Entity<PaneGroup>>` (live entities)
//!
//! Splits at the workspace level create new sibling pane groups via the
//! manager. File-open and split actions target the active group, looked
//! up through `manager.active_group_id()`.

mod render;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, Subscription, Window,
};
use oximux_agents::CliRuntime;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::pane_group::PaneGroup;
use crate::shell::pane_group_manager::{
    CloseGroupError, GroupSplitOutcome, PaneGroupManager,
};
use crate::shell::pane_tree::{Axis, PaneGroupId, SplitInsert};

pub struct ProjectPanes {
    manager: PaneGroupManager,
    groups: HashMap<PaneGroupId, Entity<PaneGroup>>,
    /// One observer per group entity so layout / focus changes inside a
    /// group bubble up to the workspace render. Dropped on group close.
    _observers: HashMap<PaneGroupId, Subscription>,
    focus_handle: FocusHandle,
    cwd: PathBuf,
    theme: Theme,
    density: Density,
    typography: Typography,
    cli_runtime: Arc<CliRuntime>,
}

impl ProjectPanes {
    /// Seed a project's panes with a single empty group. The caller
    /// usually follows up with `open_terminal_tab_in_active_group` to
    /// drop in the initial shell.
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let manager = PaneGroupManager::new();
        let initial_id = manager.active_group_id();
        let cwd_for_group = cwd.clone();
        let typography_for_group = typography.clone();
        let runtime_for_group = cli_runtime.clone();
        let group = cx.new(|cx| {
            PaneGroup::new(
                cwd_for_group,
                theme,
                density,
                typography_for_group,
                runtime_for_group,
                cx,
            )
        });
        let observer = cx.observe(&group, |_this, _g, cx| cx.notify());
        let mut groups = HashMap::new();
        groups.insert(initial_id, group);
        let mut observers = HashMap::new();
        observers.insert(initial_id, observer);
        Self {
            manager,
            groups,
            _observers: observers,
            focus_handle: cx.focus_handle(),
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
        }
    }

    pub fn manager(&self) -> &PaneGroupManager {
        &self.manager
    }

    pub fn active_group(&self) -> Option<Entity<PaneGroup>> {
        self.groups.get(&self.manager.active_group_id()).cloned()
    }

    pub fn group(&self, id: PaneGroupId) -> Option<Entity<PaneGroup>> {
        self.groups.get(&id).cloned()
    }

    /// Update the active group based on a focus event. Caller drives
    /// this from mouse-down handlers inside individual `PaneGroup`
    /// containers.
    pub fn set_active_group(
        &mut self,
        id: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.manager.set_active(id) {
            if let Some(group) = self.groups.get(&id) {
                group.update(cx, |g, cx| g.focus_active(window, cx));
            }
            cx.notify();
        }
    }

    /// Split the active group along `axis` (with insert direction). The
    /// new sibling spawns a fresh Terminal tab. Returns the new group
    /// id on success; `None` if the split mutation failed.
    pub fn split_active_group(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneGroupId> {
        let GroupSplitOutcome { new_group, .. } =
            self.manager.split_active_group(axis, insert)?;
        let cwd = self.cwd.clone();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let runtime = self.cli_runtime.clone();
        let group = cx.new(|cx| PaneGroup::new(cwd, theme, density, typography, runtime, cx));
        // Seed with one terminal tab so the new group is usable.
        group.update(cx, |g, cx| {
            g.open_terminal_tab(window, cx);
        });
        let observer = cx.observe(&group, |_this, _g, cx| cx.notify());
        self.groups.insert(new_group, group);
        self._observers.insert(new_group, observer);
        // The manager has already moved focus to the new group; pull
        // platform focus over as well.
        if let Some(group) = self.groups.get(&new_group) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
        Some(new_group)
    }

    /// Close the active group. Returns the closed id on success.
    /// `Err(LastGroup)` if it was the only group — caller may surface
    /// a HUD message but the workspace stays put.
    pub fn close_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaneGroupId, CloseGroupError> {
        let closed = self.manager.close_active_group()?;
        self.groups.remove(&closed);
        self._observers.remove(&closed);
        // Re-focus the newly-active group (manager already updated its
        // pointer).
        if let Some(group) = self.groups.get(&self.manager.active_group_id()) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
        Ok(closed)
    }

    /// Open a file in the active group (delegates to
    /// `PaneGroup::open_or_activate_editor_tab`).
    pub fn open_file_in_active_group(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.open_or_activate_editor_tab(path, window, cx);
        });
    }

    /// Spawn a fresh terminal tab in the active group.
    pub fn open_terminal_tab_in_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.open_terminal_tab(window, cx);
        });
    }

    /// File path of the editor tab currently focused in the active
    /// group, or `None` if no editor leaf is focused. Drives the
    /// Files-sidebar active-file highlight.
    pub fn active_editor_path(&self, cx: &gpui::App) -> Option<PathBuf> {
        self.active_group()
            .and_then(|g| g.read(cx).active_editor_path(cx))
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }
}

impl Focusable for ProjectPanes {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
