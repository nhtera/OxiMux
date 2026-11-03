//! Building a project's right sidebar, and rebuilding it once a plain folder
//! becomes a git repo.
//!
//! Lifted out of `workspace_ops.rs` (over the file-size soft cap). A sidebar
//! is built once per project and cached in `right_sidebar_by_project`; its
//! git-ness is decided by that one `Repository::open`. Nothing else ever
//! re-asks, so a project opened before `git init` would keep its
//! Explorer-only sidebar — no Source Control tab — until the app quit.
//! [`WorkspaceRoot::rebuild_sidebar_after_git_init`] is the re-ask.

use std::path::PathBuf;

use gpui::{AppContext, Context, Window};
use oximux_git::Repository;

use crate::shell::right_sidebar::has_git_dir;
use crate::shell::right_sidebar::tab::RightTab;
use crate::shell::workspace::workspace_ops::refocus_active_pane;
use crate::workspace_root::WorkspaceRoot;

impl WorkspaceRoot {
    /// Build `project_id`'s sidebar against `project_root`, cache it, and
    /// show it if that project is still the active one when the build lands.
    pub(crate) fn build_right_sidebar(
        &mut self,
        project_id: String,
        project_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |weak, cx| {
            // Repo presence is optional now — Repository::open may fail for
            // non-git folders. Build the sidebar in either mode: with git
            // (Source Control + Explorer + Search) or without (Explorer +
            // Search only). The Explorer + Search tabs always work from
            // `root_path` regardless of git status.
            let repo = match Repository::open(&project_root).await {
                Ok(r) => Some(r),
                Err(err) => {
                    tracing::info!(
                        ?err,
                        path = %project_root.display(),
                        "non-git project; building file-explorer-only sidebar"
                    );
                    None
                }
            };
            let _ = weak.update_in(cx, |this, window, cx| {
                this.install_right_sidebar(project_id, project_root, repo, None, window, cx);
            });
        })
        .detach();
    }

    /// Mint the sidebar for `repo`, cache it, and show it if its project is
    /// still the active one.
    ///
    /// `replacing` is the tab of the sidebar this one replaces in place (the
    /// `git init` rebuild): the new one opens on that tab and focus is left
    /// alone, since no user action asked for either to move. `None` is a
    /// project activation — default tab, focus back to the active pane.
    fn install_right_sidebar(
        &mut self,
        project_id: String,
        project_root: PathBuf,
        repo: Option<Repository>,
        replacing: Option<RightTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        // Carry the previous sidebar's open/collapsed state across
        // the rebuild — the right column must stay where the user
        // left it, not snap back open on every project switch. No
        // sidebar yet = first activation of this window, which starts
        // collapsed (the "default-collapsed on app boot" behavior).
        let prior_open = self
            .right_sidebar
            .as_ref()
            .map(|s| s.read(cx).open)
            .unwrap_or(false);
        let weak = cx.weak_entity();
        let on_open =
            crate::workspace_root::WorkspaceRoot::build_on_open_file_callback(weak.clone());
        let on_open_diff = repo.as_ref().map(|r| {
            crate::workspace_root::WorkspaceRoot::build_on_open_diff_callback(
                weak.clone(),
                r.clone(),
            )
        });
        let on_open_search_hit =
            crate::workspace_root::WorkspaceRoot::build_on_open_search_hit_callback(weak.clone());
        let on_query =
            crate::workspace_root::WorkspaceRoot::build_on_query_active_path_callback(weak);
        let worktree_settings_repo =
            Some(self.app_state.worktree_settings_repo.clone());
        // Phase 13: load persisted panel width clamped against
        // the current window so a too-large persisted value
        // can't overflow a newly-smaller window. The settings
        // repo is shared app-wide via the same DB handle.
        let window_width = f32::from(window.bounds().size.width);
        let settings_repo = self.app_state.settings_repo.clone();
        let initial_width = gpui::px(
            crate::scm_layout_settings::load_panel_width(&settings_repo, window_width),
        );
        let layout_boot = crate::shell::right_sidebar::SidebarLayoutBoot {
            initial_width: Some(initial_width),
            settings_repo: Some(settings_repo),
        };
        let built = cx.new(|cx| {
            crate::shell::right_sidebar::RightSidebar::new(
                repo,
                project_root,
                prior_open,
                Some(on_open),
                on_open_diff,
                Some(on_query),
                worktree_settings_repo,
                layout_boot,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        // Cache the freshly built sidebar so a later switch back to
        // this project reuses it (fast path in `set_active_project`)
        // instead of rebuilding from scratch.
        let ports_panel = self.ports_panel.clone();
        let simulator_panel = self.simulator.panel(cx);
        built.update(cx, |s, cx| {
            s.set_ports_panel(ports_panel, cx);
            s.set_simulator_panel(simulator_panel, cx);
            s.search_panel.update(cx, |p, _| p.set_on_open_hit(on_open_search_hit));
            if let Some(tab) = replacing {
                s.select_tab(tab, cx);
            }
        });
        self.right_sidebar_by_project
            .insert(project_id.clone(), built.clone());
        // The user may have switched projects while the repo opened;
        // the cache entry above serves them when they come back, but
        // the visible sidebar must stay the active project's.
        if self.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str())
        {
            built.read(cx).set_polling_focused(false);
            return;
        }
        self.right_sidebar = Some(built);
        // The rebuild minted fresh SCM panel entities — re-point
        // every source-control event subscription at them, or the
        // "View all" / commit / branch / discard / stash actions
        // would silently stop firing after a project switch.
        self.rewire_scm_subscriptions(window, cx);
        // RT-3: forward the new project to any open Tasks tab.
        let active_proj = self.active_project.clone();
        self.refresh_tasks_tab_for_active_project(active_proj, cx);
        // Re-focus the active pane after the right_sidebar
        // rebuild — the rebuild's `cx.notify` triggers a
        // repaint that can land focus on a freshly-mounted
        // sub-element of the sidebar (FileExplorer, etc.)
        // instead of the user's last-active terminal/editor.
        // Mirrors the "open project → cursor in last
        // working terminal" behavior; also keeps the chrome
        // toggle buttons routable since their actions need a
        // focused element inside the workspace_root subtree.
        if replacing.is_none() {
            refocus_active_pane(self, window, cx);
        }
        cx.notify();
    }

    /// Rebuild the active project's sidebar if it was built for a plain
    /// folder that has since been `git init`-ed, so the Source Control tab
    /// appears without a restart.
    ///
    /// Opens the repository first and rebuilds only once that succeeds. A
    /// failed open keeps the plain-folder sidebar and its eligibility, so the
    /// next tick asks again: a tick that lands while `git init` is still
    /// writing `.git` recovers on the one after, and a `.git` that never
    /// opens costs one `git rev-parse` per tick rather than a sidebar rebuild.
    ///
    /// Cheap when there is nothing to do: a git-backed sidebar answers
    /// without touching the disk, and a plain-folder one costs a single
    /// `.git` stat. Called from the periodic diff tick; activation has its
    /// own check on the cached sidebar in `set_active_project`.
    pub(crate) fn rebuild_sidebar_after_git_init(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(project), Some(sidebar)) =
            (self.active_project.clone(), self.right_sidebar.clone())
        else {
            return;
        };
        // Right after a switch, the visible sidebar is still the previous
        // project's until the new one's build lands — never judge it
        // against the new project's root.
        if self.right_sidebar_by_project.get(&project.id) != Some(&sidebar) {
            return;
        }
        let project_root = PathBuf::from(&project.root_path);
        if !sidebar.read(cx).awaits_git_init() || !has_git_dir(&project_root) {
            return;
        }
        // One open in flight per sidebar: ticks landing before it answers
        // must not start another.
        sidebar.update(cx, |s, _| s.set_repo_probe_in_flight(true));
        cx.spawn_in(window, async move |weak, cx| {
            let opened = Repository::open(&project_root).await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let repo = match opened {
                    Ok(repo) => repo,
                    Err(err) => {
                        tracing::debug!(?err, "`.git` present but not yet a repo; retrying");
                        sidebar.update(cx, |s, _| s.set_repo_probe_in_flight(false));
                        return;
                    }
                };
                // Replaced or dropped while the repo opened (a project
                // switch rebuilt it, or the project was removed): nothing
                // left for this answer to replace.
                if this.right_sidebar_by_project.get(&project.id) != Some(&sidebar) {
                    return;
                }
                tracing::info!(
                    project_id = %project.id,
                    "project became a git repo; rebuilding its sidebar"
                );
                let tab = sidebar.read(cx).active_tab;
                this.install_right_sidebar(
                    project.id.clone(),
                    project_root,
                    Some(repo),
                    Some(tab),
                    window,
                    cx,
                );
            });
        })
        .detach();
    }
}
