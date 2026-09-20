//! Adopting an untracked worktree, and the reverse.
//!
//! Adoption is a database act and nothing else: a `workspaces` row pointing at
//! the directory and branch that already exist. **Nothing on disk is
//! modified** — no move, no branch rename, no `.oximuxinclude` copy, no setup
//! script — because those exist to prepare a directory this app created, and
//! running them against one somebody else set up could overwrite their files.
//! The no-writes property is a test, not a convention (see below), and
//! adoption has its own insert path for exactly that reason: routing it
//! through `create_workspace_with_rollback` would reintroduce the risk.
//!
//! Adopted rows stay **un-vetted** until the user says otherwise (see
//! `WorkspaceRepo::is_unvetted`): the row menu withholds the script actions,
//! `Delete` skips the cleanup script, and `Review scripts…` opens the file the
//! marker is about. Same family as the unreviewed-ref boundary: OxiMux does
//! not run code the user has not looked at, on the user's behalf, by default.

use std::path::Path;

use gpui::{Context, Window};
use oximux_core::{Project, Workspace};
use oximux_git::derive_slug;
use oximux_storage::{StorageError, WorkspaceRepo};

use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmPrompt};
use crate::shell::toast::ToastKind;
use crate::shell::workspace::codename_ops::existing_slugs_across;
use crate::shell::workspace::discovery::UntrackedWorktree;
use crate::workspace_root::WorkspaceRoot;

/// The relative path every adopted worktree's scripts live at.
pub(crate) const SCRIPTS_FILE: &str = ".oximux/scripts.toml";

/// The name and slug an adopted row gets: the branch when there is one (it
/// is what the user called the work), else the directory's name; the slug is
/// derived from that and suffixed `-2`, `-3`, … past any slug already taken
/// across every open project, so adoption cannot collide with a row the
/// create path minted.
pub(crate) fn adoption_identity(u: &UntrackedWorktree, existing_slugs: &[String]) -> (String, String) {
    let name = u
        .branch
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| u.dir_name());
    let base = derive_slug(&name);
    let mut slug = base.clone();
    let mut n = 2;
    while existing_slugs.iter().any(|s| s == &slug) {
        slug = format!("{base}-{n}");
        n += 1;
    }
    (name, slug)
}

/// Insert the row for `u` — the whole of adoption. Pure database; see the
/// module docs for why it must stay that way.
pub(crate) fn adopt_row(
    repo: &WorkspaceRepo,
    projects: &[Project],
    u: &UntrackedWorktree,
) -> Result<Workspace, StorageError> {
    let existing = existing_slugs_across(repo, projects);
    let (name, slug) = adoption_identity(u, &existing);
    repo.adopt(
        &u.project_id,
        &name,
        &slug,
        u.branch.as_deref().unwrap_or(""),
        &u.path.to_string_lossy(),
    )
}

impl WorkspaceRoot {
    /// `Adopt` on an untracked row: insert the row, drop the worktree from the
    /// untracked group at once (the next scan would, but the click should),
    /// and say what adoption did not do.
    pub(crate) fn adopt_untracked(&mut self, u: UntrackedWorktree, cx: &mut Context<Self>) {
        let projects = self.app_state.recent_projects.clone();
        match adopt_row(&self.app_state.workspace_repo, &projects, &u) {
            Ok(row) => {
                if let Some(list) = self.untracked_by_project.get_mut(&u.project_id) {
                    list.retain(|x| x.path != u.path);
                }
                // A scan that started before this adoption would report the
                // worktree as still untracked; the round discards results
                // from before the epoch moved and scans again.
                self.adoption_epoch = self.adoption_epoch.wrapping_add(1);
                self.discovery_due = true;
                self.push_toast(
                    ToastKind::Info,
                    format!(
                        "Adopted \u{201c}{}\u{201d}. Its scripts stay off until you review them.",
                        row.name
                    ),
                    cx,
                );
                self.mark_rail_dirty(cx);
                cx.notify();
            }
            Err(err) => {
                crate::shell::toast::toast_op_error(
                    cx,
                    &format!("Adopt \u{201c}{}\u{201d}", u.dir_name()),
                    &err.to_string(),
                );
            }
        }
    }

