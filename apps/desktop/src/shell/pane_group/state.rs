use super::*;

impl PaneGroup {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        theme: Theme,
        density: Density,
        typography: Typography,
        cli_runtime: Arc<CliRuntime>,
        notifier: Arc<dyn Notifier>,
        window_active: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            tabs: Vec::new(),
            tab_order: Vec::new(),
            drag_hover: None,
            active: 0,
            pending_clean_exit_closes: Vec::new(),
            last_visible_ids: std::collections::HashSet::new(),
            focus_handle: cx.focus_handle(),
            next_terminal_n: 1,
            theme,
            density,
            typography,
            cwd,
            cli_runtime,
            notifier,
            window_active,
            chrome_w_px: density.w_left_rail,
            tab_strip_scroll: ScrollHandle::new(),
            mru: Vec::new(),
            mru_switcher: None,
            _mru_focus_out_sub: None,
            active_sub_divider: None,
            sub_divider_bounds: DividerBoundsCache::default(),
            last_sub_divider_path: None,
            dirty_close_dialog: None,
            _dirty_close_observer: None,
            _external_mutation_task: None,
            compose_bar: None,
            _compose_sub: None,
            compose_session: None,
        }
    }

    /// Lazily start the external-mutation sweep (first render is the first
    /// place we have a `&mut Context`). Idempotent.
    pub(crate) fn ensure_external_mutation_sweep(&mut self, cx: &mut Context<Self>) {
        if self._external_mutation_task.is_some() {
            return;
        }
        self._external_mutation_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                if this
                    .update(cx, |group, cx| group.sweep_external_mutations(cx))
                    .is_err()
                {
                    break; // group dropped — stop sweeping.
                }
            }
        }));
    }

    /// Stat each editor tab's file; flag any that vanished as `Deleted` and
    /// clear the flag if a file reappeared (e.g. restored on disk). Rename is
    /// folded into `Deleted` here — the path is gone either way; distinguishing
    /// a true rename needs the watcher's paired event (a follow-up).
    pub(super) fn sweep_external_mutations(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for tab in &mut self.tabs {
            let PaneGroupTabKind::Editor { path } = &tab.kind else {
                continue;
            };
            let new_state = if path.exists() {
                None
            } else {
                Some(ExternalMutation::Deleted)
            };
            if tab.external_mutation != new_state {
                tab.external_mutation = new_state;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Install the focus-out subscription that auto-dismisses the MRU
    /// HUD. Called once from the renderer on first paint (when we first
    /// have a `&mut Window`). Idempotent — subsequent calls no-op.
    pub fn ensure_mru_focus_out_sub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._mru_focus_out_sub.is_some() {
            return;
        }
        let handle = self.focus_handle.clone();
        let sub = cx.on_focus_out(&handle, window, |this, _ev, _window, cx| {
            this.cancel_mru_switch(cx);
        });
        self._mru_focus_out_sub = Some(sub);
    }

    pub fn tabs(&self) -> &[PaneGroupTab] {
        &self.tabs
    }

    /// Iterate tabs in their visible order (post-drag-reorder). Each
    /// item carries the insertion-order index alongside the tab so
    /// callers can keep using the canonical idx for click handlers and
    /// active-tracking.
    pub fn visible_tabs(&self) -> impl Iterator<Item = (usize, &PaneGroupTab)> + '_ {
        self.tab_order
            .iter()
            .filter_map(move |&idx| self.tabs.get(idx).map(|t| (idx, t)))
    }

    /// Walk `tab_order` directly (yields insertion indices in visual
    /// order). Used by the snapshot path that needs the raw indices,
    /// not the (idx, tab) pairs `visible_tabs` returns.
    pub fn tab_order_iter(&self) -> impl Iterator<Item = &usize> + '_ {
        self.tab_order.iter()
    }

    /// Replace `tab_order` with the supplied vector. The restorer uses
    /// this after pushing every restored tab to re-apply the saved
    /// visual sequence in one shot — sequencing N `move_tab` calls would
    /// be O(N²) and confuse the active-tracking guards in `move_tab`.
    ///
    /// Defensive: out-of-range indices are dropped, duplicates are
    /// dropped (keeping the first occurrence), and any insertion-index
    /// missing from `order` is appended at the end. These three guards
    /// keep a stale or corrupted snapshot from rendering the same tab
    /// twice / skipping a real tab — the invariant
    /// `tab_order.len() == tabs.len()` is preserved.
    pub fn set_tab_order(&mut self, order: Vec<usize>) {
        let next = canonicalize_tab_order(order, self.tabs.len());
        debug_assert_eq!(next.len(), self.tabs.len());
        self.tab_order = next;
    }

    /// Stamp a freshly-restored tab (at insertion index `idx`) with its
    /// saved cosmetic state and visual rank, then re-sort `tab_order` so
    /// every ranked tab sits in its saved slot. Idempotent and order-
    /// independent: called once per restored tab as it mounts (synchronously
    /// for editor/terminal/browser, asynchronously for agents), it always
    /// converges to the saved strip order. Unranked tabs (`None`) keep their
    /// relative position at the tail via the stable sort.
    pub fn place_restored_tab(
        &mut self,
        idx: usize,
        meta: RestoredTabMeta,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.restore_rank = Some(meta.rank);
            tab.is_preview = meta.is_preview;
            tab.pinned = meta.pinned;
            tab.color = meta.color;
            tab.custom_title = meta.custom_title;
        }
        // A restored custom title must reach the remote session list too — the tab
        // was created (and synced) with its default label before this override.
        self.sync_remote_tab_title(idx, cx);
        self.tab_order.sort_by_key(|&i| {
            self.tabs
                .get(i)
                .and_then(|t| t.restore_rank)
                .unwrap_or(usize::MAX)
        });
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        cx.notify();
    }

    /// Move a tab from one visible position to another. Mutates only
    /// `tab_order`; entity refs in `tabs` and the `active` insertion
    /// index are preserved so the active highlight follows the moved
    /// tab automatically. No-op when indices are out of range or
    /// identical.
    ///
    /// Pinned tabs cluster at the front of `tab_order` — drag-reorder
    /// clamps the destination to stay inside the moved tab's bucket
    /// (pinned tabs can't slide into the unpinned zone and vice versa).
    pub fn move_tab(&mut self, from_visible_idx: usize, to_visible_idx: usize) {
        if from_visible_idx >= self.tab_order.len() {
            return;
        }
        let moved_insertion = self.tab_order[from_visible_idx];
        let pinned = self
            .tabs
            .get(moved_insertion)
            .map(|t| t.pinned)
            .unwrap_or(false);
        let split = self.pinned_count();
        let (min_idx, max_idx) = if pinned {
            (0, split.saturating_sub(1))
        } else {
            (split, self.tab_order.len().saturating_sub(1))
        };
        let clamped_to = to_visible_idx.clamp(min_idx, max_idx);
        if from_visible_idx == clamped_to {
            return;
        }
        let moved = self.tab_order.remove(from_visible_idx);
        self.tab_order.insert(clamped_to, moved);
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
    }

    /// `true` if the tab at insertion index `idx` is pinned.
    pub fn is_pinned(&self, idx: usize) -> bool {
        self.tabs.get(idx).map(|t| t.pinned).unwrap_or(false)
    }

    /// Number of pinned tabs — also the visible index where the
    /// unpinned cluster starts. Walks `tab_order` so the answer
    /// reflects the actual cluster boundary (pinned tabs always sort
    /// to the front via `toggle_pin`'s re-cluster step).
    pub fn pinned_count(&self) -> usize {
        self.tab_order
            .iter()
            .take_while(|&&i| self.tabs.get(i).map(|t| t.pinned).unwrap_or(false))
            .count()
    }

    /// Toggle the pinned flag on tab `idx`. After flipping the flag we
    /// re-cluster the chip inside `tab_order` so pinned tabs stay
    /// packed at the front and unpinned tabs at the back. No-op when
    /// `idx` is out of range. Notifies on every successful flip.
    pub fn toggle_pin(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        tab.pinned = !tab.pinned;
        let now_pinned = tab.pinned;
        // Pinning a preview tab promotes it to permanent (a pinned tab the user
        // wants to keep is no longer a throwaway browse tab).
        if now_pinned {
            tab.is_preview = false;
        }
        let Some(visible_from) = self.tab_order.iter().position(|&i| i == idx) else {
            cx.notify();
            return;
        };
        // Pop the chip out, then re-insert at the cluster boundary —
        // last slot of the pinned cluster (newly pinned) or first slot
        // of the unpinned cluster (newly unpinned). pinned_count() is
        // re-read AFTER the pop so the destination index lines up with
        // the shifted vector.
        let moved = self.tab_order.remove(visible_from);
        // Same dest for both directions: pin → end of pinned cluster
        // (= split, since other pinned tabs sit at [0..split)); unpin
        // → start of unpinned cluster (= split, right after the still-
        // pinned tabs).
        let _ = now_pinned;
        let dest = self.pinned_count();
        self.tab_order.insert(dest, moved);
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        cx.notify();
    }

    pub fn drag_hover(&self) -> Option<TabDragHover> {
        self.drag_hover
    }

    /// Update the drag hover indicator. Triggers a re-render only when
    /// the value actually changes — `on_drag_move` fires on every
    /// pointer move, so naive `cx.notify()` would thrash.
    pub fn set_drag_hover(&mut self, hover: Option<TabDragHover>, cx: &mut Context<Self>) {
        if self.drag_hover == hover {
            return;
        }
        self.drag_hover = hover;
        cx.notify();
    }

    /// Visible position of the tab with `insertion_idx`, if any. Used
    /// by drop handlers to translate the drag payload's insertion-idx
    /// into the visible-idx that `move_tab` expects.
    pub fn visible_position_of(&self, insertion_idx: usize) -> Option<usize> {
        self.tab_order.iter().position(|&i| i == insertion_idx)
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> Option<&PaneGroupTab> {
        self.tabs.get(self.active)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn agent_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.kind, PaneGroupTabKind::Agent { .. }))
            .count()
    }

    /// The active tab's Agent Chat view, if the active tab is a chat. Lets a
    /// route-up handler (`CreateWorktreeWorkspaceForActiveChat`) reach the chat
    /// that raised the request without threading a handle back through the
    /// action payload.
    pub fn active_agent_chat_view(
        &self,
    ) -> Option<Entity<crate::shell::agent_chat::AgentChatView>> {
        match &self.active_tab()?.content {
            PaneContent::AgentChat(view) => Some(view.clone()),
            _ => None,
        }
    }

    /// The most-recently-active Agent Chat view in this group, wherever it sits.
    /// Walks the same `mru` list [`Self::mru_agent_session`] uses, so "the chat
    /// I last worked in" resolves identically for chat-only routing (a picked
    /// browser element) as it does for text sends.
    ///
    /// Distinct from [`Self::mru_agent_session`]: that returns an id for *any*
    /// agent tab including PTY-backed ones, which cannot take an image.
    pub fn mru_agent_chat_view(
        &self,
    ) -> Option<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.mru.iter().find_map(|&idx| match &self.tabs.get(idx)?.content {
            PaneContent::AgentChat(view) => Some(view.clone()),
            _ => None,
        })
    }

    /// First Agent Chat view in this group's tab list, regardless of active or
    /// MRU state. The last-resort target, mirroring [`Self::first_agent_session`]
    /// — every tab-creation site bumps MRU today, so this covers a chat that
    /// somehow reached `tabs` without doing so rather than a known path.
    pub fn first_agent_chat_view(
        &self,
    ) -> Option<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.tabs.iter().find_map(|t| match &t.content {
            PaneContent::AgentChat(view) => Some(view.clone()),
            _ => None,
        })
    }

    /// The chat view bound to `remote_session_id`, wherever it sits in this
    /// group — active tab or not.
    ///
    /// Unlike [`Self::active_agent_chat_view`], a remote caller has no notion of
    /// which tab happens to be focused: the phone addresses a session by id and
    /// that session is usually in a background tab.
    pub fn agent_chat_view_by_remote_id(
        &self,
        remote_session_id: &str,
        cx: &gpui::App,
    ) -> Option<Entity<crate::shell::agent_chat::AgentChatView>> {
        self.tabs.iter().find_map(|tab| match &tab.content {
            PaneContent::AgentChat(view)
                if view.read(cx).remote_session_id() == remote_session_id =>
            {
                Some(view.clone())
            }
            _ => None,
        })
    }

    /// The active tab's agent session id, if the active tab is an agent.
    /// Used by "send to active agent" actions to route directly to the
    /// agent in the focused tab — the most common layout has terminal +
    /// agent side by side, so the active tab IS the routing target.
    pub fn active_agent_session(&self) -> Option<AgentSessionId> {
        match &self.active_tab()?.kind {
            PaneGroupTabKind::Agent { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// The agent the user most recently had active in this group, in MRU
    /// order (most-recent first). Lets "send to agent" target the agent you
    /// were just working with even after focusing a terminal tab — instead of
    /// an arbitrary tab-order pick that ignores which agent you last looked at.
    /// `None` when no agent tab has ever been active. Naturally validated and
    /// reorder/close-safe: the MRU stores live tab indices maintained by
    /// `bump_mru`/`forget_mru`.
    pub fn mru_agent_session(&self) -> Option<AgentSessionId> {
        self.mru.iter().find_map(|&idx| match &self.tabs.get(idx)?.kind {
            PaneGroupTabKind::Agent { session_id, .. } => Some(*session_id),
            _ => None,
        })
    }

    /// First agent session id found anywhere in this group's tab list,
    /// regardless of active state. Fallback target when no active agent
    /// tab is available (e.g. terminal in active tab, agent in a
    /// background tab of the same group).
    pub fn first_agent_session(&self) -> Option<AgentSessionId> {
        self.tabs.iter().find_map(|t| match &t.kind {
            PaneGroupTabKind::Agent { session_id, .. } => Some(*session_id),
            _ => None,
        })
    }

    /// The agent the user is currently looking at in this group's active tab:
    /// a tracked agent session, or the cwd of a focused plain-terminal running
    /// a hand-launched (ambient) agent. `None` when the active tab is not an
    /// agent surface (plain shell, editor, diff, …) — unlike
    /// `target_agent_session`, this never falls back to a background tab, so
    /// the rail lights exactly the focused row or nothing.
    pub fn focused_rail_agent(&self, cx: &gpui::App) -> Option<FocusedRailAgent> {
        let tab = self.active_tab()?;
        match &tab.kind {
            PaneGroupTabKind::Agent { session_id, .. } => {
                Some(FocusedRailAgent::Session(*session_id))
            }
            PaneGroupTabKind::Terminal => {
                let PaneContent::Terminal(tree) = &tab.content else {
                    return None;
                };
                let view = tree.active_view()?;
                let now = std::time::Instant::now();
                let v = view.read(cx);
                // Only a terminal actually running an agent gets a rail row to
                // light; a plain shell does not. A live agent process, a hook
                // sideband, or an agent title — the same presence test the rail
                // uses to group it, so focus can never light a row that is not
                // there (or miss one that is).
                let is_agent = v.agent_process().is_some()
                    || v.ambient_agent(now).is_some()
                    || v.title().and_then(classify_agent_title).is_some();
                // Key the focused row by the terminal's PTY id, the same per-pane
                // identity the rail rows carry. No PTY id (pending view) → nothing
                // to light.
                is_agent
                    .then(|| v.external_id())
                    .flatten()
                    .map(|pty_id| FocusedRailAgent::AmbientTerminal { pty_id })
            }
            _ => None,
        }
    }

    /// Count of TTY-backed tabs (terminals + agents) in this group.
    /// Excludes editor tabs.
    pub fn tty_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    PaneGroupTabKind::Terminal | PaneGroupTabKind::Agent { .. }
                )
            })
            .count()
    }

    /// Index of an existing editor tab for `path`, if any. Used by
    /// `ProjectPanes` to activate-rather-than-reopen across groups.
    pub fn editor_tab_index(&self, path: &std::path::Path) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == path))
    }

    /// Worktree paths this group keeps "live" for the sidebar status dot.
    /// A worktree is live when it owns an open PTY-backed tab — an agent
    /// (keyed by its `worktree_path`) or a plain terminal (keyed by the
    /// group's cwd, since terminals run at the project/worktree root).
    /// Mirrors the upstream rule that any live terminal session marks the
    /// worktree active (green), not just agent sessions.
    pub fn live_worktree_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut has_terminal = false;
        for tab in &self.tabs {
            match &tab.kind {
                PaneGroupTabKind::Agent { worktree_path, .. } => {
                    paths.push(worktree_path.clone());
                }
                PaneGroupTabKind::Terminal => has_terminal = true,
                _ => {}
            }
        }
        if has_terminal {
            paths.push(self.cwd.clone());
        }
        paths
    }

    /// One [`AmbientAgentEntry`] per terminal PTY running a hand-launched agent
    /// CLI (typed `claude`/`codex`/… into a plain terminal) the spawn machinery
    /// never minted a tracked session for. Each PTY is its own entry — the
    /// reference cockpit's per-pane identity — so N agents in one worktree list
    /// as N rows rather than collapsing to one. `Agent`-kind tabs are excluded:
    /// their `StatusMachine` is authoritative and already feeds the sidebar. A
    /// view with no resolvable PTY id is skipped (it can be neither persisted
    /// nor focused without one — the same key the ambient persistence uses).
    pub fn ambient_agents(&self, cx: &gpui::App) -> Vec<AmbientAgentEntry> {
        let now = std::time::Instant::now();
        let mut out: Vec<AmbientAgentEntry> = Vec::new();
        for tab in &self.tabs {
            if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                continue;
            }
            let PaneContent::Terminal(tree) = &tab.content else {
                continue;
            };
            for (_, _, view) in tree.iter_all_views() {
                let view = view.read(cx);
                let title = view.title();
                let title_status = title.and_then(classify_agent_title);
                // A live agent process is what makes this terminal an agent —
                // it covers every CLI and holds while one sits idle. The
                // sideband and the title only ever *add* to that: they say
                // what the agent is doing, and each covers a subset of CLIs.
                let by_process = view.agent_process();
                // Naming, best evidence first: the process is the CLI, so it
                // wins over a title that merely mentions one.
                let label = by_process.or_else(|| title.and_then(agent_label_from_title));
                let Some(agent) = (match view.ambient_agent(now) {
                    Some(sb) => Some(AmbientAgent {
                        status: sb.status,
                        // A hook reports state, never which CLI sent it. On a
                        // platform with no process walk and a title that names
                        // nothing, the sender can still be pinned down: hooks
                        // are only ever installed for the one CLI.
                        label: label.or(Some("Claude Code")),
                        detail: Some(sb.detail),
                    }),
                    // A running agent that has reported nothing is idle. This
                    // is the case the sideband and the title both miss, and it
                    // is the common one: an agent waiting at its prompt emits
                    // no hook, and most CLIs write no title at all.
                    None if by_process.is_some() => Some(AmbientAgent {
                        status: title_status.unwrap_or(oximux_core::AgentStatus::Idle),
                        label,
                        detail: None,
                    }),
                    // No process reading (an unsupported platform, or a pid we
                    // could not resolve) — fall back to the title alone.
                    None => title_status.map(|status| AmbientAgent {
                        status,
                        label,
                        detail: None,
                    }),
                }) else {
                    continue;
                };
                let Some(pty_id) = view.external_id() else {
                    continue;
                };
                out.push(AmbientAgentEntry {
                    pty_id,
                    cwd: terminal_view_cwd(view, &self.cwd),
                    agent,
                });
            }
        }
        out
    }

    /// Index of the terminal tab whose split tree hosts the PTY `pty_id`, if
    /// this group owns it. Resolves a rail click on an ambient agent row to the
    /// exact terminal running it (per-pane focus).
    pub fn terminal_tab_index_for_pty(&self, pty_id: &str, cx: &gpui::App) -> Option<usize> {
        for (idx, tab) in self.tabs.iter().enumerate() {
            if !matches!(tab.kind, PaneGroupTabKind::Terminal) {
                continue;
            }
            let PaneContent::Terminal(tree) = &tab.content else {
                continue;
            };
            if tree
                .iter_all_views()
                .any(|(_, _, view)| view.read(cx).external_id().as_deref() == Some(pty_id))
            {
                return Some(idx);
            }
        }
        None
    }

    /// Count of plain-terminal *panes* running a recognizable agent (the view
    /// has a live agent process, a hook reading, or a title that classifies).
    /// Feeds the status-bar "N agents" total alongside spawned `Agent` tabs,
    /// so a hand-launched agent registers there too.
    ///
    /// Counted per pane, not per tab, so it agrees with [`ambient_agents`],
    /// which yields one rail row per PTY. A tab split into two panes running
    /// two different agents is two agents; counting tabs called it one, and
    /// the rail listed two rows beneath a status bar that said "1 agent".
    ///
    /// [`ambient_agents`]: Self::ambient_agents
    pub fn ambient_agent_count(&self, cx: &gpui::App) -> usize {
        let now = std::time::Instant::now();
        self.tabs
            .iter()
            .filter(|tab| matches!(tab.kind, PaneGroupTabKind::Terminal))
            .filter_map(|tab| match &tab.content {
                PaneContent::Terminal(tree) => Some(tree),
                _ => None,
            })
            .map(|tree| {
                tree.iter_all_views()
                    .filter(|(_, _, view)| {
                        let view = view.read(cx);
                        view.agent_process().is_some()
                            || view.ambient_agent(now).is_some()
                            || view.title().and_then(classify_agent_title).is_some()
                    })
                    .count()
            })
            .sum()
    }

    /// Worktree path of the agent tab matching `tab_id`, if this group
    /// owns it. Read-only counterpart of `set_active_by_tab_id` — the
    /// notification click router uses it to resolve which project (and
    /// rail workspace) a clicked banner belongs to.
    pub fn agent_worktree_for_tab_id(&self, tab_id: TabId) -> Option<PathBuf> {
        self.tabs.iter().find_map(|t| match &t.kind {
            PaneGroupTabKind::Agent {
                session_id,
                worktree_path,
                ..
            } if TabId::from(*session_id) == tab_id => Some(worktree_path.clone()),
            _ => None,
        })
    }

    /// The workspace cwd this group spawns terminals into. The bell-banner
    /// click router uses it to resolve the owning rail workspace.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// Index of the tab whose terminal split tree hosts `session`, if this
    /// group owns it. Walks every leaf and tab in each tree (not just live
    /// views) so a bell from a background sub-pane tab still resolves.
    pub fn tab_index_for_terminal_session(
        &self,
        session: TerminalSessionId,
        cx: &gpui::App,
    ) -> Option<usize> {
        self.tabs.iter().position(|t| match &t.content {
            PaneContent::Terminal(tree) => tree
                .iter_all_views()
                .any(|(_, _, v)| v.read(cx).session_id() == session),
            PaneContent::Editor(_)
            | PaneContent::Diff(_)
            | PaneContent::Browser(_)
            | PaneContent::Tasks(_)
            | PaneContent::Automations(_)
            | PaneContent::AgentChat(_) => false,
        })
    }

    /// Dispatch a terminal-bell banner for `session` through the
    /// notification pipeline. Called by the ringing `TerminalView` (which
    /// owns the per-pane debounce); this end contributes the request
    /// context the view can't see: the tab label, the workspace key for
    /// burst collapse, and the live window-active flag. The dispatcher
    /// applies the master/source gates, visible-pane suppression, and the
    /// focus gate.
    pub fn notify_terminal_bell(
        &self,
        session: TerminalSessionId,
        pane_visible: bool,
        cx: &gpui::App,
    ) {
        let Some(idx) = self.tab_index_for_terminal_session(session, cx) else {
            return;
        };
        let tab = &self.tabs[idx];
        let label = tab
            .custom_title
            .clone()
            .unwrap_or_else(|| tab.label.clone());
        self.notifier.notify(crate::notifier::NotificationRequest {
            source: crate::notifier::NotificationSource::TerminalBell,
            kind: crate::notifier::NotificationKind::Bell,
            // Bell banners carry the raw terminal session id in the tab_id
            // slot; the `bell:` identifier namespace keeps it from ever
            // being read as an agent tab id.
            tab_id: TabId(session.0),
            workspace_key: self.cwd.to_string_lossy().into_owned(),
            label: label.to_string(),
            body: String::new(),
            window_active: self.window_active.load(Ordering::Relaxed),
            pane_visible,
            // A bell is a one-shot event, not a sticky state — no until-focus coalescing.
            coalesce_until_focus: false,
        });
    }

    /// Index of an existing agent tab whose worktree matches `path`, if
    /// any. Lets the sidebar focus the tab already running in a clicked
    /// workspace's worktree instead of spawning a duplicate.
    pub fn agent_tab_index_for_worktree(&self, path: &std::path::Path) -> Option<usize> {
        self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Agent { worktree_path, .. } if worktree_path == path)
        })
    }

    /// Index of the agent tab driving `session_id`, if this group holds it.
    /// Lets the rail focus the exact agent a sub-row was clicked for, rather
    /// than just the worktree's first agent tab.
    pub fn agent_tab_index_for_session(&self, session_id: AgentSessionId) -> Option<usize> {
        self.tabs.iter().position(
            |t| matches!(&t.kind, PaneGroupTabKind::Agent { session_id: sid, .. } if *sid == session_id),
        )
    }

    /// Retained for the sidebar-width tracking plumbing (`set_chrome_width`
    /// still fires a repaint when a side panel toggles). Grid sizing no
    /// longer reads it — that moved to each TerminalView's canvas-bounds
    /// resize — so the getter is currently unused but kept for symmetry
    /// with the setter and any future chrome-aware layout.
    #[allow(dead_code)]
    pub(crate) fn chrome_w_px(&self) -> f32 {
        self.chrome_w_px
    }

    /// ScrollHandle the render layer should attach to the tab-strip
    /// viewport via `.track_scroll(...)`. Exposed so the strip builder
    /// (free function in `render.rs`) can wire it up.
    pub(crate) fn tab_strip_scroll_handle(&self) -> ScrollHandle {
        self.tab_strip_scroll.clone()
    }

    /// Snap the tab-strip viewport to its right edge. Called after every
    /// tab append so the newly-added (and now-active) tab is visible —
    /// matches the reference editor's `stickToEndRef` behavior. The raw
    /// offset value is intentionally far-negative; the strip's paint
    /// phase clamps it to the actual `max_offset` once the new tab is
    /// measured. Idempotent if the strip already fits without overflow.
    pub(super) fn pin_tab_strip_to_end(&self) {
        self.tab_strip_scroll
            .set_offset(Point::new(px(-100_000.0), px(0.0)));
    }

    /// Returns true when the tab strip viewport is currently snapped to
    /// its rightmost extent (i.e. the user is "pinned to end"). Used by
    /// the label-change re-pin path so widening a chip after a tab is
    /// already on screen doesn't kick the active tab + `+` button off
    /// the right edge.
    ///
    /// `max_offset.x` is the absolute maximum negative-x the viewport
    /// can scroll to (positive value). When `|offset.x| >= max_x - 1`,
    /// the strip is at its right-edge limit. Zero `max_x` means no
    /// overflow → vacuously pinned (anything appended afterwards must
    /// stay visible since there's no slack to scroll).
    pub(crate) fn was_pinned_to_end(&self) -> bool {
        let offset = self.tab_strip_scroll.offset();
        let max_x = f32::from(self.tab_strip_scroll.max_offset().x);
        if max_x.abs() < 1.0 {
            return true;
        }
        f32::from(offset.x).abs() >= max_x - 1.0
    }
}
