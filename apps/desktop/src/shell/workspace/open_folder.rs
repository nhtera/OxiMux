//! `oximux open`'s landing point on the workspace root: open a folder as the
//! active project, the registration a folder dropped on the left rail gets, and
//! select the linked worktree it was in when the rail lists one. The folder
//! arrives already resolved to its project (`remote_control::open_bridge`).

use gpui::{Context, Window};
use oximux_core::Project;

use crate::workspace_root::WorkspaceRoot;

impl WorkspaceRoot {
    /// Open `root` as the active project for `oximux open` — the registration
    /// a dropped folder gets — and, when the folder was a linked `worktree` of
    /// it that the rail lists, select that worktree too. `done` hears the
    /// project and the worktree selected once both are applied, or why not.
    pub(crate) fn open_folder(
        &mut self,
        root: std::path::PathBuf,
        worktree: Option<std::path::PathBuf>,
        done: impl FnOnce(Result<(Project, Option<String>), String>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            // Off the foreground, as the drop path does: it runs git.
            let default_branch =
                crate::shell::workspace::project_picker::detect_default_branch(&root).await;
            let opened = this.update_in(cx, |this, window, cx| {
                let project = this
                    .register_and_activate(&root, &default_branch, window, cx)
                    .map_err(|err| format!("could not register {}: {err}", root.display()))?;
                let selected = worktree.and_then(|wt| this.select_listed_worktree(&project, &wt, window, cx));
                Ok((project, selected))
            });
            done(opened.unwrap_or_else(|_| Err("the window closed before the project opened".into())));
        })
        .detach();
    }

    /// Select the rail's row for the linked worktree at `path`, if `project`
    /// has a live one; its path when it did.
    fn select_listed_worktree(
        &mut self,
        project: &Project,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let rows = self.app_state.workspace_repo.list_for_project(&project.id).ok()?;
        let same = |row: &str| std::path::Path::new(row).canonicalize().ok().as_deref() == Some(path);
        let row = rows.into_iter().find(|row| row.archived_at.is_none() && same(&row.worktree_path))?;
        let selected = row.worktree_path.clone();
        self.activate_workspace(row, window, cx);
        Some(selected)
    }
}
