use super::*;

impl PaneGroup {

    /// Read-only MRU order — front is most-recently-used. The switcher
    /// overlay walks this to render its candidate list.
    pub fn mru_order(&self) -> &[usize] {
        &self.mru
    }

    pub fn set_active_by_tab_id(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.tabs.iter().position(|t| match &t.kind {
            PaneGroupTabKind::Agent { session_id, .. } => TabId::from(*session_id) == tab_id,
            PaneGroupTabKind::Terminal
            | PaneGroupTabKind::Editor { .. }
            | PaneGroupTabKind::Diff { .. }
            | PaneGroupTabKind::Commit { .. }
            | PaneGroupTabKind::BranchFile { .. }
            | PaneGroupTabKind::CombinedDiff { .. }
            | PaneGroupTabKind::Browser { .. }
            | PaneGroupTabKind::Tasks => false,
        }) else {
            return false;
        };
        self.set_active(idx, window, cx);
        true
    }

    /// Cycle to the tab AFTER the active tab in VISUAL order. After the
    /// user drag-reorders chips, `tab_order` no longer matches the
    /// insertion-order `tabs` vector — walking `tabs.len()` directly
    /// would jump in the original insertion sequence, which feels broken.
    /// We locate the active tab's position inside `tab_order`, advance
    /// one slot (wrap), and map back to its insertion index.
    pub fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.adjacent_visible_tab(1) {
            self.set_active(next, window, cx);
        }
    }

    /// Symmetric counterpart of `next_tab` walking backwards.
    pub fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(prev) = self.adjacent_visible_tab(-1) {
            self.set_active(prev, window, cx);
        }
    }

    /// Resolve the insertion-order index of the tab `step` slots away
    /// from the active tab in visual order. Returns `None` when fewer
    /// than 2 visible tabs exist or the active tab isn't tracked.
    fn adjacent_visible_tab(&self, step: isize) -> Option<usize> {
        resolve_adjacent_visible_tab(&self.tab_order, self.active, step)
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.focus_handle.focus(window, cx);
            return;
        };
        let handle = tab.content.focus_handle(cx);
        handle.focus(window, cx);
    }

    pub fn active_editor_path(&self, cx: &gpui::App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.active)?;
        match &tab.content {
            PaneContent::Editor(view) => Some(view.read(cx).file_path().to_path_buf()),
            // Diff/Browser/Tasks tabs aren't editors and don't surface a file
            // path the host's editor-tracking flows treat as an editable target.
            PaneContent::Terminal(_)
            | PaneContent::Diff(_)
            | PaneContent::Browser(_)
            | PaneContent::Tasks(_) => None,
        }
    }

    /// Walk every tab (active + inactive) and yield the captured PTY
    /// scrollback bytes for the terminal kinds. Editor tabs contribute
    /// an empty buffer so the ordinal counter stays aligned with the
    /// tab vector.
    pub fn collect_pane_buffers(&self, max_bytes: usize, cx: &gpui::App) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(tree) => {
                    // Persist the ACTIVE sub-pane's scrollback only.
                    // Multi-sub-pane persistence is a follow-up; v1
                    // restore restores a single sub-pane per tab.
                    let bytes = tree
                        .active_view()
                        .map(|v| v.read(cx).serialize_buffer(max_bytes))
                        .unwrap_or_default();
                    out.push(bytes);
                }
                // Editor / Diff / Browser / Tasks tabs have no PTY scrollback,
                // but we still push an empty slot so the index alignment with
                // `tabs` is preserved for the duration of this collection call.
                PaneContent::Editor(_)
                | PaneContent::Diff(_)
                | PaneContent::Browser(_)
                | PaneContent::Tasks(_) => {
                    out.push(Vec::new())
                }
            }
        }
        out
    }

    pub fn collect_pane_external_ids(&self, cx: &gpui::App) -> Vec<Option<String>> {
        let mut out = Vec::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            match &tab.content {
                PaneContent::Terminal(tree) => {
                    out.push(tree.active_view().and_then(|v| v.read(cx).external_id()));
                }
                PaneContent::Editor(_)
                | PaneContent::Diff(_)
                | PaneContent::Browser(_)
                | PaneContent::Tasks(_) => {
                    out.push(None)
                }
            }
        }
        out
    }

    pub(crate) fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_terminal_tab(window, cx);
    }

    pub(crate) fn on_new_browser_tab(
        &mut self,
        _: &NewBrowserTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_browser_tab(crate::shell::browser_view::DEFAULT_URL, None, window, cx);
    }

    /// Open the scrollback search overlay on the active tab's active terminal
    /// sub-pane. Used by the root-level Search fallback so the command palette
    /// "Search Pane" entry works while the palette (not a terminal) has focus.
    /// No-op when the active tab isn't terminal-backed.
    pub(crate) fn open_search_active_terminal(
        &mut self,
        action: &Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_tab) = self.tabs.get(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &active_tab.content else {
            return;
        };
        let Some(view) = tree.active_view().cloned() else {
            return;
        };
        view.update(cx, |v, cx| v.on_search(action, window, cx));
        // Re-home keyboard focus onto this terminal. The search overlay has no
        // input of its own — its keystroke handler runs inside the terminal's
        // `on_key_down`, which only fires while the terminal is focused. On the
        // palette path the palette queues a refocus of the workspace root as it
        // closes, so an inline focus here would be clobbered; defer it so it
        // lands AFTER that refocus (same two-step focus race as
        // `refocus_active_pane`). Without this the box paints but swallows no
        // keys until the user clicks the terminal.
        window.defer(cx, move |window, cx| {
            view.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    /// Cmd+Shift+T — add a terminal tab to the FOCUSED split pane's own
    /// tab strip (per-pane tabs). When the active workspace tab isn't a
    /// terminal, fall back to a workspace-level new terminal tab.
    pub(crate) fn on_new_tab_in_pane(
        &mut self,
        _: &crate::actions::NewTabInPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Resolve the focused leaf (re-query focus like the split path
        // does — `tree.active` doesn't auto-track mouse focus). Immutable
        // borrow scoped so the mutable `add_tab_to_leaf` can follow.
        let target = match self.tabs.get(self.active).map(|t| &t.content) {
            Some(PaneContent::Terminal(tree)) => Some(
                tree.iter_live()
                    .find(|(_, v)| v.read(cx).focused())
                    .map(|(i, _)| i)
                    .unwrap_or_else(|| tree.active()),
            ),
            _ => None,
        };
        match target {
            Some(leaf) => self.add_tab_to_leaf(leaf, window, cx),
            None => {
                self.open_terminal_tab(window, cx);
            }
        }
    }

    /// Spawn a fresh shell and append it as a tab inside `leaf_idx` of the
    /// active terminal tab. Inherits the leaf's current cwd. Used by the
    /// per-pane `+` button and `on_new_tab_in_pane`.
    pub(crate) fn add_tab_to_leaf(
        &mut self,
        leaf_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback_cwd = self.cwd.clone();
        let workspace_id = self.cwd.to_string_lossy().into_owned();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Target the requested leaf so the tab lands there.
        tree.set_active(leaf_idx);
        // Inherit the leaf's active shell cwd (same three-tier lookup as
        // sub-pane splits).
        let inherited_cwd = tree
            .active_view()
            .and_then(|v| {
                let view = v.read(cx);
                view.cwd_hint().or_else(|| {
                    view.os_pid()
                        .and_then(crate::shell::cwd_resolver::cwd_of_pid)
                })
            })
            .unwrap_or(fallback_cwd);
        let ids = SurfaceIds::fresh(workspace_id);
        let Some((backend, session_id)) = spawn_local_pty(inherited_cwd, ids.env()) else {
            return;
        };
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        tree.add_tab_to_active(view, observer);
        if let Some(active_view) = tree.active_view() {
            active_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_close_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Cmd+W cascade for terminal tabs (most-granular first):
        //   1. focused leaf has >1 per-pane tab → close just that tab
        //   2. tab is split into >1 leaf → close the focused leaf
        //   3. otherwise → close the whole workspace (group) tab
        // The focused leaf is re-resolved here because `tree.active`
        // doesn't auto-track mouse-driven focus changes. Editor/diff tabs
        // (non-Terminal) skip straight to the group-tab close.
        let active_idx = self.active;
        if let Some(active_tab) = self.tabs.get_mut(active_idx)
            && let PaneContent::Terminal(tree) = &mut active_tab.content
        {
            let focused_idx = tree
                .iter_live()
                .find(|(_, v)| v.read(cx).focused())
                .map(|(i, _)| i);
            if let Some(idx) = focused_idx {
                tree.set_active(idx);
            }
            // 1. close a per-pane tab when the focused leaf has >1.
            if tree.close_active_tab() {
                if let Some(view) = tree.active_view() {
                    view.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
                return;
            }
            // 2. close the focused leaf when the tab is split.
            if tree.live_count() > 1 && tree.close_active() {
                if let Some(view) = tree.active_view() {
                    view.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
                return;
            }
        }
        // Editor/diff/browser group tabs (and lone-leaf terminals) close the
        // whole group tab — routed through the dirty-guard for unsaved editors.
        self.request_close_tab(active_idx, window, cx);
    }

    pub(crate) fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.next_tab(window, cx);
    }

    pub(crate) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.prev_tab(window, cx);
    }

    /// Ctrl+Tab while Ctrl is held — advance the MRU switcher cursor
    /// through a FROZEN MRU snapshot. The first press captures the
    /// snapshot and lands cursor on row 1 (the most-recent prior tab,
    /// the standard "jump back" behavior). Subsequent presses advance
    /// cursor through the list, wrapping at the end.
    ///
    /// The tab is NOT activated here — the commit happens at
    /// `commit_mru_switch` on Ctrl release. This matches the reference
    /// editor's "Switch Tab" HUD pattern: hold Ctrl, tap Tab N times to
    /// land N-th entry into focus, release to commit.
    pub(crate) fn on_mru_next(
        &mut self,
        _: &crate::actions::MruNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mru.len() < 2 {
            return;
        }
        self.advance_mru_switcher(1, cx);
    }

    /// Ctrl+Shift+Tab — advance the MRU switcher cursor BACKWARD.
    /// Same snapshot/HUD lifecycle as `on_mru_next`.
    pub(crate) fn on_mru_prev(
        &mut self,
        _: &crate::actions::MruPrev,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mru.len() < 2 {
            return;
        }
        self.advance_mru_switcher(-1, cx);
    }

    fn advance_mru_switcher(&mut self, step: isize, cx: &mut Context<Self>) {
        // Capture snapshot on first press of the cycle.
        if self.mru_switcher.is_none() {
            self.mru_switcher = Some(MruSwitcher {
                snapshot: self.mru.clone(),
                cursor: 0,
            });
        }
        let Some(state) = self.mru_switcher.as_mut() else {
            return;
        };
        match advance_mru_cursor(state.cursor, step, state.snapshot.len()) {
            Some(next) => {
                state.cursor = next;
                cx.notify();
            }
            None => {
                // Snapshot too short to cycle — drop the switcher so the
                // HUD doesn't get stuck open over a now-empty MRU.
                self.mru_switcher = None;
                cx.notify();
            }
        }
    }

    /// Active MRU switcher (if any). View layer reads this to decide
    /// whether to paint the floating HUD.
    pub fn mru_switcher(&self) -> Option<&MruSwitcher> {
        self.mru_switcher.as_ref()
    }

    /// Called when the modifier key (Ctrl) that opened the MRU switcher
    /// is released. Activates the highlighted tab and clears the HUD.
    /// No-op when no switcher is active.
    pub fn commit_mru_switch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.mru_switcher.take() else {
            return;
        };
        let Some(&target) = state.snapshot.get(state.cursor) else {
            cx.notify();
            return;
        };
        if target == self.active {
            cx.notify();
            return;
        }
        // Defense against tab close during a cycle: the snapshot freezes
        // indices captured BEFORE any close, so `target` may now point
        // to a different (or no) tab after `forget_mru` shifted things.
        // The `mru` vec stays consistent across closes — if `target` is
        // no longer in `mru`, the underlying tab is gone or shifted.
        if !self.mru.contains(&target) {
            cx.notify();
            return;
        }
        // `set_active` does its own `bump_mru` so the activated tab
        // floats to mru[0] for the NEXT switching session.
        if self.tabs.get(target).is_some() {
            self.set_active(target, window, cx);
        } else {
            cx.notify();
        }
    }

    /// Forcibly clear an in-flight MRU switcher. Called when focus leaves
    /// the group (the `on_modifiers_changed` listener won't fire on a
    /// different focus path, so the HUD would otherwise stay open).
    pub fn cancel_mru_switch(&mut self, cx: &mut Context<Self>) {
        if self.mru_switcher.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn on_new_agent(
        &mut self,
        _: &NewAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keyboard path — no cursor x available. `x: None` tells
        // `WorkspaceRoot` to use the post-rail fallback anchor.
        window.dispatch_action(Box::new(RequestOpenAdapterPicker { x: None }), cx);
    }

    /// Toggle the prompt composer over the active agent tab. A second press
    /// closes it; pressing over a non-agent tab is a no-op.
    pub(crate) fn on_open_composer_bar(
        &mut self,
        _: &crate::actions::OpenComposerBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Toggle: a second ⌘I closes the composer.
        if self.compose_bar.is_some() {
            self.close_composer(cx);
            return;
        }
        let (root, session_id) = match self.active_tab().map(|t| &t.kind) {
            Some(PaneGroupTabKind::Agent {
                worktree_path,
                session_id,
                ..
            }) => (worktree_path.clone(), *session_id),
            _ => return,
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let composer = cx.new(|cx| {
            crate::shell::compose_bar::ComposerBar::new(
                root, theme, density, typography, window, cx,
            )
        });
        let sub = cx.subscribe_in(
            &composer,
            window,
            |this, _composer, outcome: &crate::shell::compose_bar::ComposerOutcome, window, cx| {
                this.handle_composer_outcome(outcome, window, cx);
            },
        );
        // Focus the draft so the user types immediately.
        let handle = composer.read(cx).input_focus_handle(cx);
        window.focus(&handle, cx);
        self.compose_bar = Some(composer);
        self._compose_sub = Some(sub);
        self.compose_session = Some(session_id);
        cx.notify();
    }

    /// Send a submitted draft to the active agent and tear down the composer.
    fn handle_composer_outcome(
        &mut self,
        outcome: &crate::shell::compose_bar::ComposerOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let crate::shell::compose_bar::ComposerOutcome::Submit(draft) = outcome {
            // Wrap in bracketed paste only when the agent shell has DECSET-2004
            // on, so a multi-line prompt lands as a single insertion rather
            // than executing line-by-line. `format_send_bytes` appends a
            // trailing CR (outside the brackets) so this one ⌘↵ submits.
            let bp_on = self.active_agent_bracketed_paste(cx);
            let bytes = crate::shell::compose_bar::send_formatter::format_send_bytes(draft, bp_on);
            // `format_send_bytes` only ever emits valid UTF-8 (the payload is an
            // already-valid String, ESC and the bracket markers are ASCII).
            let text = String::from_utf8(bytes).expect("composer bytes are valid UTF-8");
            window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
            self.apply_first_prompt_title(draft);
        }
        self.close_composer(cx);
    }

    fn close_composer(&mut self, cx: &mut Context<Self>) {
        self.compose_bar = None;
        self._compose_sub = None;
        self.compose_session = None;
        cx.notify();
    }

    /// The open composer, but only while its origin agent is the active tab —
    /// the render gate that keeps it from showing over (or submitting to) a
    /// different tab. Returns the entity to render, or `None` to hide it.
    pub(super) fn composer_for_active_tab(&self) -> Option<&Entity<crate::shell::compose_bar::ComposerBar>> {
        let composer = self.compose_bar.as_ref()?;
        let session = self.compose_session?;
        match self.active_tab().map(|t| &t.kind) {
            Some(PaneGroupTabKind::Agent { session_id, .. }) if *session_id == session => {
                Some(composer)
            }
            _ => None,
        }
    }

    /// Whether the active agent tab's terminal currently has bracketed paste on.
    fn active_agent_bracketed_paste(&self, cx: &App) -> bool {
        let Some(PaneContent::Terminal(tree)) = self.active_tab().map(|t| &t.content) else {
            return false;
        };
        tree.active_view()
            .map(|view| view.read(cx).bracketed_paste_active())
            .unwrap_or(false)
    }

    /// Name an agent tab after its first composed prompt — but never clobber a
    /// title the user set themselves (`custom_title.is_none()` is exactly the
    /// "first submit" gate, since this very call fills it in).
    fn apply_first_prompt_title(&mut self, draft: &str) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if !matches!(tab.kind, PaneGroupTabKind::Agent { .. }) || tab.custom_title.is_some() {
            return;
        }
        let title: String = draft.trim().chars().take(50).collect();
        if !title.is_empty() {
            tab.custom_title = Some(SharedString::from(title));
        }
    }

    /// Split the active terminal tab's CURRENTLY-FOCUSED sub-pane along
    /// `axis`. Before splitting we re-resolve which sub-pane actually
    /// has keyboard focus (by walking the tree and asking each
    /// `TerminalView`) — necessary because the stored `tree.active`
    /// only updates on previous splits/cycles, not when the user clicks
    /// into a different sub-pane to give it focus. Without this
    /// re-resolution Cmd+D would split whichever pane was last cycled
    /// to, not the one the user is actually typing in.
    ///
    /// Spawns a fresh PTY at the CWD of the active sub-pane's shell
    /// (via libproc on macOS, falling back to the group's cwd when the
    /// pid is unknown or the lookup fails). No-op when the active tab
    /// is not a terminal or PTY spawn fails.
    fn split_active_sub_pane(
        &mut self,
        axis: Axis,
        insert: SplitInsert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fallback_cwd = self.cwd.clone();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        // Sub-panes only apply to terminal-backed tabs; editor tabs are
        // single-view.
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Re-resolve focused sub-pane: scan live entries and pick the one
        // whose view reports `focused()`. Fallback to stored active
        // (covers the keyboard-only case where focus query may race
        // against terminal mount; the cycle/split paths keep active
        // correct in that flow). Two-step borrow: collect the index
        // through an immutable borrow, then mutate.
        let focused_idx = tree
            .iter_live()
            .find(|(_, v)| v.read(cx).focused())
            .map(|(i, _)| i);
        if let Some(idx) = focused_idx {
            tree.set_active(idx);
        }
        // Exit zoom mode before splitting — a zoomed sub-pane fills the
        // body while siblings are collapsed to width 0. Splitting under
        // that state hands the new sub-pane a zero-width slot, so the
        // user sees nothing until they zoom back out. Toggling zoom off
        // first restores the tree layout; the just-split pane gets a
        // real slice of the body. Matches the reference editor.
        if tree.zoomed().is_some() {
            tree.toggle_zoom_active();
        }
        // Inherit the source shell's CWD. Three-tier lookup:
        //   1. OSC 7 cache (F4.7) — populated every prompt by the shell's
        //      OSC 7 hook. Lock-free read, no syscall. Empty only before
        //      the first prompt fires post-spawn.
        //   2. libproc / proc_pidinfo on the shell's pid — works for any
        //      local shell, including ones without an OSC 7 hook, at the
        //      cost of a syscall (~80–200 µs on macOS).
        //   3. The tab group's cwd — last-resort fallback for relay
        //      backends, dormant sub-panes, or shells that already exited.
        let inherited_cwd = tree
            .active_view()
            .and_then(|v| {
                let view = v.read(cx);
                view.cwd_hint().or_else(|| {
                    view.os_pid()
                        .and_then(crate::shell::cwd_resolver::cwd_of_pid)
                })
            })
            .unwrap_or(fallback_cwd);
        // workspace_id stays the project root even though the new sub-pane
        // spawns at the inherited (possibly cd'd) cwd.
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let Some((backend, session_id)) = spawn_local_pty(inherited_cwd, ids.env()) else {
            return;
        };
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        tree.split_active(axis, insert, view, observer);
        // Focus the just-spawned sub-pane so keyboard input lands there.
        if let Some(active_view) = tree.active_view() {
            active_view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_split_sub_pane_right(
        &mut self,
        _: &SplitSubPaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_active_sub_pane(Axis::Horizontal, SplitInsert::After, window, cx);
    }

    pub(crate) fn on_split_sub_pane_down(
        &mut self,
        _: &SplitSubPaneDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_active_sub_pane(Axis::Vertical, SplitInsert::After, window, cx);
    }

    pub(crate) fn on_focus_next_sub_pane(
        &mut self,
        _: &FocusNextSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_sub_pane_focus(true, window, cx);
    }

    pub(crate) fn on_focus_prev_sub_pane(
        &mut self,
        _: &FocusPrevSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_sub_pane_focus(false, window, cx);
    }

    fn cycle_sub_pane_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        tree.cycle_focus(forward);
        if let Some(view) = tree.active_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// Apply new weights to a Split node inside the active tab's
    /// sub-pane tree. Used by the divider resize handle. Triggers a
    /// re-render only when the call actually mutates the tree — the
    /// drag handler fires at ~60 fps and unconditional notifies would
    /// thrash.
    pub fn set_active_sub_pane_weights(
        &mut self,
        path: &[usize],
        weights: Vec<f32>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return false;
        };
        let prev = tree.split_weights(path);
        let changed = tree.set_split_weights(path, weights);
        if !changed {
            return false;
        }
        let next = tree.split_weights(path);
        if prev == next {
            return false;
        }
        cx.notify();
        true
    }

    /// Shared bounds-cache handle for the render pass to record sub-pane
    /// split-row geometry into (one `Rc` clone per render).
    pub fn sub_divider_bounds_cache(&self) -> DividerBoundsCache {
        self.sub_divider_bounds.clone()
    }

    /// The sub-pane divider currently being resized, if any.
    pub fn active_sub_divider(&self) -> Option<&ActiveDivider> {
        self.active_sub_divider.as_ref()
    }

    /// Arm a sub-pane divider for mouse-capture resize.
    pub fn arm_sub_divider(&mut self, active: ActiveDivider, cx: &mut Context<Self>) {
        self.last_sub_divider_path = Some(active.split_path.clone());
        self.active_sub_divider = Some(active);
        cx.notify();
    }

    /// Reset the sub-pane split the user just double-clicked. Resolves the
    /// target from the armed divider, or the most-recently-armed path when
    /// the first click's arm was already disarmed (so the reset still lands
    /// when the capture overlay is under the second click).
    pub fn reset_sub_divider_double_click(&mut self, cx: &mut Context<Self>) {
        let path = self
            .active_sub_divider
            .as_ref()
            .map(|a| a.split_path.clone())
            .or_else(|| self.last_sub_divider_path.clone());
        if let Some(path) = path {
            self.reset_sub_split_to_equal(&path, cx);
        }
        self.disarm_sub_divider(cx);
    }

    /// Apply a resize for the armed sub-pane divider from the cursor
    /// position. No-op when nothing is armed.
    pub fn resize_active_sub_divider(
        &mut self,
        pos: Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.active_sub_divider.clone() else {
            return false;
        };
        let Some(frac) = crate::shell::divider::fraction_along(&active, pos) else {
            return false;
        };
        let new_weights = render::redistribute_sub_pane_weights(
            &active.initial_weights,
            active.divider_idx,
            frac,
        );
        self.set_active_sub_pane_weights(&active.split_path, new_weights, cx)
    }

    /// Disarm the active sub-pane divider (mouse released).
    pub fn disarm_sub_divider(&mut self, cx: &mut Context<Self>) {
        if self.active_sub_divider.take().is_some() {
            cx.notify();
        }
    }

    /// Reset the active tab's sub-pane split at `path` to equal weights
    /// (double-click a sub-pane divider).
    pub fn reset_sub_split_to_equal(&mut self, path: &[usize], cx: &mut Context<Self>) {
        let Some(active_tab) = self.tabs.get(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &active_tab.content else {
            return;
        };
        let Some(weights) = tree.split_weights(path) else {
            return;
        };
        let equal = vec![1.0; weights.len()];
        self.set_active_sub_pane_weights(path, equal, cx);
    }

    pub(crate) fn on_toggle_zoom_sub_pane(
        &mut self,
        _: &ToggleZoomSubPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let PaneContent::Terminal(tree) = &mut active_tab.content else {
            return;
        };
        // Re-resolve focused sub-pane first (same reasoning as the
        // split / close paths — mouse clicks don't auto-update active).
        let focused_idx = tree
            .iter_live()
            .find(|(_, v)| v.read(cx).focused())
            .map(|(i, _)| i);
        if let Some(idx) = focused_idx {
            tree.set_active(idx);
        }
        tree.toggle_zoom_active();
        if let Some(view) = tree.active_view() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }
}
