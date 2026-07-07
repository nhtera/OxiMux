use super::*;

impl ProjectPanes {

    /// Every hand-launched (ambient) agent across all groups, one entry per
    /// terminal PTY (the per-pane identity the rail lists as its own row).
    /// Surfaces hand-launched agents on the sidebar without a tracked session;
    /// grouping under a workspace + collapsing for the single-agent card dot is
    /// the caller's job (it resolves each entry's cwd to a worktree root).
    pub fn ambient_agents(
        &self,
        cx: &gpui::App,
    ) -> Vec<crate::shell::pane_group::AmbientAgentEntry> {
        let mut out = Vec::new();
        for group in self.groups.values() {
            out.extend(group.read(cx).ambient_agents(cx));
        }
        out
    }

    /// Open or activate a diff tab in the active group for `(path, staged)`.
    /// Mirrors `open_or_activate_editor_tab` but routes through `PaneGroup::
    /// open_or_activate_diff_tab` which constructs a fresh `DiffView` bound
    /// to `repo`. Diff tabs don't deduplicate across groups (unlike editor
    /// tabs) — clicking an SCM row always opens in the focused group.
    pub fn open_or_activate_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| {
            g.open_or_activate_diff_tab(repo, path, staged, untracked, window, cx)
        }))
    }

    /// Open or activate the singleton Tasks tab in the active group.
    /// Routes to `PaneGroup::open_or_activate_tasks_tab` which deduplicates
    /// by `PaneGroupTabKind::Tasks` (only one Tasks tab per group session).
    pub fn open_or_activate_tasks_tab_in_active_group(
        &mut self,
        weak_root: WeakEntity<crate::workspace_root::WorkspaceRoot>,
        projects: Vec<oximux_core::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied());
        let Some(target_id) = target_id else {
            return;
        };
        self.set_active_group(target_id, window, cx);
        if let Some(target) = self.groups.get(&target_id).cloned() {
            target.update(cx, |g, cx| {
                g.open_or_activate_tasks_tab(weak_root, projects, window, cx);
            });
        }
    }

    /// Open a new Agent Chat tab in the active group (or the first group if the
    /// active id is stale). Not a singleton — each call opens a fresh chat
    /// session with its own headless subprocess. Routed to by the launch picker
    /// when `default_open_mode` is `Chat` and the picked adapter is chat-capable.
    pub fn open_agent_chat_tab_in_active_group(
        &mut self,
        cwd: PathBuf,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied());
        let Some(target_id) = target_id else {
            return;
        };
        self.set_active_group(target_id, window, cx);
        if let Some(target) = self.groups.get(&target_id).cloned() {
            target.update(cx, |g, cx| {
                g.open_agent_chat_tab(cwd, model, window, cx);
            });
        }
    }

    /// Reopen a past session as a chat tab in the active group (from the
    /// Session History side panel). Dedups an already-open session; otherwise
    /// imports the transcript from `path` and spawns a resumed chat.
    pub fn open_session_as_chat_in_active_group(
        &mut self,
        session_id: &str,
        path: Option<&str>,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied());
        let Some(target_id) = target_id else {
            return;
        };
        self.set_active_group(target_id, window, cx);
        if let Some(target) = self.groups.get(&target_id).cloned() {
            target.update(cx, |g, cx| {
                g.open_session_as_chat(session_id, path, cwd, window, cx);
            });
        }
    }

    /// Close the Tasks tab in the active group, if present. Used after a
    /// workspace is created from the Tasks page so the foreground leaves the
    /// issue browser and falls back to the group's prior tab.
    pub fn close_tasks_tab_in_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied());
        let Some(target_id) = target_id else {
            return;
        };
        if let Some(target) = self.groups.get(&target_id).cloned() {
            target.update(cx, |g, cx| {
                g.close_tasks_tab(window, cx);
            });
        }
    }

    /// Open or activate a commit-detail tab in the active group.
    /// Mirrors `open_or_activate_diff_tab` but routes through
    /// `PaneGroup::open_or_activate_commit_tab` which dedups by SHA
    /// rather than path. The new tab loads via `DiffView::load_commit`
    /// so the same DiffView render path handles both single-file and
    /// commit-detail multi-file views.
    pub fn open_or_activate_commit_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        short_oid: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| {
            g.open_or_activate_commit_tab(repo, sha, short_oid, subject, window, cx)
        }))
    }

    /// Mirrors `open_or_activate_commit_tab` but routes through
    /// `PaneGroup::open_or_activate_branch_diff_tab` — a read-only
    /// range diff (`base..head` for one file) from the "Committed on
    /// Branch" section, deduped by path.
    pub fn open_or_activate_branch_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        base: String,
        head: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| {
            g.open_or_activate_branch_diff_tab(repo, base, head, path, window, cx)
        }))
    }

    /// Mirrors `open_or_activate_commit_tab` but routes through
    /// `PaneGroup::open_or_activate_combined_diff_tab` — a combined
    /// multi-file diff for `scope` (SCM "View all" CTAs), deduped by the
    /// scope's title.
    pub fn open_or_activate_combined_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        scope: oximux_core::CombinedDiffScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| {
            g.open_or_activate_combined_diff_tab(repo, scope, window, cx)
        }))
    }

    pub fn open_terminal_tab_in_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        let n = self.take_next_terminal_n(cx);
        group.update(cx, |g, cx| {
            g.set_next_terminal_n(n);
            g.open_terminal_tab(window, cx);
        });
    }

    /// Open a lifecycle-script terminal tab (setup/run/cleanup) rooted at
    /// `cwd` in the active group. Delegates to `PaneGroup` so the script runs
    /// in a real, interactive PTY tab.
    pub fn open_script_terminal_tab_in_active_group(
        &mut self,
        cwd: PathBuf,
        title: SharedString,
        script: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        let n = self.take_next_terminal_n(cx);
        group.update(cx, |g, cx| {
            g.set_next_terminal_n(n);
            g.open_script_terminal_tab(cwd, title, script, window, cx);
        });
    }

    /// Open `path` as an editor tab inside the SPECIFIC group identified
    /// by `group_id` (does not consult / mutate the active-group pointer
    /// until the call succeeds). Drag-drop center-zone target uses this
    /// so a file dropped onto a non-focused pane opens there, not in the
    /// previously-focused pane.
    ///
    /// Activates the target group on success. No-op on unknown id.
    pub fn open_file_in_group(
        &mut self,
        group_id: PaneGroupId,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let target = self.groups.get(&group_id)?.clone();
        let idx = target.update(cx, |g, cx| g.open_or_activate_editor_tab(path, window, cx));
        self.set_active_group(group_id, window, cx);
        Some(idx)
    }

    /// Drag-to-split for the file-drag flow: insert a new sibling group
    /// next to `target` along the axis implied by `zone`, then open
    /// `path` as the new group's first (and only) editor tab.
    /// `Zone::Center` is rejected — merge is handled by
    /// `open_file_in_group`.
    pub fn split_and_open_file(
        &mut self,
        target: PaneGroupId,
        zone: Zone,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (axis, insert) = match zone {
            Zone::Center => return false,
            Zone::Left => (Axis::Horizontal, SplitInsert::Before),
            Zone::Right => (Axis::Horizontal, SplitInsert::After),
            Zone::Up => (Axis::Vertical, SplitInsert::Before),
            Zone::Down => (Axis::Vertical, SplitInsert::After),
        };
        // 1. Allocate the new sibling group in the layout tree. Mirrors
        // `split_and_move_tab` minus the source-tab transfer.
        let Some(GroupSplitOutcome { new_group, .. }) =
            self.manager.split_at_target(target, axis, insert)
        else {
            return false;
        };
        // 2. Create the matching `PaneGroup` entity (empty — populated by
        // the editor-tab push below).
        let group = build_group(
            self.cwd.clone(),
            self.theme,
            self.density,
            self.typography.clone(),
            self.cli_runtime.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            cx,
        );
        group.update(cx, |g, cx| g.set_chrome_width(self.chrome_w_px, cx));
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, new_group, window, cx);
        self.groups.insert(new_group, group.clone());
        self._observers.insert(new_group, group_observer);
        self._focus_observers
            .insert(new_group, group_focus_observer);
        // 3. Push the editor tab into the new group + activate it.
        group.update(cx, |g, cx| {
            g.open_or_activate_editor_tab(path, window, cx);
        });
        self.set_active_group(new_group, window, cx);
        cx.notify();
        true
    }

    /// Focus the active tab in the active group. Called when activating
    /// this project's panes so keystrokes route correctly.
    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.active_group() else {
            self.focus_handle.focus(window, cx);
            return;
        };
        group.update(cx, |g, cx| g.focus_active(window, cx));
    }

    pub fn active_editor_path(&self, cx: &App) -> Option<PathBuf> {
        self.active_group()
            .and_then(|g| g.read(cx).active_editor_path(cx))
    }

    /// Walk every group + every tab in DFS group order; yield each
    /// terminal tab's PTY scrollback bytes. Editor tabs contribute an
    /// empty buffer (ordinal alignment with the saved blob's tab list).
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &App) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for id in self.manager.in_order_groups() {
            if let Some(group) = self.groups.get(&id) {
                out.extend(group.read(cx).collect_pane_buffers(max_bytes, cx));
            }
        }
        out
    }

    pub fn collect_pane_external_ids(&self, cx: &App) -> Vec<Option<String>> {
        let mut out = Vec::new();
        for id in self.manager.in_order_groups() {
            if let Some(group) = self.groups.get(&id) {
                out.extend(group.read(cx).collect_pane_external_ids(cx));
            }
        }
        out
    }

    /// Spawn a freshly-built agent tab in the active group. Mirrors the
    /// legacy `WorkspaceTabs::push_agent_tab` signature so the spawn
    /// chain in `WorkspaceRoot::spawn_agent_tab` keeps shape.
    #[allow(clippy::too_many_arguments)]
    pub fn push_agent_tab(
        &mut self,
        adapter: AgentAdapter,
        adapter_id: &'static str,
        worktree_path: PathBuf,
        model: Option<String>,
        effort: Option<String>,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        label_override: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_agent_tab(
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
                backend,
                term_id,
                label_override,
                window,
                cx,
            );
        });
    }

    /// Workspace snapshot. v3 schema captures both the flat (legacy) tab
    /// projection AND the workspace-level group split tree + per-group
    /// state so multi-group layouts survive restart. Custom-agent tabs
    /// are skipped (their `(program, args)` config isn't captured).
    ///
    /// **Flat fields (back-compat):**
    /// - `tabs` — every group's tabs concatenated in DFS group order.
    /// - `active` — flat index of the focused tab.
    /// - `tab_order` — flat projection of all per-group orders, in DFS group order.
    ///
    /// **Multi-group fields (v3):**
    /// - `group_tree` — `PersistedTree` mirror of `manager.group_tree()`.
    /// - `groups` — per-leaf state (`tab_count` + local `active` + local `tab_order`).
    /// - `active_group` — DFS index of the focused group.
    ///
    /// The flat + multi-group views agree at all times: each
    /// `PersistedGroup.tab_count` is a contiguous slice of the flat `tabs`
    /// vec, in DFS leaf order. Legacy readers ignore the new fields and
    /// rebuild a single group from the flat slice; v3 readers walk
    /// `group_tree` instead.
    pub fn snapshot(&self, cx: &App) -> PersistedTabs {
        let mut tabs: Vec<PersistedTab> = Vec::new();
        let mut groups: Vec<PersistedGroup> = Vec::new();
        // AgentChat transcripts collected from live views as we walk the tabs;
        // written to per-session settings keys by `save_persisted_tabs`.
        let mut chat_transcripts: Vec<crate::persisted_chat::PersistedChatTranscript> = Vec::new();
        let mut active_offset: Option<usize> = None;
        let mut active_group_dfs_idx: Option<usize> = None;
        let active_group_id = self.manager.active_group_id();
        for (dfs_idx, group_id) in self.manager.in_order_groups().into_iter().enumerate() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            let group_active = group_ref.active();
            // Per-group emit bookkeeping. `orig_to_emitted` maps the
            // group's source tab index → its position WITHIN this group's
            // emitted slice. Custom-agent tabs drop out, shifting every
            // surviving tab's local position; this map keeps `tab_order`
            // projection honest across the skip.
            let mut emitted_in_group: usize = 0;
            let mut orig_to_emitted: HashMap<usize, usize> = HashMap::new();
            let mut local_active: usize = 0;
            let slice_start = tabs.len();
            for (idx, tab) in group_ref.tabs().iter().enumerate() {
                let (agent, kind) = match &tab.kind {
                    PaneGroupTabKind::Terminal => (None, PersistedTabKind::Terminal),
                    PaneGroupTabKind::Editor { path } => (
                        None,
                        PersistedTabKind::Editor {
                            path: path.display().to_string(),
                        },
                    ),
                    // Diff, commit-detail, and Tasks tabs are intentionally NOT
                    // persisted. Diff/commit regenerate from current git state;
                    // Tasks reopens from the nav rail after restore.
                    // Skip the slot so the persisted tab list stays compact.
                    PaneGroupTabKind::Diff { .. }
                    | PaneGroupTabKind::Commit { .. }
                    | PaneGroupTabKind::BranchFile { .. }
                    | PaneGroupTabKind::CombinedDiff { .. }
                    | PaneGroupTabKind::Tasks => continue,
                    // Agent Chat: persist the tab kind (cwd/model/session id)
                    // plus, when a turn completed, the transcript blob (drained
                    // to its own settings key by `save_persisted_tabs`). A chat
                    // with no session id yet restores fresh.
                    PaneGroupTabKind::AgentChat { cwd, model } => {
                        let session_id = if let crate::shell::pane_content::PaneContent::AgentChat(
                            view,
                        ) = &tab.content
                        {
                            let v = view.read(cx);
                            if let Some(t) = v.transcript_snapshot() {
                                chat_transcripts.push(t);
                            }
                            v.session_id().map(str::to_string)
                        } else {
                            None
                        };
                        (
                            None,
                            PersistedTabKind::AgentChat {
                                cwd: cwd.display().to_string(),
                                model: model.clone(),
                                session_id,
                            },
                        )
                    }
                    // Browser tabs persist their LIVE url (read from the
                    // BrowserView) so a restored tab reopens where the user
                    // left off, including link-click navigations.
                    PaneGroupTabKind::Browser { url } => {
                        let (live, profile_id) =
                            if let crate::shell::pane_content::PaneContent::Browser(view) =
                                &tab.content
                            {
                                let v = view.read(cx);
                                (v.current_url(), v.profile_id())
                            } else {
                                (url.clone(), None)
                            };
                        (
                            None,
                            PersistedTabKind::Browser {
                                url: live,
                                profile_id,
                            },
                        )
                    }
                    PaneGroupTabKind::Agent {
                        adapter,
                        adapter_id,
                        worktree_path,
                        model,
                        effort,
                        ..
                    } => {
                        if matches!(adapter, AgentAdapter::Custom) {
                            continue;
                        }
                        // Capture the live daemon PTY id (if the agent ran
                        // through the relay) so restore can re-attach to the
                        // still-running CLI instead of respawning it. Paired
                        // with the current relay session id — restore only
                        // re-attaches when both still match.
                        let relay_external_id =
                            if let crate::shell::pane_content::PaneContent::Terminal(tree) =
                                &tab.content
                            {
                                tree.active_view().and_then(|v| v.read(cx).external_id())
                            } else {
                                None
                            };
                        // Cached session id only — no daemon round-trip on the
                        // capture path (the full `relay_state_snapshot` ListPtys
                        // RPC isn't needed here; we never read live ids).
                        let relay_session = relay_external_id
                            .as_ref()
                            .and_then(|_| crate::shell::terminal_view::relay_session_id_cached());
                        (
                            Some(PersistedAgentTab {
                                adapter: *adapter,
                                adapter_id: (*adapter_id).to_string(),
                                worktree_path: worktree_path.display().to_string(),
                                model: model.clone(),
                                effort: effort.clone(),
                                relay_external_id,
                                relay_session,
                            }),
                            PersistedTabKind::Terminal,
                        )
                    }
                };
                if group_id == active_group_id && idx == group_active {
                    active_offset = Some(tabs.len());
                    active_group_dfs_idx = Some(dfs_idx);
                    local_active = emitted_in_group;
                }
                // For terminal tabs (non-agent), capture the sub-pane
                // tree topology + per-leaf cwd. Agent tabs are always
                // single-sub-pane; skipping keeps their blobs minimal.
                let (sub_tree, sub_panes, active_sub_pane) =
                    if matches!(tab.kind, PaneGroupTabKind::Terminal) && agent.is_none() {
                        snapshot_sub_pane_tree(tab, cx)
                    } else {
                        (PersistedTree::Leaf, Vec::new(), 0)
                    };
                tabs.push(PersistedTab {
                    label: tab.label.to_string(),
                    tree: sub_tree,
                    agent,
                    kind,
                    sub_panes,
                    active_sub_pane,
                    is_preview: tab.is_preview,
                    pinned: tab.pinned,
                    color: tab.color.map(|c| c.slug().to_string()),
                    custom_title: tab.custom_title.as_ref().map(|t| t.to_string()),
                });
                orig_to_emitted.insert(idx, emitted_in_group);
                emitted_in_group += 1;
            }
            // Per-group visual order → LOCAL indices (within this group's
            // slice). Empty when every tab was custom-agent.
            let mut local_order: Vec<usize> = Vec::with_capacity(emitted_in_group);
            for &orig_local in group_ref.tab_order_iter() {
                if let Some(&emitted) = orig_to_emitted.get(&orig_local) {
                    local_order.push(emitted);
                }
            }
            // Always record a PersistedGroup — even when empty — so
            // `groups.len()` matches `group_tree.leaf_count()`. The
            // restorer skips zero-tab groups (they consume nothing off
            // the flat tabs iterator), and an all-custom-agent group
            // restores as a fresh empty group. `slice_start` is the
            // first index of THIS group's slice; keeps invariants
            // visible in debug builds.
            debug_assert_eq!(slice_start + emitted_in_group, tabs.len());
            groups.push(PersistedGroup {
                tab_count: emitted_in_group,
                active: local_active,
                tab_order: local_order,
            });
        }
        // Flat tab_order — concatenate per-group local orders into global
        // indices. Tracks the same DFS group order as `groups`.
        let mut tab_order: Vec<usize> = Vec::with_capacity(tabs.len());
        let mut flat_base = 0usize;
        for group_snap in &groups {
            for &local in &group_snap.tab_order {
                tab_order.push(flat_base + local);
            }
            flat_base += group_snap.tab_count;
        }
        // Group tree mirror. Only emit when we have >1 group with the
        // shape matching the live tree — single-group snapshots stay
        // maximally back-compat with v2 readers (which expect
        // `group_tree: null`).
        let live_leaf_count = self.manager.group_tree().leaf_count();
        let multi_group = groups.len() > 1 && groups.len() == live_leaf_count;
        let group_tree =
            multi_group.then(|| snapshot_tree::<PaneGroupId>(self.manager.group_tree()));
        let active_group = active_group_dfs_idx
            .unwrap_or(0)
            .min(groups.len().saturating_sub(1));
        PersistedTabs {
            tabs,
            active: active_offset.unwrap_or(0),
            next_label_n: 1,
            tab_order,
            group_tree,
            groups: if multi_group { groups } else { Vec::new() },
            active_group,
            chat_transcripts,
        }
    }

    /// Build a snapshot + hand it to the save callback. No-op when no
    /// callback is registered.
    ///
    /// Takes `&App` (not `&mut Context<Self>`) so the on-quit hook in
    /// `main.rs` — which only has `&App` in its closure — can call it.
    /// Snapshotting is a pure read of `self` + per-leaf grid reads; no
    /// mutation, so a shared-ref context is sufficient.
    pub fn save_now(&self, cx: &gpui::App) {
        if let Some(cb) = self.save_callback.clone() {
            let snap = self.snapshot(cx);
            cb(snap);
        }
    }

    /// Append a restored terminal tab to the active group. Used by the
    /// project-panes factory.
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| g.push_restored_terminal_tab(label, view, cx));
    }

    /// Re-apply restored cosmetics + visual rank to the tab just pushed
    /// into `group_id` (or the active group when `None`). The factory calls
    /// this right after each synchronous restore push so the tab regains its
    /// color/title/pin/preview state and settles into its saved strip slot.
    pub fn place_restored_last_tab(
        &mut self,
        group_id: Option<PaneGroupId>,
        meta: RestoredTabMeta,
        cx: &mut Context<Self>,
    ) {
        let group = match group_id {
            Some(id) => self.groups.get(&id).cloned(),
            None => self.active_group(),
        };
        let Some(group) = group else {
            return;
        };
        group.update(cx, |g, cx| {
            let count = g.tab_count();
            if count > 0 {
                g.place_restored_tab(count - 1, meta, cx);
            }
        });
    }

    /// Multi-sub-pane restore: append a terminal tab whose
    /// `TerminalSplitTree` has been fully reconstructed by the factory
    /// (per-leaf views + observers + tree shape + active position).
    /// Targets the active group; the `_in` variant takes an explicit
    /// group id for multi-group restore.
    pub fn push_restored_terminal_tab_with_tree(
        &mut self,
        label: String,
        tree: crate::shell::pane_group::sub_pane::TerminalSplitTree,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_restored_terminal_tab_with_tree(label, tree, cx)
        });
    }

    /// Multi-sub-pane restore into a SPECIFIC group (multi-group restore
    /// path). No-op when `group_id` isn't registered.
    pub fn push_restored_terminal_tab_with_tree_in(
        &mut self,
        group_id: PaneGroupId,
        label: String,
        tree: crate::shell::pane_group::sub_pane::TerminalSplitTree,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id) else {
            return;
        };
        group.update(cx, |g, cx| {
            g.push_restored_terminal_tab_with_tree(label, tree, cx)
        });
    }

    /// Append a restored agent tab to the active group. Builds the view
    /// internally so the call shape mirrors the legacy strip.
    #[allow(clippy::too_many_arguments)]
    pub fn push_restored_agent_tab(
        &mut self,
        persisted: &PersistedAgentTab,
        adapter_id: &'static str,
        label: String,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        meta: RestoredTabMeta,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            let idx = g.push_agent_tab(
                persisted.adapter,
                adapter_id,
                PathBuf::from(&persisted.worktree_path),
                persisted.model.clone(),
                persisted.effort.clone(),
                session_id,
                status_rx,
                backend,
                term_id,
                Some(label),
                window,
                cx,
            );
            // Agent tabs mount async — after the persisted `tab_order` was
            // already applied — so this re-settles the tab into its saved
            // visual slot (and restores its color/title/pin/preview state).
            g.place_restored_tab(idx, meta, cx);
        });
    }

    /// Finalize a restore: re-apply the saved visual `tab_order`,
    /// activate the requested tab inside the single restored group,
    /// focus it, and trigger a render. `tab_order.is_empty()` means
    /// the snapshot was pre-v2 with no order field — caller passes the
    /// empty vec and the method skips the re-order pass (insertion
    /// order survives as the visual order, which matches pre-v2 behavior).
    pub fn apply_restored_state(
        &mut self,
        active: usize,
        _next_label_n: u64,
        tab_order: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        group.update(cx, |g, cx| {
            if !tab_order.is_empty() {
                g.set_tab_order(tab_order);
            }
            if active < g.tab_count() {
                g.set_active(active, window, cx);
            }
            g.focus_active(window, cx);
        });
        cx.notify();
    }

    /// Multi-group restore (v3 snapshot path). Replaces the placeholder
    /// initial group with N empty groups arranged per `persisted_tree`
    /// and returns their freshly-allocated ids in DFS leaf order. Caller
    /// then pushes each group's tabs via `push_restored_terminal_tab_in`
    /// / `open_editor_in_group_restore` / `push_restored_agent_tab_in`,
    /// and finally calls `apply_restored_state_multi` to apply per-group
    /// active + tab_order + activate the saved focused group.
    pub fn rebuild_groups_from_tree(
        &mut self,
        persisted_tree: &PersistedTree,
        active_group_dfs_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<PaneGroupId> {
        // Walk the persisted tree to allocate fresh PaneGroupIds in DFS
        // order. The closure pushes each newly-minted id into
        // `allocated`, so the returned tree's leaves and the vec stay
        // index-aligned.
        let mut allocated: Vec<PaneGroupId> = Vec::new();
        let mut next_raw: u64 = 1;
        let restored_tree =
            crate::persisted_terminals::restore_tree::<PaneGroupId, _>(persisted_tree, &mut || {
                let id = PaneGroupId(next_raw);
                next_raw += 1;
                allocated.push(id);
                id
            });
        // Drop placeholder group(s) + every observer wired to them.
        self.groups.clear();
        self._observers.clear();
        self._focus_observers.clear();
        // Resolve active group id; default to the first leaf when the
        // saved index is stale (defensive against truncated blobs).
        let active = allocated
            .get(active_group_dfs_idx)
            .copied()
            .or_else(|| allocated.first().copied())
            .unwrap_or(PaneGroupId(0));
        self.manager = PaneGroupManager::from_tree(restored_tree, active, next_raw);
        // Build empty PaneGroup entities for each allocated id.
        for &id in &allocated {
            let group = build_group(
                self.cwd.clone(),
                self.theme,
                self.density,
                self.typography.clone(),
                self.cli_runtime.clone(),
                self.notifier.clone(),
                self.window_active.clone(),
                cx,
            );
            group.update(cx, |g, cx| g.set_chrome_width(self.chrome_w_px, cx));
            let group_observer = observe_group(&group, cx);
            let group_focus_observer = observe_group_focus(&group, id, window, cx);
            self.groups.insert(id, group);
            self._observers.insert(id, group_observer);
            self._focus_observers.insert(id, group_focus_observer);
        }
        cx.notify();
        allocated
    }

    /// Push a restored terminal tab into a SPECIFIC group (multi-group
    /// restore path). No-op when `group_id` isn't registered.
    pub fn push_restored_terminal_tab_in(
        &mut self,
        group_id: PaneGroupId,
        label: String,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id) else {
            return;
        };
        group.update(cx, |g, cx| g.push_restored_terminal_tab(label, view, cx));
    }

    /// Open an editor tab inside a SPECIFIC group during multi-group
    /// restore. Bypasses the active-group resolution so a file dropped
    /// in a non-focused group is restored to the same group it was
    /// captured from. No-op when `group_id` isn't registered.
    pub fn open_editor_in_group_restore(
        &mut self,
        group_id: PaneGroupId,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id).cloned() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.open_or_activate_editor_tab(path, window, cx);
        });
    }

    /// Restore a browser tab into a SPECIFIC group (multi-group restore).
    pub fn open_browser_in_group_restore(
        &mut self,
        group_id: PaneGroupId,
        url: String,
        profile_id: Option<uuid::Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id).cloned() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.open_browser_tab(url, profile_id, window, cx);
        });
    }

    /// Restore an Agent Chat tab into a SPECIFIC group (multi-group restore).
    /// Rehydrates the transcript + resumes the session. No-op when `group_id`
    /// isn't registered.
    #[allow(clippy::too_many_arguments)]
    pub fn open_agent_chat_in_group_restore(
        &mut self,
        group_id: PaneGroupId,
        cwd: PathBuf,
        model: Option<String>,
        session_id: Option<String>,
        entries: Vec<oximux_agents::thread::ThreadEntry>,
        slash_commands: Vec<String>,
        thinking_level: crate::shell::agent_chat::ThinkingLevel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id).cloned() else {
            return;
        };
        group.update(cx, |g, cx| {
            g.open_agent_chat_tab_restored(
                cwd, model, session_id, entries, slash_commands, thinking_level, window, cx,
            );
        });
    }

    /// Push a restored agent tab into a SPECIFIC group. Multi-group
    /// counterpart of `push_restored_agent_tab` — the active-group
    /// pointer is irrelevant here because the target is named
    /// explicitly. No-op when `group_id` isn't registered.
    #[allow(clippy::too_many_arguments)]
    pub fn push_restored_agent_tab_in(
        &mut self,
        group_id: PaneGroupId,
        persisted: &PersistedAgentTab,
        adapter_id: &'static str,
        label: String,
        session_id: AgentSessionId,
        status_rx: AgentStatusStream,
        backend: SharedBackend,
        term_id: TerminalSessionId,
        meta: RestoredTabMeta,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.groups.get(&group_id).cloned() else {
            return;
        };
        group.update(cx, |g, cx| {
            let idx = g.push_agent_tab(
                persisted.adapter,
                adapter_id,
                PathBuf::from(&persisted.worktree_path),
                persisted.model.clone(),
                persisted.effort.clone(),
                session_id,
                status_rx,
                backend,
                term_id,
                Some(label),
                window,
                cx,
            );
            g.place_restored_tab(idx, meta, cx);
        });
    }

    /// Finalize multi-group restore. Applies per-group `tab_order` +
    /// local `active` to each registered group, then activates the
    /// saved focused group + focuses its active tab. Mirrors
    /// `apply_restored_state` for the multi-group path.
    pub fn apply_restored_state_multi(
        &mut self,
        per_group: Vec<(PaneGroupId, Vec<usize>, usize)>,
        active_group: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (group_id, tab_order, active) in per_group {
            let Some(group) = self.groups.get(&group_id).cloned() else {
                continue;
            };
            group.update(cx, |g, cx| {
                if !tab_order.is_empty() {
                    g.set_tab_order(tab_order);
                }
                if active < g.tab_count() {
                    g.set_active(active, window, cx);
                }
            });
        }
        self.set_active_group(active_group, window, cx);
        cx.notify();
    }

    /// Persist scrollback bytes for every terminal tab keyed by
    /// `(project_id, ordinal)`. Ordinal counts terminal tabs in DFS
    /// group + per-group order; agent tabs are skipped (`CliRuntime`
    /// reloads their own history on restore).
    pub fn capture_pane_buffers(
        &self,
        repo: &PaneBufferRepo,
        project_id: &str,
        window_id: &str,
        max_bytes_per_pane: usize,
        cx: &App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id, window_id) {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "pane_buffers: delete_for_project failed"
            );
            return;
        }
        let mut ordinal: u32 = 0;
        for group_id in self.manager.in_order_groups() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            for tab in group_ref.tabs() {
                if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                    continue;
                }
                let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                // F3.4: capture EVERY live sub-pane's scrollback (not
                // just the active one). `sub_pane_ordinal` is the DFS
                // leaf position inside this tab; matches the DFS-leaf
                // order the restorer uses to dispatch bytes back into
                // each sub-pane. Single-sub-pane tabs end up writing
                // one row at `(ordinal, 0)`, identical to pre-F3.4.
                for (sub_pane_ordinal, slot) in tree.tree().in_order_leaves().iter().enumerate() {
                    let Some(view) = tree.get(*slot) else {
                        continue;
                    };
                    let bytes = view.read(cx).serialize_buffer(max_bytes_per_pane);
                    if bytes.is_empty() {
                        continue;
                    }
                    if let Err(err) = repo.set(
                        project_id,
                        window_id,
                        ordinal,
                        sub_pane_ordinal as u32,
                        &bytes,
                    ) {
                        tracing::warn!(
                            ?err,
                            project_id,
                            window_id,
                            ordinal,
                            sub_pane_ordinal,
                            "pane_buffers: set failed"
                        );
                    }
                }
                ordinal += 1;
            }
        }
    }

    /// Walk every terminal tab in DFS group order and persist each
    /// leaf's relay-side PTY id, if any. Same ordinal-counting rules
    /// as `capture_pane_buffers`.
    pub fn capture_pane_relay_ids(
        &self,
        repo: &PaneRelayIdRepo,
        project_id: &str,
        window_id: &str,
        relay_session_id: &str,
        cx: &App,
    ) {
        if let Err(err) = repo.delete_for_project(project_id, window_id) {
            tracing::warn!(
                ?err,
                project_id,
                window_id,
                "pane_relay_ids: delete_for_project failed"
            );
            return;
        }
        let mut ordinal: u32 = 0;
        for group_id in self.manager.in_order_groups() {
            let Some(group) = self.groups.get(&group_id) else {
                continue;
            };
            let group_ref = group.read(cx);
            for tab in group_ref.tabs() {
                if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                    continue;
                }
                let crate::shell::pane_content::PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                // Persist a relay id for EVERY live leaf-tab, keyed
                // (ordinal, sub_pane, tab), so split leaves and background
                // per-pane tabs re-attach their surviving daemon PTYs
                // independently on the next launch instead of dormant-
                // respawning. `sub_pane` is the leaf's DFS position (same
                // order `snapshot_sub_pane_tree` and `pane_buffers` use);
                // `tab` is its per-pane tab index. Views with no relay id
                // (in-process backend) are skipped — those leaves still
                // dormant-respawn, unchanged.
                for (sub_pane, slot) in tree.tree().in_order_leaves().into_iter().enumerate() {
                    let Some(leaf) = tree.leaf(slot) else {
                        continue;
                    };
                    for (tab_idx, lt) in leaf.tabs().iter().enumerate() {
                        // `relay_id_for_capture` (not `external_id`): a view
                        // still awaiting its post-paint attach answers with
                        // the persisted hint, so a quit that races the
                        // reconcile keeps the row instead of orphaning the
                        // daemon PTY.
                        if let Some(pty_id) = lt.view().read(cx).relay_id_for_capture()
                            && let Err(err) = repo.set(
                                project_id,
                                window_id,
                                ordinal,
                                sub_pane as u32,
                                tab_idx as u32,
                                &pty_id,
                                relay_session_id,
                            )
                        {
                            tracing::warn!(
                                ?err,
                                project_id,
                                window_id,
                                ordinal,
                                sub_pane,
                                tab_idx,
                                "pane_relay_ids: set failed"
                            );
                        }
                    }
                }
                ordinal += 1;
            }
        }
    }
}
