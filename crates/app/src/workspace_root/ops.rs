use super::*;

impl WorkspaceRoot {
    /// Launch dirs for the *active* project (its root plus each linked git
    /// worktree) — the default same-project scope for the session-history
    /// picker. Empty when no project is active, which the picker reads as
    /// "show all". Synchronous SQLite reads only.
    pub(super) fn active_project_scope_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        if let Some(project) = &self.active_project {
            paths.push(project.root_path.clone());
            if let Ok(list) = self.app_state.workspace_repo.list_for_project(&project.id) {
                for w in list {
                    if w.worktree_path != project.root_path {
                        paths.push(w.worktree_path);
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    /// Collect every worktree path across all recent projects (the project
    /// root plus each linked worktree). Same source the rail snapshot uses;
    /// deduped so a project root that already has a workspace row is counted
    /// once. Synchronous SQLite reads only — no git, safe to call per round.
    fn all_worktree_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        for project in &self.app_state.recent_projects {
            paths.push(project.root_path.clone());
            if let Ok(list) = self.app_state.workspace_repo.list_for_project(&project.id) {
                for w in list {
                    if w.worktree_path != project.root_path {
                        paths.push(w.worktree_path);
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();
        paths
    }

    /// Run one concurrent diff-count refresh round. Self-guards against
    /// overlap via `diff_refresh_in_flight`. Fans out all per-worktree
    /// `git diff --numstat` shellouts concurrently (serial per-worktree
    /// shellouts previously froze the rail), then writes the results back on
    /// the main thread and evicts paths that no longer exist so the cache
    /// cannot grow without bound.
    ///
    /// The numstat shellout spawns a child via `tokio::process`, which needs a
    /// live Tokio reactor. GPUI's background executor has none, so the fan-out
    /// runs on the app's Tokio runtime (entered on the main thread for the life
    /// of the app) and results are ferried back over a oneshot to a GPUI task.
    /// Called only from main-thread GPUI callbacks, where `Handle::try_current`
    /// resolves to that runtime.
    pub(crate) fn run_diff_refresh_round(&mut self, cx: &mut Context<Self>) {
        // Reconciliation net for the sidebar's DB caches: any workspace /
        // agent-session write that missed an explicit `mark_rail_dirty`
        // call is picked up within one focus-gated tick.
        self.mark_rail_dirty(cx);
        if self.diff_refresh_in_flight {
            return;
        }
        let paths = self.all_worktree_paths();
        if paths.is_empty() {
            return;
        }
        // Bail (leaving the flag clear) when no runtime is entered so a
        // headless/test context degrades to "no live counts" instead of
        // panicking inside the child-process spawn.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::workspace_root",
                    "no tokio runtime; worktree diff counts stay stale this round"
                );
                return;
            }
        };
        self.diff_refresh_in_flight = true;
        let (tx, rx) =
            tokio::sync::oneshot::channel::<Vec<(String, Option<DiffCounts>)>>();
        handle.spawn(async move {
            let futs = paths.into_iter().map(|path| async move {
                let counts = oximux_git::diff_numstat_head(std::path::Path::new(&path))
                    .await
                    .ok()
                    .map(|map| sum_numstat(&map));
                (path, counts)
            });
            let _ = tx.send(futures::future::join_all(futs).await);
        });
        cx.spawn(async move |weak, cx| {
            let Ok(results) = rx.await else {
                // Sender dropped (runtime torn down) — clear the flag so a
                // later round can retry rather than wedging in-flight.
                let _ = weak.update(cx, |this, _| {
                    this.diff_refresh_in_flight = false;
                });
                return;
            };
            let _ = weak.update(cx, |this, cx| {
                // Snapshot of paths seen this round drives eviction so removed
                // worktrees age out of the cache.
                let current: std::collections::HashSet<String> =
                    results.iter().map(|(p, _)| p.clone()).collect();
                for (path, counts) in results {
                    // A failed fetch leaves the prior value intact rather than
                    // blanking the chip on a transient git error.
                    if let Some(counts) = counts {
                        this.diff_counts.insert(path, counts);
                    }
                }
                this.diff_counts.retain(|k, _| current.contains(k));
                this.diff_refresh_in_flight = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Load global + active-project custom commands and push them into the
    /// command palette. Safe to call with no active project (loads global
    /// only, project file simply won't exist). Called on startup and
    /// whenever `ReloadCustomCommands` fires.
    pub(crate) fn reload_custom_commands(&self, cx: &mut Context<Self>) {
        // `load_for_project` gracefully no-ops a missing project-level
        // `.oximux/commands.toml`, so passing a non-existent root is fine.
        let project_root = self
            .active_project
            .as_ref()
            .map(|p| std::path::PathBuf::from(&p.root_path))
            .unwrap_or_else(|| std::path::PathBuf::from("/dev/null"));
        let commands = crate::custom_commands_loader::load_for_project(&project_root);
        self.palette
            .update(cx, |p, cx| p.set_custom_commands(commands, cx));
    }

    /// Open a fresh local-PTY tab in the active project's active pane group.
    pub(super) fn spawn_local_terminal_tab(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| p.open_terminal_tab_in_active_group(window, cx));
    }

    /// Open (or re-activate) the singleton Tasks tab in the active project's
    /// active pane group. Called from the nav rail's Tasks row mouse handler.
    ///
    /// When no project is active (welcome state), surfaces a brief toast so
    /// the click is never a silent no-op (RT-4).
    pub(crate) fn open_tasks_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.active_project_panes() else {
            // RT-4: no project open → inform the user instead of silently failing.
            self.push_toast(
                ToastKind::Info,
                "Open a project to browse tasks.",
                cx,
            );
            return;
        };
        let weak_root: WeakEntity<WorkspaceRoot> = cx.weak_entity();
        // The page spans every known project (aggregate scope by default), so
        // it's seeded with the full project list, not just the active one.
        let projects = self.app_state.recent_projects.clone();
        panes.update(cx, |p, cx| {
            p.open_or_activate_tasks_tab_in_active_group(
                weak_root,
                projects,
                window,
                cx,
            );
        });
    }

    // `toggle_floating_terminal` and the rest of the floating-terminal host
    // logic (restore, new-tab spawn, expand-to-pane, rename) live in
    // `crate::shell::floating_terminal_host` — same split-impl pattern as
    // `workspace_ops`.


    /// Resolves the currently-visible `ProjectPanes` entity by reading
    /// `active_project.id` against the per-project map. `None` when no
    /// project is active (welcome state) or when the project has no entity
    /// yet (mid-`set_active_project`).
    pub(crate) fn active_project_panes(&self) -> Option<Entity<ProjectPanes>> {
        let id = self.active_project.as_ref().map(|p| p.id.as_str())?;
        self.project_panes_by_project.get(id).cloned()
    }

    /// Resolve the right-clicked terminal grid's view plus its
    /// `(group_id, tab_idx)` from the session id carried in
    /// `OpenTerminalContextMenuAt`. Walks the active project's groups → tabs →
    /// split-tree leaves. `None` when no live view matches (e.g. the pane was
    /// torn down between right-click and action dispatch).
    pub(crate) fn resolve_terminal_view_by_session(
        &self,
        session_id: u64,
        cx: &gpui::App,
    ) -> Option<(
        gpui::WeakEntity<crate::shell::terminal_view::TerminalView>,
        u64,
        u32,
    )> {
        let panes = self.active_project_panes()?;
        let panes_ref = panes.read(cx);
        for group_id in panes_ref.manager().in_order_groups() {
            let Some(group) = panes_ref.group(group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            for (tab_idx, tab) in group_ref.tabs().iter().enumerate() {
                if let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content {
                    for (_, _, view) in tree.iter_all_views() {
                        if view.read(cx).session_id().0 == session_id {
                            return Some((view.downgrade(), group_id.0, tab_idx as u32));
                        }
                    }
                }
            }
        }
        None
    }

    /// (Re)wire every source-control-panel event subscription against the
    /// CURRENT `right_sidebar` entities. Called from `new` AND after every
    /// `set_active_project` sidebar rebuild: that rebuild mints fresh
    /// `git_panel` / `commit_graph` / `branch_commits` / `stash_panel`
    /// entities, so any subscription captured against the prior generation
    /// silently stops firing. (Single-file diff opens survive a rebuild
    /// because they route through a stable `weak_self` callback re-passed at
    /// every sidebar build, not through an entity subscription.) Overwriting
    /// each `_*_subscription` field drops the stale one.
    pub(crate) fn rewire_scm_subscriptions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sc) = self
            .right_sidebar
            .as_ref()
            .and_then(|rs| rs.read(cx).source_control.as_ref().cloned())
        else {
            // Non-git sidebar — no SCM panels. Drop any subscriptions left
            // over from a prior git project.
            self._discard_subscription = None;
            self._push_stash_subscription = None;
            self._show_commit_subscription = None;
            self._show_branch_file_subscription = None;
            self._show_combined_diff_subscription = None;
            self._show_branch_diff_all_subscription = None;
            return;
        };
        // Clone the child entities + repo up front so the immutable read
        // borrow ends before `cx.subscribe_in` borrows `cx` mutably.
        let (git_panel, stash_panel, commit_graph, branch_commits, repo) = {
            let sc = sc.read(cx);
            (
                sc.git_panel.clone(),
                sc.stash_panel.clone(),
                sc.commit_graph.clone(),
                sc.branch_commits.clone(),
                sc.repo.clone(),
            )
        };

        self._discard_subscription = Some(cx.subscribe_in(
            &git_panel,
            window,
            |root, panel, _ev: &DiscardRequested, window, cx| {
                root.mount_discard_dialog(panel.clone(), window, cx);
            },
        ));

        self._push_stash_subscription = Some(cx.subscribe_in(
            &stash_panel,
            window,
            |root, panel, _ev: &PushStashRequested, window, cx| {
                root.mount_push_stash_dialog(panel.clone(), window, cx);
            },
        ));

        let commit_repo = repo.clone();
        self._show_commit_subscription = Some(cx.subscribe_in(
            &commit_graph,
            window,
            move |root, _graph, ev: &ShowCommitRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let sha = ev.sha.clone();
                let short_oid = ev.short_oid.clone();
                let subject = ev.subject.clone();
                let repo = commit_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_commit_tab(repo, sha, short_oid, subject, window, cx);
                });
            },
        ));

        let branch_file_repo = repo.clone();
        self._show_branch_file_subscription = Some(cx.subscribe_in(
            &branch_commits,
            window,
            move |root, _panel, ev: &ShowBranchFileRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let base = ev.base.clone();
                let head = ev.head.clone();
                let path = ev.path.clone();
                let repo = branch_file_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_branch_diff_tab(repo, base, head, path, window, cx);
                });
            },
        ));

        let combined_repo = repo.clone();
        self._show_combined_diff_subscription = Some(cx.subscribe_in(
            &git_panel,
            window,
            move |root, _panel, ev: &ShowCombinedDiffRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let scope = ev.scope.clone();
                let repo = combined_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_combined_diff_tab(repo, scope, window, cx);
                });
            },
        ));

        let branch_all_repo = repo.clone();
        self._show_branch_diff_all_subscription = Some(cx.subscribe_in(
            &branch_commits,
            window,
            move |root, _panel, ev: &ShowBranchDiffAllRequested, window, cx| {
                let Some(panes) = root.active_project_panes() else {
                    return;
                };
                let scope = oximux_core::CombinedDiffScope::Branch {
                    base: ev.base.clone(),
                    head: ev.head.clone(),
                };
                let repo = branch_all_repo.clone();
                panes.update(cx, |p, cx| {
                    p.open_or_activate_combined_diff_tab(repo, scope, window, cx);
                });
            },
        ));
    }

    /// Route the Explorer context-menu Rename action into the FileExplorer's
    /// inline-rename flow. The row turns into an editable
    /// Input in-place (no modal). FileExplorer owns the actual fs op +
    /// post-rename refresh; this handler just kicks the state transition.
    pub(crate) fn start_inline_file_rename(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Refuse paths without a parent (i.e. filesystem root) — rename
        // isn't meaningful there and `std::fs::rename` would fail anyway.
        if path.parent().is_none() {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                path = %path.display(),
                "rename refused: path has no parent directory"
            );
            return;
        }
        // Close the context menu so its backdrop doesn't sit on top of
        // the inline input the explorer is about to mount.
        self.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
        let Some(rs) = self.right_sidebar.as_ref() else {
            return;
        };
        let fe = rs.read(cx).file_explorer.clone();
        fe.update(cx, |fe, cx| fe.start_rename(path, window, cx));
    }

    /// Begin inline creation of a new file (`is_dir == false`) or folder under
    /// `parent`. Closes the context menu and hands off to the explorer, which
    /// injects an editable placeholder row and creates the entry on Enter.
    pub(crate) fn start_inline_create(
        &mut self,
        parent: std::path::PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_context_menu.update(cx, |m, cx| m.close(cx));
        let Some(rs) = self.right_sidebar.as_ref() else {
            return;
        };
        let fe = rs.read(cx).file_explorer.clone();
        fe.update(cx, |fe, cx| fe.start_create(parent, is_dir, window, cx));
    }

    /// Open `path` as a new editor tab in the active project's active
    /// pane group. If the file is already open in any tab of that group,
    /// activate it instead of opening a duplicate.
    ///
    /// Pre-filters binary / system files so the editor never opens an
    /// empty buffer on a UTF-8 decode failure. No-op when there's no
    /// active project.
    pub fn open_file_in_active_pane(
        &self,
        path: std::path::PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !is_openable_text_file(&path) {
            tracing::info!(
                file = %path.display(),
                "open-file: refusing non-text file (binary, system metadata, or unreadable)"
            );
            return;
        }
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            if preview {
                p.open_preview_editor_tab(path, window, cx);
            } else {
                p.open_or_activate_editor_tab(path, window, cx);
            }
        });
    }

    /// Open `path` as a read-only diff tab in the active project's active
    /// pane group. `staged=true` shows the staged-vs-HEAD diff; `false`
    /// shows worktree-vs-index. Idempotent — clicking the same SCM row
    /// re-focuses the existing diff tab rather than opening a duplicate.
    pub fn open_diff_in_active_pane(
        &self,
        repo: oximux_git::Repository,
        path: std::path::PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            p.open_or_activate_diff_tab(repo, path, staged, untracked, window, cx);
        });
    }

    /// Seed the Search panel's include-glob field and switch the right
    /// sidebar to the Search tab. Drives the file-tree "Find in Folder"
    /// context-menu item. No-ops when no right sidebar is mounted (the
    /// active project isn't a git repo OR no project is active).
    pub(crate) fn seed_search_include_and_switch(
        &self,
        include_glob: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rs) = self.right_sidebar.clone() else {
            return;
        };
        rs.update(cx, |sidebar, cx| {
            // Seed first, then flip the tab so the rendered panel
            // already shows the glob when it appears.
            let include_input = sidebar.search_panel.read(cx).include_input_ref().clone();
            include_input.update(cx, |state, cx| {
                state.set_value(include_glob.as_str(), window, cx);
            });
            sidebar.select_tab(crate::shell::right_sidebar::tab::RightTab::Search, cx);
        });
    }

    /// Reveal `path` in the file-tree sidebar: open the sidebar if collapsed,
    /// switch to the Explorer tab, then expand the path's ancestors and scroll
    /// to its row. Drives the editor breadcrumb's "Reveal in Explorer View"
    /// action. No-ops when no right sidebar is mounted.
    pub(crate) fn reveal_path_in_explorer(
        &self,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(rs) = self.right_sidebar.clone() else {
            return;
        };
        rs.update(cx, |sidebar, cx| {
            if !sidebar.open {
                sidebar.toggle(cx);
            }
            sidebar.select_tab(crate::shell::right_sidebar::tab::RightTab::Explorer, cx);
            sidebar
                .file_explorer
                .update(cx, |fe, cx| fe.reveal_path(path, cx));
        });
    }

    /// Force the file explorer to re-read its cached directories from disk.
    /// Called after a mutation (duplicate / delete) so the new tree state
    /// shows up without waiting for the filesystem watcher.
    fn refresh_file_explorer(&self, cx: &mut Context<Self>) {
        if let Some(rs) = self.right_sidebar.as_ref() {
            let fe = rs.read(cx).file_explorer.clone();
            fe.update(cx, |fe, cx| fe.manual_refresh(cx));
        }
    }

    /// Duplicate a file/folder next to itself, then refresh the tree and
    /// reveal the new entry. Errors surface as a toast — duplication failures
    /// (permissions, disk full) aren't recoverable from the UI.
    pub(crate) fn duplicate_file_entry(
        &mut self,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |weak, cx| {
            // Copy off the main thread — duplicating a large folder must not
            // freeze the UI. Re-enter the main thread only for the refresh.
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::shell::file_explorer::file_mutations::duplicate_path(&path)
                })
                .await;
            weak.update(cx, |this, cx| match result {
                Ok(new_path) => {
                    this.refresh_file_explorer(cx);
                    this.reveal_path_in_explorer(new_path, cx);
                }
                Err(err) => {
                    this.push_toast(ToastKind::Error, format!("Duplicate failed: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Mount a plain confirm dialog for a file-tree Delete. On confirm the
    /// target is moved to the macOS Trash (reversible) and the tree refreshes;
    /// an open editor tab for the path is left to the external-mutation sweep,
    /// which flags it as deleted. Reuses the shared `confirm_dialog` slot +
    /// observer, same as the SCM discard flow.
    pub(crate) fn mount_file_delete_confirm(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let kind = if path.is_dir() { "folder" } else { "file" };
        let body = format!("Move the {kind} “{name}” to the Trash? You can restore it from the Trash.");

        let target = path.clone();
        let weak = cx.entity().downgrade();
        let on_confirm: ConfirmCallback = Rc::new(move |_window, cx| {
            match crate::shell::file_explorer::file_mutations::move_to_trash(&target) {
                Ok(()) => {
                    if let Some(root) = weak.upgrade() {
                        root.update(cx, |root, cx| root.refresh_file_explorer(cx));
                    }
                }
                Err(err) => crate::shell::toast::toast_op_error(cx, "Delete", &err),
            }
        });

        let prompt = ConfirmPrompt {
            title: "Move to Trash".into(),
            body: body.into(),
            expected: "".into(),
            on_confirm,
            confirm_label: Some("Move to Trash".into()),
            on_cancel: None,
            secondary: None,
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));
        // Cancel any in-flight observer (e.g. an SCM discard dialog) before
        // installing this one, matching the explicit-clear pattern used at the
        // other `confirm_dialog` mount sites.
        self._discard_dialog_observer = None;
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));
        self.confirm_dialog = Some(dialog);
        cx.notify();
    }

    /// Mount a `ConfirmDialog` for the SCM panel's pending discard
    /// request. Builds the prompt copy from the panel's snapshot,
    /// wires `on_confirm` to `confirmed_discard_path` and `on_cancel`
    /// to `clear_pending_discard`, then installs an observer that
    /// drops the dialog from the slot once the user confirms or
    /// cancels.
    fn mount_discard_dialog(
        &mut self,
        panel: Entity<GitPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = panel.read(cx).pending_discard().cloned() else {
            return;
        };

        // Dispatch on `scope`: Single → the existing single-path flow
        // (which Slice C teaches about pure-untracked dispatch);
        // Area → the per-section sequence with its unstage-first-for-
        // staged + git-clean-for-untracked branches.
        //
        // `request.paths` is moved (not cloned): `request` is the
        // owned snapshot from `pending_discard().cloned()` and we
        // don't use the rest of it after this point. The inner
        // `paths.clone()` in the callback is the unavoidable one —
        // `ConfirmCallback` is `Rc<dyn Fn>` and may fire more than
        // once.
        let on_confirm: ConfirmCallback = {
            let panel = panel.clone();
            let scope = request.scope;
            let paths = request.paths;
            Rc::new(move |_window, cx| {
                panel.update(cx, |p, cx| match scope {
                    crate::shell::git_panel::DiscardScope::Single { .. } => {
                        if let Some(path) = paths.first().cloned() {
                            p.confirmed_discard_path(path, cx);
                        }
                    }
                    crate::shell::git_panel::DiscardScope::Area { area } => {
                        p.confirmed_discard_area(area, paths.clone(), cx);
                    }
                });
            })
        };
        let on_cancel: ConfirmCallback = {
            let panel = panel.clone();
            Rc::new(move |_window, cx| {
                panel.update(cx, |p, cx| p.clear_pending_discard(cx));
            })
        };

        let prompt = ConfirmPrompt {
            title: request.copy.title,
            body: request.copy.body,
            expected: request.expected,
            on_confirm,
            confirm_label: Some(request.copy.confirm_label),
            on_cancel: Some(on_cancel),
            secondary: None,
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog = cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));

        // Drop the dialog the moment the user resolves it. Replacing
        // `_discard_dialog_observer` cancels any previous observer
        // that's tied to a stale dialog.
        self._discard_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.confirm_dialog = None;
                    root._discard_dialog_observer = None;
                    cx.notify();
                }
            },
        ));

        self.confirm_dialog = Some(dialog);
        cx.notify();
    }

    /// Mount a `PushStashDialog` for the SCM panel's stash-push
    /// request. Wires `on_confirm` to call `StashPanel::push` with
    /// the user-supplied message + include-untracked toggle. Installs
    /// an observer that drops the dialog from the slot once the user
    /// confirms or cancels.
    ///
    /// First-open-wins: a double-click on the header `+` button (or
    /// any sequence that re-fires `PushStashRequested` while the
    /// dialog is already mounted) is ignored. Replacing the slot
    /// would silently drop a half-typed form, which is the bug Phase
    /// 01's discard-dialog reviewer caught for the destructive flow;
    /// applying the same guard here so the user's in-progress
    /// message survives a stray re-click.
    fn mount_push_stash_dialog(
        &mut self,
        panel: Entity<StashPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.push_stash_dialog.is_some() {
            return;
        }
        let on_confirm: PushCallback = {
            let panel = panel.clone();
            Rc::new(move |msg, include_untracked, _window, cx| {
                panel.update(cx, |p, cx| p.push(msg, include_untracked, cx));
            })
        };
        // Cancel path is a no-op on the panel side — the dialog flips
        // `cancelled`, the observer below drops the slot. Wired anyway
        // so future telemetry (e.g. counting abandoned pushes) has a
        // hook point.
        let on_cancel: CancelCallback = Rc::new(|_window, _cx| {});

        let prompt = PushStashPrompt {
            on_confirm,
            on_cancel: Some(on_cancel),
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| PushStashDialog::new(prompt, theme, density, typography, window, cx));

        // Drop the dialog the moment the user resolves it. Replacing
        // `_push_stash_dialog_observer` cancels any previous observer
        // tied to a stale dialog.
        self._push_stash_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |root, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    root.push_stash_dialog = None;
                    root._push_stash_dialog_observer = None;
                    cx.notify();
                }
            },
        ));

        self.push_stash_dialog = Some(dialog);
        cx.notify();
    }

    /// Build the on-click callback handed to the SCM panel for diff
    /// opens. Captures a weak self-handle so the callback survives
    /// project switches that rebuild `RightSidebar`. The `repo`
    /// argument is captured at build time — RightSidebar already owns
    /// it for the lifetime of the source-control surface.
    pub(crate) fn build_on_open_diff_callback(
        weak: WeakEntity<Self>,
        repo: oximux_git::Repository,
    ) -> crate::shell::file_tree_view::OnOpenDiff {
        Arc::new(move |path, staged, untracked, window, cx| {
            let repo = repo.clone();
            let _ = weak.update(cx, |this, cx| {
                this.open_diff_in_active_pane(repo, path, staged, untracked, window, cx);
            });
        })
    }

    /// Build the on-click callback handed to the Files-tab `FileTreeView`.
    /// The closure captures a weak self-handle so the callback survives
    /// project switches that rebuild `RightSidebar`. A dropped weak handle
    /// (window closed) silently no-ops the click.
    pub(crate) fn build_on_open_file_callback(
        weak: WeakEntity<Self>,
    ) -> crate::shell::file_tree_view::OnOpenFile {
        Arc::new(move |path, preview, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.open_file_in_active_pane(path, preview, window, cx);
            });
        })
    }

    /// Build the active-file query handed to the Files-tab `FileTreeView`.
    /// Resolves the focused leaf of the active project's active tab and
    /// returns the file path of its currently-active editor tab (`None`
    /// when the focused leaf is a terminal or when no project is active).
    /// Fires once per FileTreeView render; cheap enough to walk on every
    /// frame since the tab + pane lookups are HashMap reads.
    pub(crate) fn build_on_query_active_path_callback(
        weak: WeakEntity<Self>,
    ) -> crate::shell::file_tree_view::OnQueryActivePath {
        Arc::new(move |cx| {
            let root = weak.upgrade()?;
            let panes = root.read(cx).active_project_panes()?;
            panes.read(cx).active_editor_path(cx)
        })
    }

    /// Walk every open project's tabs and serialize plain-terminal
    /// scrollback to `pane_buffers`. Called from the app-quit hook so
    /// state restored on next launch reflects the user's final view.
    pub fn capture_all_pane_buffers(&self, cx: &gpui::App) {
        let repo = self.app_state.pane_buffer_repo.clone();
        let window_id = &self.window_id;
        for (project_id, panes) in &self.project_panes_by_project {
            panes.read(cx).capture_pane_buffers(
                &repo,
                project_id,
                window_id,
                crate::project_panes_factory::PANE_BUFFER_MAX_BYTES,
                cx,
            );
        }
    }

    /// Walk every open project's `ProjectPanes` and persist its full
    /// layout snapshot (groups, sub-pane trees, tab_order, active
    /// indices, editor paths, agent metadata). Without this hook the
    /// on-quit save chain would only persist pane scrollback + relay
    /// ids — every tab/group structural change made during the session
    /// would be lost. Pairs with `capture_all_pane_buffers` so a single
    /// quit fires both writes.
    pub fn capture_all_layouts(&self, cx: &gpui::App) {
        for panes in self.project_panes_by_project.values() {
            panes.read(cx).save_now(cx);
        }
    }

    /// Walk every open project's tabs and persist each plain-terminal
    /// leaf's relay PTY id (Phase 5 step 6). Called from the same
    /// hooks as `capture_all_pane_buffers` so the two tables stay in
    /// sync. No-op when there's no relay session (in-process backend).
    pub fn capture_all_pane_relay_ids(&self, cx: &gpui::App) {
        // Cached session id — no ListPtys daemon round-trip on this
        // autosave/capture path (it only needs the session id, not live ids).
        let Some(session_id) = crate::shell::terminal_view::relay_session_id_cached() else {
            return;
        };
        self.capture_all_pane_relay_ids_with_session(&session_id, cx);
    }

    /// Same capture, but with the relay session id already in hand —
    /// for callers that just took a relay snapshot (the post-paint
    /// reconcile), so the capture adds no extra daemon round-trip on
    /// the main thread.
    pub fn capture_all_pane_relay_ids_with_session(&self, session_id: &str, cx: &gpui::App) {
        let repo = self.app_state.pane_relay_id_repo.clone();
        let window_id = &self.window_id;
        for (project_id, panes) in &self.project_panes_by_project {
            panes
                .read(cx)
                .capture_pane_relay_ids(&repo, project_id, window_id, session_id, cx);
        }
    }

    /// The id of the project currently active in this window, if any. Read
    /// by the open-windows manifest writer so the next launch can reopen
    /// this window onto the same project.
    pub(crate) fn active_project_id(&self) -> Option<String> {
        self.active_project.as_ref().map(|p| p.id.clone())
    }

    /// Borrow this window's `SettingsRepo` (shared app-wide via the same DB).
    /// The lib-level session-capture helper uses it to persist the
    /// open-windows manifest without the binary crate reaching into
    /// `AppState`'s private fields.
    pub(crate) fn settings_repo(&self) -> &oximux_storage::SettingsRepo {
        &self.app_state.settings_repo
    }

    /// Spawn the chosen agent in a new tab inside the active pane group.
    /// Runs the start_session → backend_for → terminal_session_id →
    /// subscribe_status chain, then hands the assembled handles to
    /// `ProjectPanes::push_agent_tab`.
    ///
    /// If `update_in` errors (window/workspace dropped mid-spawn), cancels
    /// the half-mounted session so the PTY doesn't zombie.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_agent_tab(
        &self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        cwd: std::path::PathBuf,
        model: Option<String>,
        effort: Option<String>,
        resumption: oximux_core::SessionResumption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        // Apply per-agent launch defaults from `agent_launch.toml`: fill the
        // model when the caller didn't pin one, and append the configured
        // extra flags (e.g. a skip-permissions default). The global is unset
        // until the settings layer seeds it, in which case defaults are empty.
        let (model, mut extra_args, status_hooks_on) = {
            let defaults = cx.try_global::<oximux_settings::AgentLaunchSettings>();
            (
                model.or_else(|| defaults.and_then(|d| d.model_for(adapter_id))),
                defaults.map(|d| d.args_for(adapter_id)).unwrap_or_default(),
                // On by default (mirrors `AgentLaunchSettings::default`); also
                // on when the global isn't seeded yet, so a very early launch
                // still gets status. A user's explicit `false` flows through.
                defaults.map(|d| d.status_hooks_enabled).unwrap_or(true),
            )
        };
        // OSC-9999 status hooks (on by default; Settings → Agents toggle, or
        // the OXIMUX_STATUS_HOOKS=1 env override): inject the `--settings`
        // hooks block so Claude Code emits the prompt + tool + lifecycle the
        // poll-loop scanner reads. Claude-only for now; no-op when disabled.
        if adapter_id == "claude-code" {
            crate::agent_status_hooks::maybe_inject(status_hooks_on, &mut extra_args);
        }
        let runtime = self.cli_runtime.clone();
        let cwd_for_tab = cwd.clone();

        cx.spawn_in(window, async move |root, cx| {
            // `adapter_id` arrives from the row the user clicked — the
            // picker holds the `RegistryEntry` slug at click time, so we
            // skip a redundant `detect_available` walk here (M1 fix from
            // review 260520-1830).
            let cfg = AgentSessionConfig {
                adapter,
                worktree_path: cwd,
                prompt: None,
                model: model.clone(),
                effort: effort.clone(),
                extra_args,
                env: Vec::new(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                custom_command: None,
                resumption,
            };
            let session_id = match runtime.start_session(cfg).await {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, adapter = adapter_id, "start_session failed");
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };
            let backend = match runtime.backend_for(session_id) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(?err, "backend_for after start_session");
                    let _ = runtime.cancel(session_id).await;
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };
            let term_id = match runtime.terminal_session_id(session_id) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(?err, "terminal_session_id after start_session");
                    let _ = runtime.cancel(session_id).await;
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };
            let status_rx = match runtime.subscribe_status(session_id) {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(?err, "subscribe_status after start_session");
                    let _ = runtime.cancel(session_id).await;
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(
                            cx,
                            &format!("Start {adapter_id} agent"),
                            &err.to_string(),
                        );
                    });
                    return;
                }
            };

            // Mirror this session's status history into the agent_sessions
            // row so the rail/dashboard rows show Running / Done / Stopped.
            // Watch receivers are cheap clones; the watcher self-terminates
            // on the terminal status.
            let _ = root.update(cx, |this, cx| {
                crate::shell::agent_session_persistence::spawn_for_session(
                    this,
                    cwd_for_tab.to_string_lossy().into_owned(),
                    adapter_id,
                    model.clone(),
                    effort.clone(),
                    session_id,
                    status_rx.clone(),
                    // Fresh launch — always inserts a new session row.
                    false,
                    cx,
                );
            });

            let mount_result = panes.update_in(cx, |p, window, cx| {
                p.push_agent_tab(
                    adapter,
                    adapter_id,
                    cwd_for_tab,
                    model,
                    effort,
                    session_id,
                    status_rx,
                    backend,
                    term_id,
                    None,
                    window,
                    cx,
                );
            });
            if mount_result.is_err() {
                tracing::warn!(
                    ?session_id,
                    "spawn_agent_tab: workspace dropped mid-spawn; cancelling orphan"
                );
                let _ = runtime.cancel(session_id).await;
            }
        })
        .detach();
    }

    /// Accessor for the workspace's CLI agent runtime. Used by the (future)
    /// settings panel + tests; the main consumer is the per-project
    /// `ProjectPanes`, which receives its own `Arc` clone at construction.
    #[doc(hidden)]
    pub fn cli_runtime(&self) -> Arc<CliRuntime> {
        self.cli_runtime.clone()
    }

    /// Accessor for the adapter registry. Same rationale as `cli_runtime`.
    #[doc(hidden)]
    pub fn adapter_registry(&self) -> Arc<AdapterRegistry> {
        self.adapter_registry.clone()
    }

    /// Test-only inspector for the left-rail visibility flag.
    #[doc(hidden)]
    pub fn left_rail_open(&self) -> bool {
        self.left_rail_open
    }

    // -----------------------------------------------------------------------
    // Cross-window tear-off (Slice C)
    // -----------------------------------------------------------------------

    /// Handler for the "Move Tab to New Window" context-menu action.
    ///
    /// Ordering contract (relay client enforces single subscriber per PTY):
    ///   1. Collect the tab's relay external_id while the view is still alive.
    ///   2. Call `detach` on each terminal leaf so the relay session is released
    ///      WITHOUT killing the daemon PTY.
    ///   3. `take_tab` removes the tab from the source group. The now-detached
    ///      `TerminalView` drops harmlessly (its `Drop` → `close` is a no-op
    ///      after `detach`).
    ///   4. Push a `PendingTearOff` for the minted destination window id.
    ///   5. Spawn an async task that opens the destination window. The window
    ///      build closure calls `consume_pending_tearoff` and then
    ///      `mount_pending_tearoff` to attach the PTY and mount a fresh
    ///      `TerminalView` in the new window's context.
    ///
    /// Rollback: if `attach_pty_existing` fails in the destination window, the
    /// PTY is orphaned in the daemon (alive but with no subscriber). We log
    /// loudly and do NOT silently swallow it. A future enhancement could
    /// re-attach to the source window; for v1 the orphan is a daemon-level
    /// concern (the relay's idle-gc eventually reaps it).
    pub(crate) fn handle_move_tab_to_new_window(
        &mut self,
        group_id_raw: u64,
        tab_idx_raw: u32,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        let group_id = crate::shell::pane_tree::PaneGroupId(group_id_raw);
        let tab_idx = tab_idx_raw as usize;

        // 1. Borrow the group to collect external_id(s) and tab metadata.
        //    All reads happen while the tab is still alive.
        let (external_ids, label, color, custom_title) = {
            let panes_ref = panes.read(cx);
            let Some(group) = panes_ref.group(group_id) else {
                return;
            };
            let group_ref = group.read(cx);
            let Some(tab) = group_ref.tabs().get(tab_idx) else {
                return;
            };
            let label = tab
                .custom_title
                .clone()
                .unwrap_or_else(|| tab.label.clone());
            let color = tab.color;
            let custom_title = tab.custom_title.clone();
            // Collect external_ids from every live terminal leaf.
            let ids: Vec<String> = match &tab.content {
                crate::shell::pane_content::PaneContent::Terminal(tree) => tree
                    .iter_live()
                    .filter_map(|(_, view)| view.read(cx).external_id())
                    .collect(),
                _ => return, // not a terminal tab — bail silently
            };
            if ids.is_empty() {
                tracing::warn!(
                    group_id = group_id_raw,
                    tab_idx,
                    "move-tab: no relay external_id on this tab; tear-off skipped"
                );
                return;
            }
            (ids, label, color, custom_title)
        };

        // 2. Detach BEFORE take_tab so the subscription is released
        //    while the TerminalView is still alive.
        {
            let panes_ref = panes.read(cx);
            let Some(group) = panes_ref.group(group_id) else {
                return;
            };
            let group_ref = group.read(cx);
            if let Some(tab) = group_ref.tabs().get(tab_idx)
                && let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content
            {
                for (_, view) in tree.iter_live() {
                    view.read(cx).detach();
                }
            }
        }

        // 3. Remove the tab from the source group. The detached TerminalViews
        //    drop here; their Drop → close is now a no-op.
        panes.update(cx, |p, cx| {
            if let Some(group) = p.group(group_id) {
                group.update(cx, |g, cx| {
                    let _ = g.take_tab(tab_idx, cx);
                });
            }
        });

        // 4. Mint a destination persist id and push the pending entry.
        let dest_window_id = crate::window_registry::next_persist_id(cx);
        let leaves: Vec<crate::window_registry::PendingLeaf> = external_ids
            .into_iter()
            .map(|id| crate::window_registry::PendingLeaf { external_id: id })
            .collect();
        let app_state = self.app_state.clone();
        let project_id = self.active_project_id();
        let pending = crate::window_registry::PendingTearOff {
            dest_window_id: dest_window_id.clone(),
            leaves,
            label,
            color,
            custom_title,
        };
        crate::window_registry::push_pending_tearoff(pending);

        // 5. Open the destination window asynchronously so we're out of the
        //    current borrow stack. The window build closure (in window_factory)
        //    consumes the pending entry and calls `mount_pending_tearoff`.
        cx.spawn_in(window, async move |_root, cx| {
            let _ = cx.update(|_window, cx| {
                crate::window_factory::open_workspace_window_with(
                    cx,
                    None, // repo resolved from project_id in window_factory
                    app_state,
                    dest_window_id,
                    project_id,
                );
            });
        })
        .detach();
    }

    /// Called by the destination window's build closure (via
    /// `window_factory::open_workspace_window_with`) when a pending tear-off
    /// entry is found for this window's id.
    ///
    /// For each relay PTY leaf in the entry: attach the existing relay PTY,
    /// mount a fresh `TerminalView`, and push it as a tab into the active
    /// pane group. The tab inherits the label, color, and custom title from
    /// the source window.
    ///
    /// On failure (e.g. `attach_pty_existing` returns `None`): logs a loud
    /// warning. The PTY is orphaned in the relay daemon and will be reaped by
    /// the daemon's idle-gc. No silent swallowing.
    pub fn mount_pending_tearoff(
        &mut self,
        tearoff: crate::window_registry::PendingTearOff,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            tracing::warn!(
                dest_window_id = %tearoff.dest_window_id,
                "mount_pending_tearoff: no active project panes; PTY orphaned in relay"
            );
            return;
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        // The torn-off PTY survives in the daemon, so its shell keeps the
        // original OXIMUX_* env it was spawned with. The destination view
        // gets a fresh identity under THIS window's workspace for future
        // persistence/respawn (carrying the source ids across windows is a
        // follow-up).
        let workspace_id = panes.read(cx).cwd().to_string_lossy().into_owned();

        for leaf in &tearoff.leaves {
            let Some((backend, session_id)) =
                crate::shell::terminal_view::attach_pty_existing(&leaf.external_id)
            else {
                tracing::warn!(
                    external_id = %leaf.external_id,
                    dest_window_id = %tearoff.dest_window_id,
                    "mount_pending_tearoff: attach_pty_existing failed; PTY orphaned in relay"
                );
                continue;
            };

            // Mount a fresh TerminalView in this window's entity context.
            // Entity<TerminalView> cannot cross windows — a new one is
            // required in the destination window context.
            let ids = crate::shell::context_env::SurfaceIds::fresh(workspace_id.clone());
            let view = cx.new(|cx| {
                crate::shell::terminal_view::TerminalView::mount(
                    backend,
                    session_id,
                    ids,
                    theme,
                    density,
                    typography.clone(),
                    window,
                    cx,
                )
            });

            let label_str = tearoff.label.to_string();
            let color_for_tab = tearoff.color;
            let custom_title_for_tab = tearoff.custom_title.clone();
            panes.update(cx, |p, cx| {
                if let Some(group) = p.active_group() {
                    group.update(cx, |g, cx| {
                        // Use the restore helper to append the tab (wires
                        // the observer that drives group re-renders on
                        // TerminalView notifications).
                        g.push_restored_terminal_tab(label_str.clone(), view.clone(), cx);
                        // Apply color + custom title onto the freshly-appended tab.
                        let last_idx = g.tabs().len().saturating_sub(1);
                        g.set_tab_color(last_idx, color_for_tab, cx);
                        if custom_title_for_tab.is_some() {
                            g.set_tab_title(last_idx, custom_title_for_tab.clone(), cx);
                        }
                    });
                }
            });
        }
    }

    /// Route a directional split to the active project's pane groups.
    /// New sibling spawns one Terminal tab + steals focus.
    pub(super) fn split_active_pane_group(
        &self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            p.split_active_group(axis, insert, window, cx);
        });
    }

    /// Reshape the active project's pane layout to `preset`. No-op when
    /// no project is active.
    pub(super) fn reshape_active_project_layout(
        &self,
        preset: crate::shell::pane_group::layout_presets::Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            p.apply_layout_preset(preset, window, cx);
        });
    }

    /// Close the focused pane group in the active project. Manager
    /// returns `LastGroup` when no siblings exist; we swallow that so
    /// the keybind / menu item is a no-op rather than an error popup.
    pub(super) fn close_active_pane_group(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        panes.update(cx, |p, cx| {
            let _ = p.close_active_group(window, cx);
        });
    }

    /// Scan every group in the active project's pane layout for an open Tasks
    /// tab and refresh its known-project set. Called on project switch / create
    /// so an already-open Tasks tab reflects the current project list without a
    /// manual Refresh. The aggregate scope keeps showing every project; a scope
    /// pinned to one project is preserved and only refetches if the set
    /// actually changed. The `_project` arg is retained for the call sites but
    /// no longer pins the tab — the page is scope-driven, not active-driven.
    pub(crate) fn refresh_tasks_tab_for_active_project(
        &self,
        _project: Option<oximux_core::Project>,
        cx: &mut Context<Self>,
    ) {
        let Some(panes) = self.active_project_panes() else {
            return;
        };
        let projects = self.app_state.recent_projects.clone();
        // Collect views first (immutable borrow) then update (mutable borrow)
        // to satisfy the borrow checker.
        let mut found_views: Vec<gpui::Entity<crate::shell::tasks_view::TasksView>> = Vec::new();
        {
            let panes_ref = panes.read(cx);
            for group_id in panes_ref.manager().in_order_groups() {
                let Some(group) = panes_ref.group(group_id) else {
                    continue;
                };
                for tab in group.read(cx).tabs() {
                    if let crate::shell::pane_content::PaneContent::Tasks(v) = &tab.content {
                        found_views.push(v.clone());
                    }
                }
            }
        }
        for view in found_views {
            let projects = projects.clone();
            view.update(cx, |tv, cx| {
                // Updates the list and refetches only when the set changed.
                tv.set_projects(projects, cx);
                tv.activate(cx);
            });
        }
    }

    /// Surface a quiet transient toast (bottom-right). The one entry point for
    /// fleeting cross-surface events; routes to the owned `ToastLayer`.
    pub(crate) fn push_toast(&self, kind: ToastKind, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        self.toast_layer.update(cx, |layer, cx| layer.push(kind, text, cx));
    }
}
