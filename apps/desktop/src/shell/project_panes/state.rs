use super::*;

impl ProjectPanes {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let window_active = Arc::new(AtomicBool::new(window.is_window_active()));
        let window_active_for_observer = window_active.clone();
        let observer = cx.observe_window_activation(window, move |_this, window, _cx| {
            window_active_for_observer.store(window.is_window_active(), Ordering::Relaxed);
        });
        let manager = PaneGroupManager::new();
        let initial_id = manager.active_group_id();
        let group = build_group(
            cwd.clone(),
            theme,
            density,
            typography.clone(),
            cli_runtime.clone(),
            notifier.clone(),
            window_active.clone(),
            cx,
        );
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, initial_id, window, cx);
        let mut groups = HashMap::new();
        groups.insert(initial_id, group);
        let mut observers = HashMap::new();
        observers.insert(initial_id, group_observer);
        let mut focus_observers = HashMap::new();
        focus_observers.insert(initial_id, group_focus_observer);
        Self {
            manager,
            groups,
            _observers: observers,
            _focus_observers: focus_observers,
            focus_handle: cx.focus_handle(),
            cwd,
            theme,
            density,
            typography,
            cli_runtime,
            notifier,
            window_active,
            _window_activation_observer: observer,
            save_callback: None,
            chrome_w_px: density.w_left_rail,
            workspace_tint: None,
            hovered_drop_target: None,
            next_terminal_n: 1,
            last_active_group: None,
            rim_flash_token: 0,
            active_divider: None,
            divider_bounds: DividerBoundsCache::default(),
            last_divider_path: None,
        }
    }

    /// Pull-and-bump the workspace-global terminal counter. Called right
    /// before every shell spawn so labels stay monotonic across panes.
    /// Walks every existing tab label, parses any `Terminal N` suffix,
    /// and floors the counter at `max(N) + 1` so restored sessions don't
    /// re-issue colliding labels (the per-group counter resets on app
    /// boot but labels persist).
    pub(super) fn take_next_terminal_n(&mut self, cx: &App) -> u64 {
        let mut highest = self.next_terminal_n.saturating_sub(1);
        for group in self.groups.values() {
            let g = group.read(cx);
            highest = highest.max(g.next_terminal_n_peek().saturating_sub(1));
            for (_, tab) in g.visible_tabs() {
                if let Some(rest) = tab.label.strip_prefix("Terminal ")
                    && let Ok(parsed) = rest.parse::<u64>()
                {
                    highest = highest.max(parsed);
                }
            }
        }
        let n = highest + 1;
        self.next_terminal_n = n + 1;
        n
    }

    pub fn manager(&self) -> &PaneGroupManager {
        &self.manager
    }

    pub fn active_group(&self) -> Option<Entity<PaneGroup>> {
        self.groups.get(&self.manager.active_group_id()).cloned()
    }

    /// The active group's active-tab Agent Chat view, if any. Used by the
    /// `CreateWorktreeWorkspaceForActiveChat` route-up handler to reach the chat
    /// that armed the worktree draft (it's the focused tab that just sent).
    pub fn active_agent_chat_view(
        &self,
        cx: &App,
    ) -> Option<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.active_group()?.read(cx).active_agent_chat_view()
    }

    /// The chat view bound to `remote_session_id` across every group in this
    /// project. Used by the remote rewind path, which addresses a session by id
    /// with no idea which group or tab holds it.
    pub fn agent_chat_view_by_remote_id(
        &self,
        remote_session_id: &str,
        cx: &App,
    ) -> Option<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.groups
            .values()
            .find_map(|g| g.read(cx).agent_chat_view_by_remote_id(remote_session_id, cx))
    }

    pub fn group(&self, id: PaneGroupId) -> Option<Entity<PaneGroup>> {
        self.groups.get(&id).cloned()
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn set_save_callback(&mut self, cb: SaveCallback) {
        self.save_callback = Some(cb);
    }

    pub fn window_active(&self) -> bool {
        self.window_active.load(Ordering::Relaxed)
    }

    pub fn agent_count(&self, cx: &App) -> usize {
        self.groups.values().map(|g| g.read(cx).agent_count()).sum()
    }

    /// Count of plain-terminal tabs running a hand-launched agent across all
    /// groups. Added to `agent_count` for the status-bar total so a manually
    /// started `claude`/`codex`/… shows up in "N agents".
    pub fn ambient_agent_count(&self, cx: &App) -> usize {
        self.groups
            .values()
            .map(|g| g.read(cx).ambient_agent_count(cx))
            .sum()
    }

    /// Pick a target agent session for "send to active agent" actions.
    /// Preference order: (1) the active group's active tab when it's an
    /// agent (most-direct routing for the common "terminal + agent side
    /// by side" layout); (2) any group's active tab that's an agent; (3)
    /// any agent tab anywhere. Returns `None` when no agent is open.
    /// The agent whose tab is the active pane, for the rail's focused-row
    /// highlight. Resolves the active group's active tab only — no fallback to
    /// background tabs or other groups (unlike `target_agent_session`), so the
    /// highlight tracks exactly what the user is looking at. `None` when the
    /// active tab is not an agent surface.
    pub fn focused_rail_agent(
        &self,
        cx: &App,
    ) -> Option<crate::shell::pane_group::FocusedRailAgent> {
        self.active_group()?.read(cx).focused_rail_agent(cx)
    }

    /// Resolve which agent a "send to agent" action targets. Preference order,
    /// most-specific first, so the send lands on the agent the user is actually
    /// working with rather than an arbitrary one when several exist:
    ///   1. agent in the active tab (terminal + agent side by side),
    ///   2. most-recently-active agent in the active group (you were just in it
    ///      before focusing a terminal — the common "run a command, send its
    ///      output to the agent" flow),
    ///   3. then the same two, widened to any group,
    ///   4. finally any agent at all (last resort).
    pub fn target_agent_session(&self, cx: &App) -> Option<AgentSessionId> {
        if let Some(active) = self.active_group() {
            let group = active.read(cx);
            if let Some(id) = group.active_agent_session() {
                return Some(id);
            }
            if let Some(id) = group.mru_agent_session() {
                return Some(id);
            }
        }
        for group in self.groups.values() {
            if let Some(id) = group.read(cx).active_agent_session() {
                return Some(id);
            }
        }
        for group in self.groups.values() {
            if let Some(id) = group.read(cx).mru_agent_session() {
                return Some(id);
            }
        }
        self.groups
            .values()
            .find_map(|g| g.read(cx).first_agent_session())
    }

    pub fn tab_count(&self, cx: &App) -> usize {
        self.groups.values().map(|g| g.read(cx).tab_count()).sum()
    }

    /// Total TTY-backed tab count across every pane group. Drives the
    /// status bar's "N TTY" metric.
    pub fn tty_count(&self, cx: &App) -> usize {
        self.groups.values().map(|g| g.read(cx).tty_count()).sum()
    }

    /// Every terminal's shell pid, paired with the working directory of the
    /// group it lives in. The root set the ports scan walks down from.
    ///
    /// The cwd rather than a group id because that is the identity a person
    /// recognises — they started the server "in OxiMux", not "in group 2" —
    /// and because two groups on one project should share a heading.
    ///
    /// Terminals whose pid cannot be resolved are skipped, not reported as
    /// pid 0: an unresolvable pid means the shell has not spawned yet or its
    /// relay checkpoint is unreadable, and nothing is listening under a
    /// process that does not exist. Note that on the relay path `os_pid`
    /// falls back to reading that checkpoint file, so this is a poll-rate
    /// cost and not free — the caller gates it on window focus.
    pub fn terminal_roots(&self, cx: &App) -> Vec<(PathBuf, u32)> {
        let mut roots = Vec::new();
        for group in self.groups.values() {
            let group = group.read(cx);
            let cwd = group.cwd().clone();
            for tab in group.tabs() {
                let PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                for slot in tree.tree().in_order_leaves() {
                    let Some(leaf) = tree.leaf(slot) else {
                        continue;
                    };
                    for leaf_tab in leaf.tabs() {
                        if let Some(pid) = leaf_tab.view().read(cx).os_pid() {
                            roots.push((cwd.clone(), pid));
                        }
                    }
                }
            }
        }
        roots
    }

    pub fn is_empty(&self, cx: &App) -> bool {
        self.tab_count(cx) == 0
    }

    pub fn set_chrome_width(&mut self, chrome: f32, cx: &mut App) {
        if (self.chrome_w_px - chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = chrome;
        for group in self.groups.values() {
            group.update(cx, |g, cx| g.set_chrome_width(chrome, cx));
        }
    }

    /// Set the active workspace's tint (read at tab-strip render time for the
    /// active-tab edge accent). Stored only — the next render reads the field;
    /// pushed down by `WorkspaceRoot` alongside `set_chrome_width`.
    pub fn set_workspace_tint(&mut self, tint: Option<crate::shell::pane_group::TabColor>) {
        self.workspace_tint = tint;
    }

    /// Throttle every terminal in this (now-inactive) project by marking all
    /// views hidden. Called by `WorkspaceRoot` on a project switch because an
    /// inactive project's `PaneGroup::render` never runs to push visibility.
    /// The views still drain (output buffered); they just poll slower until
    /// the project is re-activated and its render re-syncs visibility.
    pub fn hide_all_terminals(&self, cx: &mut Context<Self>) {
        for group in self.groups.values() {
            group.update(cx, |g, gcx| g.hide_all_terminals(gcx));
        }
    }

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

    /// Activate the tab whose agent session matches `tab_id`. Walks
    /// every group; the first hit wins. Returns true if any group had
    /// the matching tab.
    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut found_group: Option<PaneGroupId> = None;
        for (id, group) in &self.groups {
            let hit = group.update(cx, |g, cx| g.set_active_by_tab_id(tab_id, window, cx));
            if hit {
                found_group = Some(*id);
                break;
            }
        }
        if let Some(id) = found_group {
            self.set_active_group(id, window, cx);
            true
        } else {
            false
        }
    }

    /// Worktree path of the agent tab matching `tab_id` anywhere in this
    /// project's groups. `Some` doubles as the ownership answer for the
    /// notification click router's cross-project search.
    pub fn agent_worktree_for_tab_id(
        &self,
        tab_id: TabId,
        cx: &gpui::App,
    ) -> Option<std::path::PathBuf> {
        self.groups
            .values()
            .find_map(|g| g.read(cx).agent_worktree_for_tab_id(tab_id))
    }

    /// The cwd of the pane group owning `session`, if any group in this
    /// project does. Read-only counterpart of `activate_terminal_session`;
    /// the bell-banner click router uses it to resolve which project (and
    /// rail workspace) a clicked banner belongs to.
    pub fn group_cwd_for_terminal_session(
        &self,
        session: oximux_pty::TerminalSessionId,
        cx: &gpui::App,
    ) -> Option<std::path::PathBuf> {
        self.groups.values().find_map(|g| {
            let group = g.read(cx);
            group
                .tab_index_for_terminal_session(session, cx)
                .map(|_| group.cwd().clone())
        })
    }

    /// Activate the tab hosting `session` in whichever group owns it and
    /// focus that group. Returns false when no group does (tab closed).
    pub fn activate_terminal_session(
        &mut self,
        session: oximux_pty::TerminalSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut found_group: Option<PaneGroupId> = None;
        for (id, group) in &self.groups {
            let hit = group.update(cx, |g, cx| {
                match g.tab_index_for_terminal_session(session, cx) {
                    Some(idx) => {
                        g.set_active(idx, window, cx);
                        true
                    }
                    None => false,
                }
            });
            if hit {
                found_group = Some(*id);
                break;
            }
        }
        if let Some(id) = found_group {
            self.set_active_group(id, window, cx);
            true
        } else {
            false
        }
    }

    /// Spawn the first Terminal tab in the only (initial) pane group.
    pub fn seed_default_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group() {
            let n = self.take_next_terminal_n(cx);
            group.update(cx, |g, cx| {
                g.set_next_terminal_n(n);
                g.open_terminal_tab(window, cx);
            });
        }
    }

    /// Current hovered drop target during a tab drag — `Some` only while
    /// the cursor is over a pane body. Render reads this to paint the
    /// 5-zone overlay on the matching group.
    pub fn hovered_drop_target(&self) -> Option<TabDragHoveredTarget> {
        self.hovered_drop_target
    }

    /// Update the hovered drop target. Triggers a re-render only when
    /// the value actually changes — `on_drag_move` fires on every pointer
    /// move at ~60 fps and unconditional notifies would thrash.
    pub fn set_hovered_drop_target(
        &mut self,
        target: Option<TabDragHoveredTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.hovered_drop_target == target {
            return;
        }
        self.hovered_drop_target = target;
        cx.notify();
    }

    /// Move a tab from `source` group into `target` group by stealing its
    /// `PaneGroupTab` (preserving the inner terminal/editor entity, PTY,
    /// scrollback). Returns `false` when source/target/idx is invalid or
    /// source==target (drop-on-self merge is a no-op).
    pub fn transfer_tab(
        &mut self,
        source: PaneGroupId,
        source_tab_idx: usize,
        target: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if source == target {
            return false;
        }
        let Some(source_entity) = self.groups.get(&source).cloned() else {
            return false;
        };
        let Some(target_entity) = self.groups.get(&target).cloned() else {
            return false;
        };
        let Some(tab) = source_entity.update(cx, |g, cx| g.take_tab(source_tab_idx, cx)) else {
            return false;
        };
        target_entity.update(cx, |g, cx| {
            g.push_existing_tab(tab, window, cx);
        });
        // Focus follows the moved tab; the manager + project-panes
        // notify chain will repaint and purge any group that's now empty.
        self.set_active_group(target, window, cx);
        cx.notify();
        true
    }

    /// Move a tab from `source` into `target` at a precise visible slot.
    /// Wraps [`Self::transfer_tab`] (which appends the stolen tab to the
    /// end of `target`) and then slides the appended tab to `visible_slot`
    /// so a cross-group strip drop lands where the insertion bar previewed
    /// rather than always at the end. Returns `false` when the transfer was
    /// refused (e.g. the source tab is pinned). Pinned clamps inside
    /// `move_tab` keep the moved tab within its bucket.
    pub fn transfer_tab_at(
        &mut self,
        source: PaneGroupId,
        source_tab_idx: usize,
        target: PaneGroupId,
        visible_slot: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.transfer_tab(source, source_tab_idx, target, window, cx) {
            return false;
        }
        let Some(target_entity) = self.groups.get(&target).cloned() else {
            return false;
        };
        target_entity.update(cx, |g, cx| {
            // `transfer_tab` appended the stolen tab to the end of the
            // visible order; its position is the last visible slot. Slide
            // it back to where the cursor previewed the insertion bar.
            let appended = g.tab_count().saturating_sub(1);
            g.move_tab(appended, visible_slot.min(appended));
            cx.notify();
        });
        true
    }

    /// Drag-to-split: insert a new sibling group next to `target` along
    /// the axis implied by `zone`, then populate it.
    ///
    /// Normally the dragged tab is **moved** into the new group. The one
    /// exception: splitting a pane whose ONLY tab is a terminal would empty
    /// the source pane (purged immediately), defeating the split — so in
    /// that case the source terminal stays put and a **fresh** terminal is
    /// spawned in the new pane instead (two populated panes, focus on the
    /// new terminal). Multi-tab panes and non-terminal tabs keep move
    /// semantics. `Zone::Center` is a no-op here (merge is `transfer_tab`).
    pub fn split_and_move_tab(
        &mut self,
        source: PaneGroupId,
        source_tab_idx: usize,
        target: PaneGroupId,
        zone: Zone,
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
        // Decide spawn-vs-move BEFORE mutating the tree: spawn a fresh
        // terminal only when the source pane holds exactly one tab and it
        // is a terminal (moving it would leave an empty, purged pane).
        let spawn_new_terminal = self
            .groups
            .get(&source)
            .map(|g| {
                let g = g.read(cx);
                g.tab_count() == 1
                    && matches!(
                        g.tabs().get(source_tab_idx).map(|t| &t.kind),
                        Some(PaneGroupTabKind::Terminal)
                    )
            })
            .unwrap_or(false);
        // 1. Allocate the new sibling group in the layout tree.
        let Some(GroupSplitOutcome { new_group, .. }) =
            self.manager.split_at_target(target, axis, insert)
        else {
            return false;
        };
        // 2. Create the matching PaneGroup entity (empty — populated below).
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
        self.groups.insert(new_group, group);
        self._observers.insert(new_group, group_observer);
        self._focus_observers
            .insert(new_group, group_focus_observer);
        // 3a. Single-terminal source → spawn a fresh terminal in the new
        // pane and leave the original in place. Focus follows the new one.
        if spawn_new_terminal {
            let n = self.take_next_terminal_n(cx);
            let spawned = self
                .groups
                .get(&new_group)
                .map(|g| {
                    g.update(cx, |g, cx| {
                        g.set_next_terminal_n(n);
                        g.open_terminal_tab(window, cx).is_some()
                    })
                })
                .unwrap_or(false);
            if !spawned {
                // PTY spawn failed — roll back the empty new group.
                let _ = self.close_group_by_id(new_group, window, cx);
                return false;
            }
            self.set_active_group(new_group, window, cx);
            cx.notify();
            return true;
        }
        // 3b. Otherwise steal the dragged tab into the new group.
        let moved = self.transfer_tab(source, source_tab_idx, new_group, window, cx);
        if !moved {
            // Roll back the empty new group — purge_empty_groups will
            // catch it on next render, but doing it inline keeps the
            // tree consistent for the next user action.
            let _ = self.close_group_by_id(new_group, window, cx);
        }
        moved
    }

    /// Apply new weights to the Split node at `path` (drag-resize handle
    /// between sibling groups). Triggers a re-render only when the call
    /// actually mutates the tree — the drag handler fires on every mouse
    /// move at ~60 fps and unconditional notifies would thrash.
    pub fn set_split_weights(
        &mut self,
        path: &[usize],
        new_weights: Vec<f32>,
        cx: &mut Context<Self>,
    ) -> bool {
        let prev = self.manager.group_tree().split_weights(path);
        let changed = self.manager.set_split_weights(path, new_weights);
        if !changed {
            return false;
        }
        let next = self.manager.group_tree().split_weights(path);
        if prev == next {
            return false;
        }
        cx.notify();
        true
    }

    /// Shared bounds cache handle for the render pass to record split-row
    /// geometry into (one `Rc` clone per render).
    pub fn divider_bounds_cache(&self) -> DividerBoundsCache {
        self.divider_bounds.clone()
    }

    /// The divider currently being resized, if any.
    pub fn active_divider(&self) -> Option<&ActiveDivider> {
        self.active_divider.as_ref()
    }

    /// Arm a workspace divider for mouse-capture resize. Called from the
    /// divider hitbox MouseDown once the parent split-row bounds are known.
    pub fn arm_divider(&mut self, active: ActiveDivider, cx: &mut Context<Self>) {
        self.last_divider_path = Some(active.split_path.clone());
        self.active_divider = Some(active);
        cx.notify();
    }

    /// Reset the split the user just double-clicked. Resolves the target
    /// from the armed divider, or — if the first click's arm was already
    /// disarmed — the most-recently-armed path, so the reset still lands
    /// when the capture overlay is the element under the second click.
    pub fn reset_split_double_click(&mut self, cx: &mut Context<Self>) {
        let path = self
            .active_divider
            .as_ref()
            .map(|a| a.split_path.clone())
            .or_else(|| self.last_divider_path.clone());
        if let Some(path) = path {
            self.reset_split_to_equal(&path, cx);
        }
        self.disarm_divider(cx);
    }

    /// Apply a resize for the armed divider from the cursor position.
    /// No-op when nothing is armed. Returns `true` if weights changed.
    pub fn resize_active_divider(
        &mut self,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_divider.clone() else {
            return false;
        };
        let Some(frac) = crate::shell::divider::fraction_along(&active, pos) else {
            return false;
        };
        let new_weights =
            render::redistribute_weights(&active.initial_weights, active.divider_idx, frac);
        self.set_split_weights(&active.split_path, new_weights, cx)
    }

    /// Disarm the active divider (mouse released). No-op when nothing armed.
    pub fn disarm_divider(&mut self, cx: &mut Context<Self>) {
        if self.active_divider.take().is_some() {
            cx.notify();
        }
    }

    /// Reset the split at `path` to equal weights (double-click a divider).
    pub fn reset_split_to_equal(&mut self, path: &[usize], cx: &mut Context<Self>) {
        let Some(weights) = self.manager.group_tree().split_weights(path) else {
            return;
        };
        let equal = vec![1.0; weights.len()];
        self.set_split_weights(path, equal, cx);
    }

    pub fn split_active_group(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneGroupId> {
        let GroupSplitOutcome { new_group, .. } = self.manager.split_active_group(axis, insert)?;
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
        let n = self.take_next_terminal_n(cx);
        group.update(cx, |g, cx| {
            g.set_chrome_width(self.chrome_w_px, cx);
            g.set_next_terminal_n(n);
            g.open_terminal_tab(window, cx);
        });
        let group_observer = observe_group(&group, cx);
        let group_focus_observer = observe_group_focus(&group, new_group, window, cx);
        self.groups.insert(new_group, group);
        self._observers.insert(new_group, group_observer);
        self._focus_observers
            .insert(new_group, group_focus_observer);
        if let Some(group) = self.groups.get(&new_group) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
        Some(new_group)
    }

    pub fn close_active_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaneGroupId, CloseGroupError> {
        let closed = self.manager.close_active_group()?;
        self.groups.remove(&closed);
        self._observers.remove(&closed);
        self._focus_observers.remove(&closed);
        if let Some(group) = self.groups.get(&self.manager.active_group_id()) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
        Ok(closed)
    }

    /// Close a specific group by id (no-op when unknown or last).
    pub fn close_group_by_id(
        &mut self,
        id: PaneGroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaneGroupId, CloseGroupError> {
        if !self.manager.set_active(id) {
            return Err(CloseGroupError::NotFound);
        }
        self.close_active_group(window, cx)
    }

    /// Pre-render sweep: drop any group whose tabs hit zero. Refuses
    /// to close the last group — that path falls through to render-as-
    /// empty so the user still has a stable container.
    pub(crate) fn purge_empty_groups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() <= 1 {
            return;
        }
        let empties: Vec<PaneGroupId> = self
            .groups
            .iter()
            .filter_map(|(id, g)| g.read(cx).is_empty().then_some(*id))
            .collect();
        for id in empties {
            if self.groups.len() <= 1 {
                break;
            }
            let _ = self.close_group_by_id(id, window, cx);
        }
    }

    /// Reshape the workspace pane layout into `preset`.
    ///
    /// For `Preset::BottomTerminal` this method walks the existing groups to
    /// find the last group in DFS order that holds at least one terminal tab
    /// and passes it to the pure transform as the bottom-docked leaf. If no
    /// terminal group exists the method falls back to Stacked (no new group
    /// is spawned — adding spawn here would require `Window` + `Context` and
    /// entangle the pure reshape with side-effects; callers that want an
    /// auto-spawned terminal can do so before calling this method).
    ///
    /// Focus is restored to whichever group was active before the reshape;
    /// the caller should focus that group's active tab after this returns.
    pub fn apply_layout_preset(
        &mut self,
        preset: Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // For BottomTerminal, find a terminal-bearing group to dock at bottom.
        let terminal_id = if preset == Preset::BottomTerminal {
            // Prefer the last terminal group in DFS order so a freshly-split
            // pane ends up at the bottom (matches the "dock newest terminal"
            // expectation).
            let existing = self.manager.in_order_groups().into_iter().rev().find(|id| {
                self.groups
                    .get(id)
                    .map(|g| g.read(cx).tty_count() > 0)
                    .unwrap_or(false)
            });
            match existing {
                Some(id) => Some(id),
                // No terminal group exists yet — spawn one. `split_active_group`
                // creates a fresh group and opens a terminal tab in it; the
                // returned id is then docked at the bottom by the reshape, so
                // Bottom Terminal always lands on a real terminal.
                None => self.split_active_group(Axis::Vertical, SplitInsert::After, window, cx),
            }
        } else {
            None
        };

        self.manager.apply_layout_preset(preset, terminal_id);

        // Re-focus the active group so the focused pane stays active.
        let active_id = self.manager.active_group_id();
        if let Some(group) = self.groups.get(&active_id) {
            group.update(cx, |g, cx| g.focus_active(window, cx));
        }
        cx.notify();
    }

    pub fn open_or_activate_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Already open somewhere? Activate that group + that tab.
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .editor_tab_index(path.as_path())
                .map(|idx| (*id, idx))
        });
        if let Some((id, idx)) = existing {
            self.set_active_group(id, window, cx);
            let group = self.groups.get(&id)?.clone();
            return Some(group.update(cx, |g, cx| {
                g.set_active(idx, window, cx);
                idx
            }));
        }

        // New file: always land in the focused (active) group, mixing
        // freely with whatever it already contains (terminals/agents).
        // Falls back to the topmost group if no active group exists.
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| g.open_or_activate_editor_tab(path, window, cx)))
    }

    /// Single-click preview open: activate an already-open tab in any group,
    /// otherwise open/reuse a reusable preview tab in the active group.
    pub fn open_preview_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .editor_tab_index(path.as_path())
                .map(|idx| (*id, idx))
        });
        if let Some((id, idx)) = existing {
            self.set_active_group(id, window, cx);
            let group = self.groups.get(&id)?.clone();
            return Some(group.update(cx, |g, cx| {
                g.set_active(idx, window, cx);
                idx
            }));
        }
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied())?;
        self.set_active_group(target_id, window, cx);
        let target = self.groups.get(&target_id)?.clone();
        Some(target.update(cx, |g, cx| g.open_preview_editor_tab(path, window, cx)))
    }

    /// Focus the agent tab already running in `worktree_path`, if one
    /// exists in any group. Returns `true` when a matching tab was found
    /// and activated; `false` when no tab matches (caller decides whether
    /// to spawn a fresh session). Mirrors the cross-group activate path of
    /// `open_or_activate_editor_tab` but never opens a new tab itself.
    pub fn focus_workspace_tab(
        &mut self,
        worktree_path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .agent_tab_index_for_worktree(worktree_path)
                .map(|idx| (*id, idx))
        });
        let Some((id, idx)) = existing else {
            return false;
        };
        self.set_active_group(id, window, cx);
        if let Some(group) = self.groups.get(&id).cloned() {
            group.update(cx, |g, cx| g.set_active(idx, window, cx));
        }
        true
    }

    /// Whether any group in these panes holds the agent tab for `session_id`.
    /// Read-only — used to locate the owning project before switching to it.
    pub fn has_agent_session(
        &self,
        session_id: oximux_core::AgentSessionId,
        cx: &gpui::App,
    ) -> bool {
        self.groups
            .values()
            .any(|g| g.read(cx).agent_tab_index_for_session(session_id).is_some())
    }

    /// Activate the tab driving `session_id` (its group becomes active, then
    /// the tab within it). Mirror of `focus_workspace_tab` but keyed by the
    /// runtime session rather than the worktree, so a clicked rail sub-row
    /// focuses that exact agent. Returns `false` if no group holds it.
    pub fn focus_agent_session(
        &mut self,
        session_id: oximux_core::AgentSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .agent_tab_index_for_session(session_id)
                .map(|idx| (*id, idx))
        });
        let Some((id, idx)) = existing else {
            return false;
        };
        self.set_active_group(id, window, cx);
        if let Some(group) = self.groups.get(&id).cloned() {
            group.update(cx, |g, cx| g.set_active(idx, window, cx));
        }
        true
    }

    /// Whether any group hosts the terminal PTY `pty_id` (the per-pane identity
    /// an ambient agent rail row is keyed by).
    pub fn has_terminal_pty(&self, pty_id: &str, cx: &gpui::App) -> bool {
        self.groups
            .values()
            .any(|group| group.read(cx).terminal_tab_index_for_pty(pty_id, cx).is_some())
    }

    /// The cwd of the pane group hosting the terminal PTY `pty_id`, if any
    /// group in this project does. The ambient rail-row click router uses it to
    /// resolve which rail workspace owns the clicked terminal (matched by
    /// worktree path) so the active-row highlight follows the click. Read-only
    /// counterpart of `group_cwd_for_terminal_session`, keyed by PTY id.
    pub fn group_cwd_for_pty(&self, pty_id: &str, cx: &gpui::App) -> Option<std::path::PathBuf> {
        self.groups.values().find_map(|g| {
            let group = g.read(cx);
            group
                .terminal_tab_index_for_pty(pty_id, cx)
                .map(|_| group.cwd().clone())
        })
    }

    /// Activate the terminal tab hosting the PTY `pty_id`. This is the
    /// ambient-terminal counterpart of `focus_agent_session`, focusing the
    /// exact pane the user clicked in the rail (per-pane identity).
    pub fn focus_ambient_agent_terminal(
        &mut self,
        pty_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let existing: Option<(PaneGroupId, usize)> = self.groups.iter().find_map(|(id, group)| {
            group
                .read(cx)
                .terminal_tab_index_for_pty(pty_id, cx)
                .map(|idx| (*id, idx))
        });
        let Some((id, idx)) = existing else {
            return false;
        };
        self.set_active_group(id, window, cx);
        if let Some(group) = self.groups.get(&id).cloned() {
            group.update(cx, |g, cx| g.set_active(idx, window, cx));
        }
        true
    }

    /// Collect the worktree paths kept "live" by open PTY tabs across all
    /// groups (as strings, to match the sidebar's `Workspace.worktree_path`).
    /// Drives the left rail's live/idle status dot: a workspace with an
    /// open terminal or agent reads as "live" (green).
    pub fn live_worktree_paths(&self, cx: &gpui::App) -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        for group in self.groups.values() {
            for path in group.read(cx).live_worktree_paths() {
                set.insert(path.display().to_string());
            }
        }
        set
    }
}