    /// `Stop tracking`: remove the row and leave the worktree alone. The
    /// confirmation says the worktree will reappear as untracked, so the act
    /// does not read as a failed delete.
    pub(crate) fn request_stop_tracking_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = self.app_state.workspace_repo.clone();
        let weak = cx.weak_entity();
        let target = workspace.clone();
        let on_confirm: ConfirmCallback = std::rc::Rc::new(move |_window, cx| {
            let outcome = repo.delete(&target.id);
            let _ = weak.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => {
                        this.push_toast(
                            ToastKind::Info,
                            format!(
                                "Stopped tracking \u{201c}{}\u{201d}. The worktree is still on disk.",
                                target.name
                            ),
                            cx,
                        );
                        // Let the next round find it again without waiting
                        // for the discovery cadence, and discard a scan that
                        // ran against the row that no longer exists.
                        this.adoption_epoch = this.adoption_epoch.wrapping_add(1);
                        this.discovery_due = true;
                    }
                    Err(err) => crate::shell::toast::toast_op_error(
                        cx,
                        &format!("Stop tracking \u{201c}{}\u{201d}", target.name),
                        &err.to_string(),
                    ),
                }
                this.mark_rail_dirty(cx);
            });
        });
        let prompt = ConfirmPrompt {
            title: "Stop tracking workspace".into(),
            body: format!(
                "Removes \u{201c}{}\u{201d} from OxiMux only. The worktree at {} stays on disk, \
                 untouched, and will show up again under Untracked.",
                workspace.name, workspace.worktree_path
            )
            .into(),
            on_confirm,
            confirm_label: Some("Stop Tracking".into()),
            on_cancel: None,
            secondary: None,
        };
        // Refusal means a live prompt is already up; nothing here to undo.
        let _ = self.mount_confirm_dialog(prompt, window, cx);
    }

    /// The per-project toggle: hide (or show) the untracked group for a repo
    /// where it is noise. Persisted; the next rail gather reads it.
    pub(crate) fn set_hide_untracked_for_project(
        &mut self,
        project_id: &str,
        hide: bool,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.app_state.project_repo.set_hide_untracked(project_id, hide) {
            crate::shell::toast::toast_op_error(cx, "Hide untracked worktrees", &err.to_string());
            return;
        }
        // The gather re-reads the preference, but the next round's scan
        // consults the cached set before that gather lands — so keep the
        // cache in step here, or un-hiding would be a no-op for a cadence.
        if hide {
            self.rail_hidden_untracked.insert(project_id.to_string());
            self.untracked_by_project.remove(project_id);
        } else {
            self.rail_hidden_untracked.remove(project_id);
            self.discovery_due = true;
        }
        self.mark_rail_dirty(cx);
        cx.notify();
    }

    /// `Review scripts…`: open the adopted worktree's `.oximux/scripts.toml`
    /// in an editor tab of the row's OWN project, so the user can read what
    /// the withheld actions would run. A worktree with no such file has
    /// nothing to review, and says so.
    pub(crate) fn review_workspace_scripts(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = Path::new(&workspace.worktree_path).join(SCRIPTS_FILE);
        if !path.is_file() {
            self.push_toast(
                ToastKind::Info,
                format!(
                    "\u{201c}{}\u{201d} has no {SCRIPTS_FILE} — nothing to review. You can mark its scripts reviewed.",
                    workspace.name
                ),
                cx,
            );
            return;
        }
        // The ROW's project's panes, as `run_workspace_script` does: a project
        // that has never been activated in this window has no panes yet, so
        // activate it first; a row whose project is not open gets a decline,
        // never another project's pane group.
        let mut panes = self.project_panes_by_project.get(&workspace.project_id).cloned();
        if panes.is_none() {
            let Some(project) = crate::shell::workspace::workspace_ops::resolve_project_for_workspace(
                &self.app_state.recent_projects,
                &workspace,
            ) else {
                tracing::warn!(
                    project_id = %workspace.project_id,
                    "review_workspace_scripts: row's project is not open"
                );
                return;
            };
            self.set_active_project(project, window, cx);
            panes = self.project_panes_by_project.get(&workspace.project_id).cloned();
        }
        if let Some(panes) = panes {
            panes.update(cx, |p, cx| {
                p.open_or_activate_editor_tab(path, window, cx);
            });
        }
    }

    /// `Mark scripts reviewed`: the explicit act that clears the marker and
    /// lets the row's script actions appear.
    pub(crate) fn mark_workspace_scripts_reviewed(
        &mut self,
        workspace: Workspace,
        cx: &mut Context<Self>,
    ) {
        match self.app_state.workspace_repo.mark_scripts_reviewed(&workspace.id) {
            Ok(()) => self.push_toast(
                ToastKind::Info,
                format!(
                    "Scripts for \u{201c}{}\u{201d} marked reviewed. Run actions are available.",
                    workspace.name
                ),
                cx,
            ),
            Err(err) => crate::shell::toast::toast_op_error(
                cx,
                &format!("Mark scripts reviewed for \u{201c}{}\u{201d}", workspace.name),
                &err.to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::{ProjectRepo, open_memory};
    use std::path::PathBuf;

    fn untracked(project_id: &str, path: &Path, branch: Option<&str>) -> UntrackedWorktree {
        UntrackedWorktree {
            project_id: project_id.into(),
            path: path.to_path_buf(),
            branch: branch.map(str::to_string),
        }
    }

    #[test]
    fn the_branch_names_the_row_and_the_slug_dodges_taken_ones() {
        let u = untracked("p", Path::new("/wt/Feature Work"), Some("feat/login-flow"));
        let (name, slug) = adoption_identity(&u, &[]);
        assert_eq!(name, "feat/login-flow");
        assert_eq!(slug, derive_slug("feat/login-flow"));
        let taken = vec![slug.clone(), format!("{slug}-2")];
        let (_, slug3) = adoption_identity(&u, &taken);
        assert_eq!(slug3, format!("{slug}-3"));
    }

    #[test]
    fn a_detached_worktree_is_named_after_its_directory() {
        let u = untracked("p", Path::new("/wt/spike-42"), None);
        let (name, slug) = adoption_identity(&u, &[]);
        assert_eq!(name, "spike-42");
        assert_eq!(slug, "spike-42");
    }

    /// Every file under the directory, with size and modification time — the
    /// shape a write of any kind would change. Directories count by presence
    /// only: NTFS updates a directory's own mtime lazily after a write inside
    /// it, so the `.oximux` entry's stamp can move between two snapshots with
    /// nothing having been written in between (seen on Windows CI). A file
    /// created, removed, or rewritten still changes the list.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    out.push((entry.path(), 0, std::time::UNIX_EPOCH));
                    walk(&entry.path(), out);
                } else {
                    out.push((entry.path(), meta.len(), meta.modified().unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }

    /// The property adoption exists to keep: the directory is byte-for-byte
    /// as it was — no include copied, no setup run, nothing created.
    #[test]
    fn adoption_writes_nothing_to_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("someone-elses-worktree");
        std::fs::create_dir_all(wt.join(".oximux")).unwrap();
        std::fs::write(wt.join(".oximux").join("scripts.toml"), "setup = \"rm -rf /\"\n").unwrap();
        std::fs::write(wt.join("theirs.txt"), "do not touch\n").unwrap();
        let before = snapshot(&wt);

        let db = open_memory().unwrap();
        let projects = ProjectRepo::new(db.clone());
        let repo = WorkspaceRepo::new(db);
        let project = projects.insert("Acme", "/repo", "main").unwrap();
        let u = untracked(&project.id, &wt, Some("theirs"));
        let row = adopt_row(&repo, std::slice::from_ref(&project), &u).unwrap();

        assert_eq!(snapshot(&wt), before, "adoption must not write to the worktree");
        assert_eq!(row.worktree_path, wt.to_string_lossy());
        assert!(!row.branch_minted);
        assert!(repo.is_unvetted(&row.id).unwrap(), "adopted rows start un-vetted");
    }

    #[test]
    fn adopting_twice_under_one_slug_yields_distinct_rows() {
        let db = open_memory().unwrap();
        let projects = ProjectRepo::new(db.clone());
        let repo = WorkspaceRepo::new(db);
        let project = projects.insert("Acme", "/repo", "main").unwrap();
        let a = untracked(&project.id, Path::new("/wt/a"), Some("topic"));
        let b = untracked(&project.id, Path::new("/wt/b"), Some("topic"));
        let ra = adopt_row(&repo, std::slice::from_ref(&project), &a).unwrap();
        let rb = adopt_row(&repo, std::slice::from_ref(&project), &b).unwrap();
        assert_eq!(ra.slug, "topic");
        assert_eq!(rb.slug, "topic-2");
    }
}
