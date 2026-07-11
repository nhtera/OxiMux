use super::*;

use crate::shell::agent_chat::{ChatTerminalSpec, ChatViewMode};
use crate::shell::terminal_view::{DEFAULT_COLS, DEFAULT_ROWS};
use oximux_agents::AgentSessionConfig;
use oximux_core::SessionResumption;
use oximux_settings::AgentLaunchSettings;

impl PaneGroup {

    /// Schedule a one-frame-deferred re-pin to the strip's right edge.
    /// Used after any mutation that may have widened a tab chip (custom
    /// title set, agent status badge appears) — the new `max_offset`
    /// isn't known until after the next paint, so we sleep 16 ms then
    /// re-pin. No-op when `was_pinned` is false (user had manually
    /// scrolled left → respect their position).
    pub(crate) fn schedule_repin_if_was_pinned(&self, was_pinned: bool, cx: &mut Context<Self>) {
        if !was_pinned {
            return;
        }
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let _ = weak.update(cx, |g, _cx| {
                g.pin_tab_strip_to_end();
            });
        })
        .detach();
    }

    /// Wraps `cx.notify()` with a snapshot of the strip's pin state and
    /// a deferred re-pin if the user was pinned. Called by the agent
    /// status watcher (which mutates the badge dot's visibility,
    /// changing chip width) and by any other label-mutating path that
    /// goes through `cx.notify()` without explicitly handling the pin
    /// snapshot itself.
    pub(crate) fn notify_with_label_change_check(&self, cx: &mut Context<Self>) {
        let was_pinned = self.was_pinned_to_end();
        cx.notify();
        self.schedule_repin_if_was_pinned(was_pinned, cx);
    }

    pub(crate) fn focus_handle_clone(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_chrome_width(&mut self, new_chrome: f32, cx: &mut Context<Self>) {
        if (self.chrome_w_px - new_chrome).abs() < f32::EPSILON {
            return;
        }
        self.chrome_w_px = new_chrome;
        cx.notify();
    }

    /// Override the next-terminal-label seed. `ProjectPanes` uses this to
    /// route a workspace-global counter into each group right before a
    /// spawn, keeping terminal numbering monotonic across splits.
    pub fn set_next_terminal_n(&mut self, n: u64) {
        self.next_terminal_n = n;
    }

    /// Mark every terminal view in this group hidden (background-project
    /// throttle). Output keeps draining; only the poll cadence + repaints drop.
    /// Clears the visibility cache so the next render reconciles back to the
    /// real shown-set when this group is on screen again.
    ///
    /// Also hides any browser tab's native webview — an off-screen project's
    /// PaneGroups never render again until reactivated, so the per-render
    /// visibility sweep can't fire; without this the webview (a native view
    /// above the GPU canvas) keeps floating over the incoming project's pane.
    pub fn hide_all_terminals(&mut self, cx: &mut Context<Self>) {
        for tab in self.tabs.iter() {
            if let PaneContent::Terminal(tree) = &tab.content {
                for (_, _, view) in tree.iter_all_views() {
                    view.update(cx, |v, vcx| v.set_visible(false, vcx));
                }
            }
            if let PaneContent::Browser(view) = &tab.content {
                view.update(cx, |v, _| v.set_active(false));
            }
        }
        // No cx.notify() — this group is off-screen, so a repaint would be
        // wasted. Clearing the cache guarantees the post-reactivation render's
        // `desired != last_visible_ids` check is true, so the sweep re-shows
        // the real set instead of skipping on a stale-equal cache.
        self.last_visible_ids.clear();
    }

    /// Peek at the next-terminal seed (without mutating). Used by
    /// `ProjectPanes::take_next_terminal_n` to compute the workspace-wide
    /// floor across every group's local counter.
    pub fn next_terminal_n_peek(&self) -> u64 {
        self.next_terminal_n
    }

    /// Append a freshly-spawned shell terminal as a new tab. Returns the
    /// index of the new tab; `None` if PTY spawn failed.
    pub fn open_terminal_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let (backend, session_id) = spawn_local_pty(self.cwd.clone(), ids.env())?;
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        let n = self.next_terminal_n;
        self.next_terminal_n += 1;
        let tab = PaneGroupTab {
            label: SharedString::from(format!("Terminal {n}")),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            pinned: false,
            // Tab-level observer is unused for terminal tabs — sub-pane
            // observers inside TerminalSplitTree drive re-renders.
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        Some(self.active)
    }

    /// Open a terminal tab rooted at `cwd` that runs `script` once, leaving
    /// the interactive shell live afterward (so a short setup script's tab
    /// stays usable and a long-running `run` stays attached). Used by the
    /// per-project lifecycle scripts surface.
    ///
    /// The script is fed as if typed at the prompt + Enter rather than passed
    /// as shell args: the relay spawn path honors only `shell` and drops extra
    /// args, so feeding input is the one path that runs a command on both the
    /// relay-backed and in-process backends. Input queues in the PTY master
    /// and the shell consumes it once it starts reading stdin.
    ///
    /// Multi-line scripts work as written: each embedded `\n` is delivered to
    /// the shell's line discipline as an Enter, so every line executes in
    /// order. There is no single-line restriction.
    pub fn open_script_terminal_tab(
        &mut self,
        cwd: PathBuf,
        title: SharedString,
        script: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let ids = SurfaceIds::fresh(cwd.to_string_lossy().into_owned());
        let (backend, session_id) = spawn_local_pty(cwd, ids.env())?;
        {
            let mut guard = backend.lock().expect("shared backend poisoned");
            // Trailing `\n` runs the (possibly multi-line) script. Each
            // newline is an Enter to the shell's line discipline.
            let line = format!("{}\n", script.trim());
            if let Err(err) = guard.write(session_id, line.as_bytes()) {
                // Keep the tab: a live interactive shell at the worktree cwd
                // is still useful (the user can re-run the script by hand).
                // Only the in-process fallback can fail here; the relay path
                // enqueues without blocking.
                tracing::warn!(?err, "failed to feed lifecycle script into pty");
            }
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, session_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        let n = self.next_terminal_n;
        self.next_terminal_n += 1;
        let tab = PaneGroupTab {
            label: SharedString::from(format!("Terminal {n}")),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: Some(title),
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        Some(self.active)
    }

    pub fn open_or_activate_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        self.open_or_activate_editor_tab_at(path, None, false, window, cx)
    }

    /// Build an `EditorView` for `path` with its language server attached and
    /// a promote-on-edit observer wired. Shared by the new-tab and the
    /// preview-replace paths. The observer clears `is_preview` on the owning
    /// tab the first time the buffer goes dirty, so editing a preview tab
    /// makes it permanent.
    fn build_editor_view(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<oximux_editor::EditorView>, Subscription) {
        let view = cx.new(|cx| oximux_editor::EditorView::new(path.to_path_buf(), window, cx));
        // Resolve + attach a language server (extension-keyed, PATH-resolved;
        // unsupported language or uninstalled server is a clean no-op).
        if let Some(server) = oximux_editor::resolve_lsp_server(path) {
            let workspace_root = self.cwd.clone();
            view.update(cx, |v, cx| {
                v.attach_lsp(
                    &server.program,
                    server.args,
                    server.language_id,
                    workspace_root,
                    cx,
                );
            });
        }
        let observer = cx.observe(&view, |this, view_entity, cx| {
            // Promote the preview tab to permanent on first edit.
            if view_entity.read(cx).is_dirty()
                && let Some(idx) = this
                    .tabs
                    .iter()
                    .position(|t| matches!(&t.content, PaneContent::Editor(v) if v == &view_entity))
            {
                this.tabs[idx].is_preview = false;
            }
            cx.notify();
        });
        (view, observer)
    }

    /// Clear the preview (italic/ephemeral) flag on the tab at `ix`, making it
    /// a permanent tab. The double-click-the-chip gesture: a preview tab opened
    /// by a single explorer click sticks instead of being replaced by the next
    /// preview open. No-op if the tab is already permanent or `ix` is stale.
    pub fn promote_tab_to_permanent(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(ix)
            && tab.is_preview
        {
            tab.is_preview = false;
            cx.notify();
        }
    }

    /// Open `path` as a reusable single-click preview tab (italic label). If
    /// the file is already open anywhere, just activate it. Otherwise reuse
    /// the active group's existing preview tab in place (so browsing the tree
    /// never piles up tabs); if none exists, open a fresh preview tab.
    pub fn open_preview_editor_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self.editor_tab_index(&path) {
            self.set_active(idx, window, cx);
            return idx;
        }
        // Reuse an existing preview tab in place, keeping its slot + preview-ness.
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| t.is_preview && matches!(t.kind, PaneGroupTabKind::Editor { .. }))
        {
            let (view, observer) = self.build_editor_view(&path, window, cx);
            let label = editor_tab_label(&path);
            let tab = &mut self.tabs[idx];
            tab.content = PaneContent::Editor(view);
            tab.kind = PaneGroupTabKind::Editor { path };
            tab.label = SharedString::from(label);
            tab.external_mutation = None;
            // Reset per-tab decorations from the file we replaced — a fresh
            // preview carries no custom title or color tag.
            tab.custom_title = None;
            tab.color = None;
            tab._observer = Some(observer);
            self.set_active(idx, window, cx);
            return idx;
        }
        self.open_or_activate_editor_tab_at(path, None, true, window, cx)
    }

    /// Give a freshly-mounted terminal a weak handle back to this group so
    /// Cmd-click on a `path:line:col` link can open it in an editor tab. An
    /// associated fn (not `&self`) so it can run while a sub-pane tree is
    /// borrowed mutably at the split/add call sites.
    pub(super) fn wire_opener(view: &gpui::Entity<TerminalView>, cx: &mut Context<Self>) {
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        // Auto-close: a clean child exit (status 0) asks the group to close the
        // hosting tab. Window-free here (subscribe has no `&mut Window`), so we
        // queue the session id and let `render` (which has a window) do the
        // actual `close_tab`.
        cx.subscribe(view, |this, _view, event, cx| match event {
            TerminalViewEvent::CleanExit { session_id } => {
                this.pending_clean_exit_closes.push(*session_id);
                cx.notify();
            }
        })
        .detach();
    }

    /// Drain `pending_clean_exit_closes`: for each session that reported a
    /// clean exit, close the pane it lived in. Cascades like Cmd+W —
    /// stacked leaf-tab → drop that tab; split leaf (its last tab) → drop the
    /// leaf; the tab's last remaining view → close the whole group tab.
    /// Called from `render`, the one place with a `&mut Window`.
    pub fn close_lone_exited_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_clean_exit_closes.is_empty() {
            return;
        }
        let sessions = std::mem::take(&mut self.pending_clean_exit_closes);
        for session in sessions {
            // Locate the exited view: (tab, leaf slot, leaf-tab idx) plus the
            // tab's total view count and that leaf's tab count, so we know
            // which rung of the cascade to take. Done first (immutable +
            // `view.read`) so the close mutation below holds no live borrow.
            let mut hit = None;
            for (tab_idx, tab) in self.tabs.iter().enumerate() {
                let PaneContent::Terminal(tree) = &tab.content else {
                    continue;
                };
                let mut total = 0usize;
                let mut found: Option<(usize, usize)> = None;
                for (slot, leaf_tab_idx, view) in tree.iter_all_views() {
                    total += 1;
                    if view.read(cx).session_id() == session {
                        found = Some((slot, leaf_tab_idx));
                    }
                }
                if let Some((slot, leaf_tab_idx)) = found {
                    let leaf_tab_count = tree.leaf(slot).map(|l| l.len()).unwrap_or(1);
                    hit = Some((tab_idx, slot, leaf_tab_idx, total, leaf_tab_count));
                    break;
                }
            }
            let Some((tab_idx, slot, leaf_tab_idx, total, leaf_tab_count)) = hit else {
                continue;
            };
            if total == 1 {
                // Lone view → close the whole group tab.
                self.close_tab(tab_idx, window, cx);
            } else if let PaneContent::Terminal(tree) = &mut self.tabs[tab_idx].content {
                if leaf_tab_count > 1 {
                    // Stacked leaf-tab → drop just that tab.
                    tree.close_tab_in_leaf(slot, leaf_tab_idx);
                } else {
                    // Split leaf holding only this view → drop the leaf.
                    tree.close_leaf(slot);
                }
                cx.notify();
            }
        }
    }

    /// Open `path` and move the editor cursor to the 1-based `line`/`col` as
    /// shown in terminal output (e.g. a clicked `src/foo.rs:42:7`). Converts
    /// to the editor's 0-based `Position`; the editor scrolls to it on the
    /// next paint via its focus-on-activation path. A `None` line just opens
    /// the file.
    pub fn open_editor_at_position(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_or_activate_editor_tab(path.clone(), window, cx);
        let Some(line) = line else {
            return;
        };
        let editor = self.tabs.iter().find_map(|t| match &t.content {
            PaneContent::Editor(v) if v.read(cx).file_path() == path.as_path() => Some(v.clone()),
            _ => None,
        });
        if let Some(view) = editor
            && let Some(state) = view.read(cx).state()
        {
            let position = gpui_component::input::Position {
                line: line.saturating_sub(1),
                character: col.unwrap_or(1).saturating_sub(1),
            };
            state.update(cx, |s, cx| s.set_cursor_position(position, window, cx));
        }
    }

    /// Open `path` as an editor tab, optionally inserting it at a specific
    /// VISUAL slot in the strip. `insert_at_visible_idx` semantics:
    /// `None` → append (tab lands at the end of the strip);
    /// `Some(idx)` → tab lands at visual position `idx` (clamped to the
    /// strip length, with pinned tabs always staying clustered at front).
    ///
    /// When the file is already open as a tab, the existing tab is moved
    /// to the requested slot AND activated — this matches the drag-onto-
    /// strip muscle memory: the user expects "drop here" to place the tab
    /// here regardless of whether it's new or pre-existing.
    pub fn open_or_activate_editor_tab_at(
        &mut self,
        path: PathBuf,
        insert_at_visible_idx: Option<usize>,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Already-open path: move + activate the existing tab.
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Editor { path: p } if p == &path))
        {
            // A permanent (non-preview) open of an already-previewed tab
            // promotes it to permanent.
            if !preview {
                self.tabs[idx].is_preview = false;
            }
            if let Some(visible_target) = insert_at_visible_idx
                && let Some(from) = self.visible_position_of(idx)
            {
                // `move_tab` removes `from` first then inserts at the
                // post-remove index. When `visible_target > from`, the
                // remove step shifts the destination left by one — so
                // adjust before clamping. Mirrors the same dance in
                // the chip-level TabDragPayload drop handler.
                let raw_to = visible_target.min(self.tab_order.len());
                let to = if raw_to > from { raw_to - 1 } else { raw_to };
                let bounded = to.min(self.tab_order.len().saturating_sub(1));
                self.move_tab(from, bounded);
            }
            // Route through `set_active` so `bump_mru` + `focus_active`
            // + `pin_tab_strip_to_end` + `cx.notify` all fire — same as
            // every other activation path in the file.
            self.set_active(idx, window, cx);
            return idx;
        }
        // New tab path — construct, push, then optionally re-slot inside
        // tab_order at the requested visible index.
        let (view, observer) = self.build_editor_view(&path, window, cx);
        let label = editor_tab_label(&path);
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Editor(view),
            kind: PaneGroupTabKind::Editor { path },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: preview,
            external_mutation: None,
            restore_rank: None,
            _observer: Some(observer),
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        // Keep MRU in step with every other tab-open path — without this
        // the new editor tab is invisible to the MRU switcher until the
        // user manually re-activates it.
        self.bump_mru(new_idx);
        if let Some(visible_target) = insert_at_visible_idx {
            let from = self.tab_order.len() - 1;
            let bounded = visible_target.min(from);
            // Skip the no-op (already at end) — `move_tab` would still
            // succeed, but `pin_tab_strip_to_end` below covers append case.
            if bounded < from {
                self.move_tab(from, bounded);
            }
        }
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        self.active
    }

    /// Open a read-only diff tab for `path` (staged-vs-HEAD when
    /// `staged=true`, worktree-vs-index otherwise). Idempotent: if a
    /// diff tab for the same (path, staged) pair already exists in this
    /// group, it's activated rather than duplicated.
    ///
    /// Constructs a fresh `DiffView` entity bound to `repo` and kicks
    /// off the patch fetch via `DiffView::load`. The DiffView is then
    /// mounted as `PaneContent::Diff`.
    pub fn open_or_activate_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Already-open (same path AND same staged flag) → activate.
        // `untracked` is not part of the tab key because a file can't be
        // both tracked and untracked at the same time; whichever variant
        // opened first wins until the user closes the tab.
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(
                &t.kind,
                PaneGroupTabKind::Diff { path: p, staged: s } if p == &path && *s == staged
            )
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        // New diff tab path. DiffView::new takes (repo, theme, density,
        // typography, cx). Then load(path, staged, untracked, cx) kicks
        // off the async fetch — the view paints a "Loading…" state until
        // the patch arrives.
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load(path_for_load, staged, untracked, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = {
            let leaf = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("diff")
                .to_string();
            // Suffix tells the user which side they're looking at. Kept
            // short so narrow tab strips don't truncate the filename.
            let suffix = if staged { " · staged" } else { " · diff" };
            SharedString::from(format!("{leaf}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::Diff { path, staged },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open a new embedded browser tab at `url`. Unlike editor/diff tabs,
    /// browser tabs don't dedup — every call appends a fresh tab, like a
    /// new terminal. Mirrors the diff opener's mount/activate sequence.
    pub fn open_browser_tab(
        &mut self,
        url: impl Into<String>,
        profile_id: Option<uuid::Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let url = url.into();
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let url_for_view = url.clone();
        let view = cx.new(|cx| {
            crate::shell::browser_view::BrowserView::new(
                url_for_view,
                profile_id,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = crate::shell::browser_view::host_label(&url);
        let tab = PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Browser(view),
            kind: PaneGroupTabKind::Browser { url },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate the singleton Tasks tab (GitHub issue / PR browser).
    ///
    /// If a Tasks tab already exists in this group it is activated rather than
    /// duplicated (singleton dedup — same pattern as diff tabs). Otherwise a
    /// fresh `TasksView` is constructed with `active_project`, the tab is
    /// appended and made active.
    /// Close the singleton Tasks tab if this group has one. Returns whether a
    /// tab was closed. Used when a workspace is created from the Tasks page —
    /// the browser has served its purpose, so the foreground returns to the
    /// group's prior tab (the pane-tab equivalent of returning the rail home).
    pub fn close_tasks_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Tasks))
        {
            self.close_tab(idx, window, cx);
            true
        } else {
            false
        }
    }

    pub fn open_or_activate_tasks_tab(
        &mut self,
        weak_root: WeakEntity<crate::workspace_root::WorkspaceRoot>,
        projects: Vec<oximux_core::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Singleton dedup: re-activate existing tab if one is already open.
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::Tasks)
        }) {
            // Keep the known-project set current; the page's scope is preserved.
            if let PaneContent::Tasks(view) = &self.tabs[idx].content {
                let view = view.clone();
                view.update(cx, |tv, cx| {
                    tv.set_projects(projects, cx);
                    tv.activate(cx);
                });
            }
            self.set_active(idx, window, cx);
            return idx;
        }
        // Construct a fresh TasksView for this pane group.
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let projects_for_view = projects;
        let view = cx.new(|cx| {
            let mut v = crate::shell::tasks_view::TasksView::new(
                weak_root,
                theme,
                density,
                typography,
                window,
                cx,
            );
            v.set_projects(projects_for_view, cx);
            v.activate(cx);
            v
        });
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let tab = PaneGroupTab {
            label: SharedString::from("Tasks"),
            content: PaneContent::Tasks(view),
            kind: PaneGroupTabKind::Tasks,
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open a new Agent Chat tab in this group, backed by its own headless
    /// `claude` subprocess (separate PID). Not a singleton — each chat is its
    /// own session, so a second call opens a second chat. The label is a
    /// running `Chat N` count over the group's existing chat tabs.
    pub fn open_agent_chat_tab(
        &mut self,
        cwd: PathBuf,
        model: Option<String>,
        backend: oximux_agents::thread::ChatBackend,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cwd_for_view = cwd.clone();
        let model_for_view = model.clone();
        let view = cx.new(|cx| {
            crate::shell::agent_chat::AgentChatView::new(
                cwd_for_view,
                model_for_view,
                backend,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        self.push_agent_chat_view(view, cwd, model, window, cx)
    }

    /// Open an **unbound** *New Agent* draft chat: no subprocess spawns until the
    /// first message. Seeds the provider-agnostic default (Claude stream-json);
    /// the composer's agent picker can retarget it before the first send, which
    /// binds the chosen transport. Shares the tab-push path with the bound
    /// [`open_agent_chat_tab`].
    pub fn open_agent_chat_tab_unbound(
        &mut self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cwd_for_view = cwd.clone();
        let backend = oximux_agents::thread::ChatBackend::stream_json();
        let view = cx.new(|cx| {
            crate::shell::agent_chat::AgentChatView::new_unbound(
                cwd_for_view,
                None,
                backend,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        self.push_agent_chat_view(view, cwd, None, window, cx)
    }

    /// Restore an Agent Chat tab: rehydrate the transcript and spawn the
    /// subprocess with `--resume <session_id>` (via `new_resumed`). Shares the
    /// tab-push path with [`open_agent_chat_tab`]; only the view construction
    /// differs. Called by the session-restore factory.
    #[allow(clippy::too_many_arguments)]
    pub fn open_agent_chat_tab_restored(
        &mut self,
        cwd: PathBuf,
        model: Option<String>,
        backend: oximux_agents::thread::ChatBackend,
        session_id: Option<String>,
        entries: Vec<oximux_agents::thread::ThreadEntry>,
        slash_commands: Vec<String>,
        thinking_level: crate::shell::agent_chat::ThinkingLevel,
        draft: Option<String>,
        queued: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cwd_for_view = cwd.clone();
        let model_for_view = model.clone();
        let view = cx.new(|cx| {
            crate::shell::agent_chat::AgentChatView::new_resumed(
                cwd_for_view,
                model_for_view,
                backend,
                session_id,
                entries,
                slash_commands,
                thinking_level,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        if draft.is_some() || !queued.is_empty() {
            view.update(cx, |v, cx| v.seed_draft_and_queue(draft, queued, window, cx));
        }
        self.push_agent_chat_view(view, cwd, model, window, cx)
    }

    /// Restore an UNBOUND *New Agent* draft: rebuild the unbound picker shape
    /// (no session, "New Agent" label, agent/model picker) and seed the persisted
    /// draft + queued text. Distinct from [`Self::open_agent_chat_tab_restored`],
    /// which restores an already-bound chat via `new_resumed`.
    pub fn open_agent_chat_tab_unbound_restored(
        &mut self,
        cwd: PathBuf,
        draft: Option<String>,
        queued: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cwd_for_view = cwd.clone();
        let backend = oximux_agents::thread::ChatBackend::stream_json();
        let view = cx.new(|cx| {
            crate::shell::agent_chat::AgentChatView::new_unbound(
                cwd_for_view,
                None,
                backend,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        if draft.is_some() || !queued.is_empty() {
            view.update(cx, |v, cx| v.seed_draft_and_queue(draft, queued, window, cx));
        }
        self.push_agent_chat_view(view, cwd, None, window, cx)
    }

    /// Shared tab-push for a freshly-built chat view (new or restored): assigns
    /// the running `Chat N` label, wires the repaint observer, and activates it.
    fn push_agent_chat_view(
        &mut self,
        view: Entity<crate::shell::agent_chat::AgentChatView>,
        cwd: PathBuf,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Hand the chat view a weak handle to this group so its `@terminal`
        // context provider can enumerate sibling terminal tabs. Weak — the group
        // already owns the view's `Entity`.
        let weak_group = cx.entity().downgrade();
        view.update(cx, |v, _cx| v.set_pane_group(weak_group));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        // Fold an in-chat model switch back into this tab's kind so the choice
        // survives relaunch (the layout persists the kind). Detached: it stops
        // firing and is cleaned up when the chat view is dropped (tab closed).
        cx.subscribe_in(
            &view,
            window,
            |this, v, ev: &crate::shell::agent_chat::AgentChatEvent, window, cx| {
                this.on_agent_chat_event(v, ev, window, cx);
            },
        )
        .detach();
        // An unbound *New Agent* draft reads as a draft ("New Agent") until its
        // first send binds it, at which point `TitleChanged` relabels it to the
        // chosen agent. A bound chat gets the running `Chat N` label.
        let label = if view.read(cx).is_unbound() {
            SharedString::from("New Agent")
        } else {
            let n = self
                .tabs
                .iter()
                .filter(|t| matches!(t.kind, PaneGroupTabKind::AgentChat { .. }))
                .count()
                + 1;
            SharedString::from(format!("Chat {n}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::AgentChat(view),
            kind: PaneGroupTabKind::AgentChat { cwd, model },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Handle an event raised by an Agent Chat view. Currently only a model
    /// switch: fold it into the owning tab's kind so a relaunch reopens the
    /// chat on the chosen model (the layout persists the kind).
    fn on_agent_chat_event(
        &mut self,
        view: &Entity<crate::shell::agent_chat::AgentChatView>,
        ev: &crate::shell::agent_chat::AgentChatEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            crate::shell::agent_chat::AgentChatEvent::ModelChanged(model) => {
                for tab in &mut self.tabs {
                    if let PaneContent::AgentChat(v) = &tab.content
                        && v.entity_id() == view.entity_id()
                        && let PaneGroupTabKind::AgentChat { model: m, .. } = &mut tab.kind
                    {
                        *m = Some(model.clone());
                    }
                }
                cx.notify();
            }
            crate::shell::agent_chat::AgentChatEvent::TitleChanged(title) => {
                // Update the tab's fallback label (a user's manual `custom_title`
                // rename still wins over it in the header render).
                for tab in &mut self.tabs {
                    if let PaneContent::AgentChat(v) = &tab.content
                        && v.entity_id() == view.entity_id()
                    {
                        tab.label = SharedString::from(title.clone());
                    }
                }
                cx.notify();
            }
            crate::shell::agent_chat::AgentChatEvent::ForkReady {
                cwd,
                model,
                session_id,
                entries,
                slash_commands,
                thinking_level,
            } => {
                // Open the truncated fork as a separate tab; the source tab is
                // untouched (this is the whole point of Fork vs Rewind). Fork is a
                // Claude-only feature (rewind/fork is hidden for Codex).
                self.open_agent_chat_tab_restored(
                    cwd.clone(),
                    model.clone(),
                    oximux_agents::thread::ChatBackend::stream_json(),
                    Some(session_id.clone()),
                    entries.clone(),
                    slash_commands.clone(),
                    *thinking_level,
                    None,
                    Vec::new(),
                    window,
                    cx,
                );
            }
            crate::shell::agent_chat::AgentChatEvent::OpenLoginTerminalRequested {
                adapter_id,
                cwd,
            } => {
                // Drop the user into the agent's interactive CLI at the chat's
                // cwd so `/login` is one command away. The bare binary is the
                // robust choice — it always lands where auth happens, without
                // guessing a login subcommand per CLI.
                let program = match *adapter_id {
                    "claude-code" => "claude",
                    "codex" => "codex",
                    // No interactive login binary wired for other adapters.
                    _ => return,
                };
                self.open_script_terminal_tab(
                    cwd.clone(),
                    SharedString::from("Sign in"),
                    program,
                    window,
                    cx,
                );
            }
        }
    }

    /// Toggle the agent-chat tab at insertion-order `ix` between chat and its
    /// companion terminal. The tab-header view-options menu's target: unlike the
    /// ⌃⇧V action (which hits the active tab), this addresses a specific tab so
    /// the menu works even if the click didn't first activate it. No-op unless the
    /// tab at `ix` is an agent chat.
    pub fn toggle_chat_terminal_at(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let PaneContent::AgentChat(view) = &tab.content else {
            return;
        };
        let view = view.clone();
        self.toggle_terminal_for_chat(view, window, cx);
    }

    /// Toggle the ACTIVE tab between chat and its companion terminal view — the
    /// ⌃⇧V action, routed here by the workspace root. No-op unless the active tab
    /// is an agent chat.
    pub fn toggle_active_chat_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let PaneContent::AgentChat(view) = &tab.content else {
            return;
        };
        let view = view.clone();
        self.toggle_terminal_for_chat(view, window, cx);
    }

    /// Toggle a specific agent-chat view between chat and its companion terminal.
    /// Switching to terminal spawns the resume terminal on first use (async, via
    /// the runtime); switching back just flips the mode (the terminal stays alive
    /// underneath so a re-toggle is instant). Shared by the ⌃⇧V action and the
    /// composer's terminal button.
    fn toggle_terminal_for_chat(
        &mut self,
        view: Entity<crate::shell::agent_chat::AgentChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // In terminal view → back to chat (no spawn).
        if view.read(cx).view_mode() == ChatViewMode::Terminal {
            view.update(cx, |v, cx| v.set_view_mode(ChatViewMode::Chat, window, cx));
            self.focus_active(window, cx);
            return;
        }
        // Companion already exists → just show it.
        if view.read(cx).has_companion_terminal() {
            view.update(cx, |v, cx| v.set_view_mode(ChatViewMode::Terminal, window, cx));
            self.focus_active(window, cx);
            return;
        }
        // First switch: spawn the resume terminal, then attach it.
        let Some(spec) = view.read(cx).terminal_launch_spec() else {
            crate::shell::toast::toast_op_error(
                cx,
                "Terminal view",
                "Send a message first — the terminal resumes this chat's session.",
            );
            return;
        };
        self.spawn_companion_terminal(view, spec, window, cx);
    }

    /// Spawn a companion terminal that resumes the chat's session interactively
    /// (`--resume`), then hand it to the chat view. Mirrors `spawn_agent_tab`'s
    /// runtime dance but mounts into the existing chat tab instead of a new tab,
    /// and skips status-hook injection + session-row persistence (the companion
    /// isn't a tracked agent tab — it's reaped when the chat tab closes).
    fn spawn_companion_terminal(
        &mut self,
        view: Entity<crate::shell::agent_chat::AgentChatView>,
        spec: ChatTerminalSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let runtime = self.cli_runtime.clone();
        // Per-agent launch defaults (model fallback + extra args).
        let (model, extra_args) = {
            let defaults = cx.try_global::<AgentLaunchSettings>();
            (
                spec.model.clone().or_else(|| defaults.and_then(|d| d.model_for(spec.adapter_id))),
                defaults.map(|d| d.args_for(spec.adapter_id)).unwrap_or_default(),
            )
        };
        let adapter = spec.adapter;
        let adapter_id = spec.adapter_id;
        let effort = spec.effort.clone();
        let cwd = spec.cwd.clone();
        let session_id = spec.session_id.clone();
        // A wired ACP preset (opencode) resumes through the generic `Custom`
        // adapter, which spawns `custom_command`'s `(program, argv)` verbatim and
        // ignores `resumption` (and `model`/`effort`/`extra_args` — a resume
        // replays the session as-is, so per-agent model/flag overrides
        // deliberately don't re-apply here). Its resume id already rides in the
        // argv (`opencode --session <id>`). Claude/Codex keep `custom_command:
        // None` and resume through their own `--resume` via `resumption`.
        let is_custom = adapter == AgentAdapter::Custom;
        let custom_command = is_custom
            .then(|| oximux_settings::acp_preset(adapter_id))
            .flatten()
            .and_then(|p| p.interactive_resume.map(|f| (p.command.to_string(), f(&session_id))));
        cx.spawn_in(window, async move |group, cx| {
            let cfg = AgentSessionConfig {
                adapter,
                worktree_path: cwd,
                prompt: None,
                model,
                effort,
                extra_args,
                env: Vec::new(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                custom_command,
                resumption: if is_custom {
                    SessionResumption::None
                } else {
                    SessionResumption::Resume { id: session_id }
                },
            };
            let session = match runtime.start_session(cfg).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        adapter = adapter_id,
                        "companion terminal start_session failed"
                    );
                    let _ = cx.update(|_, cx| {
                        crate::shell::toast::toast_op_error(cx, "Terminal view", &err.to_string());
                    });
                    return;
                }
            };
            let backend = match runtime.backend_for(session) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(?err, "companion terminal backend_for failed");
                    let _ = runtime.cancel(session).await;
                    return;
                }
            };
            let term_id = match runtime.terminal_session_id(session) {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!(?err, "companion terminal terminal_session_id failed");
                    let _ = runtime.cancel(session).await;
                    return;
                }
            };
            let attached = view.update_in(cx, |v, window, cx| {
                v.attach_terminal(session, backend, term_id, window, cx);
            });
            if attached.is_err() {
                // The chat tab was dropped mid-spawn — reap the orphan session.
                let _ = runtime.cancel(session).await;
                return;
            }
            // Route focus to the freshly-shown terminal.
            let _ = group.update_in(cx, |g, window, cx| g.focus_active(window, cx));
        })
        .detach();
    }

    /// Open a past session (chosen in the Session History side panel) as a chat tab.
    /// Already-open sessions just activate their tab; otherwise the transcript
    /// is imported from the session `.jsonl` and a resumed chat is opened.
    pub(crate) fn open_session_as_chat(
        &mut self,
        session_id: &str,
        path: Option<&str>,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Dedup: if this session is already open in a chat tab, activate it
        // rather than spawning a second `--resume` on the same session file.
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.content, PaneContent::AgentChat(v) if v.read(cx).session_id() == Some(session_id))
        }) {
            self.active = idx;
            self.bump_mru(idx);
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        // Import the transcript off the session log (bounded + capped inside).
        let entries = match path {
            Some(p) => oximux_agents::thread::transcript_from_jsonl(std::path::Path::new(p))
                .unwrap_or_default(),
            None => Vec::new(),
        };
        // Session-History reopen imports a Claude `.jsonl` session log.
        self.open_agent_chat_tab_restored(
            cwd,
            None,
            oximux_agents::thread::ChatBackend::stream_json(),
            Some(session_id.to_string()),
            entries,
            Vec::new(),
            crate::shell::agent_chat::ThinkingLevel::default(),
            None,
            Vec::new(),
            window,
            cx,
        );
    }

    /// Open or activate a commit-detail tab. Dedup key is the full
    /// SHA — clicking the same commit row twice activates the
    /// existing tab. `short_oid` and `subject` are display-only and
    /// land in the tab label.
    pub fn open_or_activate_commit_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        short_oid: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Commit { sha: s } if s == &sha))
        {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let sha_for_load = sha.clone();
        let short_for_load = short_oid.clone();
        let subject_for_load = subject.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_commit(sha_for_load, short_for_load, subject_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        // Tab label: short SHA + truncated subject. The tab strip
        // truncates anything long, so we keep the subject readable up
        // to a sane bound rather than trying to fit the entire commit
        // message.
        let label = {
            let subject_trim: String = subject.chars().take(50).collect();
            let suffix = if subject.chars().count() > 50 {
                "…"
            } else {
                ""
            };
            SharedString::from(format!("{short_oid}: {subject_trim}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::Commit { sha },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a read-only range-diff tab for one file from the
    /// "Committed on Branch" section. Dedup key is the path. `base`/`head`
    /// are the `merge_base`/`HEAD` OIDs the section was computed against;
    /// the `DiffView` loads `diff_for_range(base, head, path)`.
    pub fn open_or_activate_branch_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        base: String,
        head: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::BranchFile { path: p } if p == &path))
        {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let leaf = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("diff")
            .to_string();
        let title = leaf.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_range(base, head, path_for_load, title, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = SharedString::from(format!("{leaf} · branch"));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::BranchFile { path },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a combined multi-file diff tab for `scope`. Dedup
    /// key is the scope title ("All Changes" / "Staged Changes" /
    /// "Untracked" / "Branch Diff") so re-clicking the same "View all" CTA
    /// reactivates the existing tab. The new `DiffView` loads via
    /// `load_combined` — the same multi-file render path commit/branch tabs
    /// use, with per-file-group staging routing.
    pub fn open_or_activate_combined_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        scope: oximux_core::CombinedDiffScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Dedup by `tab_key` (range-aware for Branch) so switching branches
        // opens a fresh tab; the display label stays the shorter `title`.
        let scope_key = SharedString::from(scope.tab_key());
        let label = SharedString::from(scope.title());
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::CombinedDiff { scope_key: k } if k == &scope_key)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let scope_for_load = scope.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_combined(scope_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::CombinedDiff { scope_key },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

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
    ) -> usize {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        // Agent PTYs are spawned by the CLI runtime, not via spawn_local_pty,
        // so their shell env isn't threaded here in v1; the view still gets a
        // fresh identity for persistence/respawn parity with plain terminals.
        let ids = SurfaceIds::fresh(self.cwd.to_string_lossy().into_owned());
        let view = cx.new(|cx| {
            TerminalView::mount(
                backend, term_id, ids, theme, density, typography, window, cx,
            )
        });
        Self::wire_opener(&view, cx);
        let observer = Some(cx.observe(&view, |_this, _view, cx| cx.notify()));
        let label = match label_override {
            Some(s) => SharedString::from(s),
            None => {
                let current_labels: Vec<SharedString> =
                    self.tabs.iter().map(|t| t.label.clone()).collect();
                agent_tab_label::next_label_for(adapter_id, &current_labels)
            }
        };
        let status_task = spawn_status_task(
            status_rx.clone(),
            self.notifier.clone(),
            self.window_active.clone(),
            TabId::from(session_id),
            label.clone(),
            worktree_path.to_string_lossy().into_owned(),
            view.downgrade(),
            cx,
        );
        // Agent tabs are terminal-backed: wrap the agent PTY view in a
        // single-leaf sub-pane tree so Cmd+D can later add side PTYs.
        let agent_observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        self.tabs.push(PaneGroupTab {
            label,
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, agent_observer)),
            kind: PaneGroupTabKind::Agent {
                adapter,
                adapter_id,
                worktree_path,
                model,
                effort,
                session_id,
                status_rx,
            },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: Some(status_task),
        });
        let _ = observer; // legacy single-view observer no longer used
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        cx.notify();
        self.active
    }

    /// Remove a tab from this group by its insertion-order idx and
    /// return its owning `PaneGroupTab`. Used by the drag-to-split path
    /// to transfer entity ownership across `PaneGroup`s without rebuilding
    /// the inner terminal/editor view (preserves PTY + scrollback).
    ///
    /// Fixes `tab_order` (drops the entry pointing at `idx`, shifts the
    /// higher indices down by one) and `active` (decrements past the
    /// removed slot) so the source group renders correctly afterwards.
    pub fn take_tab(&mut self, idx: usize, cx: &mut Context<Self>) -> Option<PaneGroupTab> {
        if idx >= self.tabs.len() {
            return None;
        }
        // Refuse to tear out a pinned tab — pinning is a "keep here"
        // promise to the user. Drag-to-split drop becomes a no-op when
        // this fires; the drag overlay clears via the standard cancel
        // path.
        if self.tabs[idx].pinned {
            return None;
        }
        let removed = self.tabs.remove(idx);
        if let Some(pos) = self.tab_order.iter().position(|&i| i == idx) {
            self.tab_order.remove(pos);
        }
        for entry in self.tab_order.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        self.forget_mru(idx);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active == idx {
            // Active tab moved out — clamp to the last visible position.
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
        } else if idx < self.active {
            self.active -= 1;
        }
        cx.notify();
        Some(removed)
    }

    /// Append a `PaneGroupTab` that was moved in from another group.
    /// The original `_observer` is re-attached against this `cx` so the
    /// destination group re-renders on inner view updates. Returns the
    /// insertion-order index in the destination group.
    pub fn push_existing_tab(
        &mut self,
        mut tab: PaneGroupTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Drop the prior cx's observer (would point at the source group's
        // entity) and re-subscribe under this group so notify() fires
        // here on inner-view changes. For terminal tabs we re-attach to
        // the ACTIVE leaf's active view only. Background leaves / per-pane
        // tabs in a moved tab still repaint on focus (their inner
        // observers fire the source group); live cross-group repaint of a
        // non-active moved sub-tab is a known v1 limit.
        tab._observer = match &tab.content {
            PaneContent::Terminal(tree) => tree
                .active_view()
                .map(|view| cx.observe(view, |_this, _v, cx| cx.notify())),
            PaneContent::Editor(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
            // Diff tabs notify the host the same way as editor tabs.
            PaneContent::Diff(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
            // Browser tabs notify the host on title/URL/loading changes.
            PaneContent::Browser(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
            // Tasks tabs notify the host on list data / loading changes.
            PaneContent::Tasks(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
            // Agent Chat tabs notify the host on transcript/streaming changes.
            PaneContent::AgentChat(view) => Some(cx.observe(view, |_this, _v, cx| cx.notify())),
        };
        self.tabs.push(tab);
        self.tab_order.push(self.tabs.len() - 1);
        self.active = self.tabs.len() - 1;
        self.bump_mru(self.active);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        self.active
    }

    /// Append a pre-built terminal tab (used by the restore path).
    pub fn push_restored_terminal_tab(
        &mut self,
        label: String,
        view: gpui::Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        // Restored terminals also need a link opener, else Cmd-click is dead
        // after a session restore.
        Self::wire_opener(&view, cx);
        let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
        self.tabs.push(PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        });
        self.tab_order.push(self.tabs.len() - 1);
        self.pin_tab_strip_to_end();
        cx.notify();
    }

    /// Append a pre-built terminal tab with a fully-restored sub-pane
    /// tree. Restore path for multi-sub-pane tabs (Cmd+D splits): the
    /// caller constructs the `TerminalSplitTree` via `from_persisted`
    /// with all child views + observers already in place, then hands
    /// the whole tree over here. Observers stay inside the tree's
    /// per-leaf `observers` Vec; the tab-level `_observer` slot is
    /// unused for this path (single-leaf shape would use it, but the
    /// per-leaf observers in the tree drive notifications for every
    /// pane already).
    pub fn push_restored_terminal_tab_with_tree(
        &mut self,
        label: String,
        tree: TerminalSplitTree,
        cx: &mut Context<Self>,
    ) {
        // Wire the link opener into every restored leaf view (collect first to
        // drop the tree borrow before the per-view entity updates).
        let views: Vec<_> = tree.iter_all_views().map(|(_, _, v)| v.clone()).collect();
        for view in &views {
            Self::wire_opener(view, cx);
        }
        self.tabs.push(PaneGroupTab {
            label: SharedString::from(label),
            content: PaneContent::Terminal(tree),
            kind: PaneGroupTabKind::Terminal,
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: None,
            _status_task: None,
        });
        self.tab_order.push(self.tabs.len() - 1);
        self.pin_tab_strip_to_end();
        cx.notify();
    }

    /// Close entry-point for user-initiated single closes (the chip "✕" and
    /// Cmd+W). An editor tab with an unsaved buffer first prompts
    /// Save / Discard / Cancel via a modal overlay; everything else closes
    /// immediately. Bulk closes (Close Others / Close to Right) and the
    /// post-confirm path call [`Self::close_tab`] directly.
    pub fn request_close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let dirty_editor = self.tabs.get(idx).and_then(|tab| match &tab.content {
            PaneContent::Editor(view) if view.read(cx).is_dirty() => Some(view.clone()),
            _ => None,
        });
        match dirty_editor {
            Some(view) => self.mount_dirty_close_dialog(view, window, cx),
            None => self.close_tab(idx, window, cx),
        }
    }

    /// `true` while the unsaved-changes prompt is up — drives the render
    /// overlay.
    pub(crate) fn dirty_close_dialog(&self) -> Option<Entity<ConfirmDialog>> {
        self.dirty_close_dialog.clone()
    }

    /// Mount the unsaved-changes prompt for a dirty editor tab. Save writes
    /// then closes; Discard closes losing edits; Cancel keeps the tab. The
    /// tab is re-resolved by file path at choice time so an unrelated tab
    /// close (e.g. a terminal clean-exit) between mount and choice can't
    /// shift the index onto the wrong tab.
    fn mount_dirty_close_dialog(
        &mut self,
        view: Entity<oximux_editor::EditorView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Re-entry guard: one prompt at a time.
        if self.dirty_close_dialog.is_some() {
            return;
        }
        let path = view.read(cx).file_path().to_path_buf();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "This file".to_string());
        let group = cx.weak_entity();

        // Save → write the buffer, then close the (re-resolved) tab.
        let on_confirm: ConfirmCallback = {
            let group = group.clone();
            let view = view.clone();
            let path = path.clone();
            Rc::new(move |window, cx| {
                view.update(cx, |v, cx| {
                    v.save_if_dirty(cx);
                });
                let _ = group.update(cx, |g, cx| {
                    if let Some(idx) = g.editor_tab_index(&path) {
                        g.close_tab(idx, window, cx);
                    }
                });
            })
        };
        // Discard → close the (re-resolved) tab without saving.
        let on_discard: ConfirmCallback = {
            let group = group.clone();
            let path = path.clone();
            Rc::new(move |window, cx| {
                let _ = group.update(cx, |g, cx| {
                    if let Some(idx) = g.editor_tab_index(&path) {
                        g.close_tab(idx, window, cx);
                    }
                });
            })
        };

        let prompt = ConfirmPrompt {
            title: "Unsaved changes".into(),
            body: format!("{file_name} has unsaved changes. Save before closing?").into(),
            // Plain confirm — no type-to-confirm; closing one tab isn't worth
            // a typed gate.
            expected: "".into(),
            on_confirm,
            confirm_label: Some("Save".into()),
            on_cancel: None,
            secondary: Some(ConfirmSecondary {
                label: "Discard".into(),
                on_click: on_discard,
            }),
        };

        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));

        // Focus the dialog so Enter (Save) / Escape (Cancel) work without a
        // click — the prompt has no inner Input to grab focus on its own.
        dialog.read(cx).focus_handle(cx).focus(window, cx);

        // Drop the dialog the moment the user resolves it. Replacing the
        // observer cancels any stale one.
        self._dirty_close_observer =
            Some(
                cx.observe_in(&dialog, window, |group, dialog, _window, cx| {
                    let d = dialog.read(cx);
                    if d.is_confirmed() || d.is_cancelled() {
                        group.dirty_close_dialog = None;
                        group._dirty_close_observer = None;
                        cx.notify();
                    }
                }),
            );
        self.dirty_close_dialog = Some(dialog);
        cx.notify();
    }

    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        let removed = self.tabs.remove(idx);
        // Drop the removed index from `tab_order`, then decrement every
        // remaining entry that pointed past it so the vector stays
        // consistent with the now-shifted `tabs` vector.
        if let Some(pos) = self.tab_order.iter().position(|&i| i == idx) {
            self.tab_order.remove(pos);
        }
        for entry in self.tab_order.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
        debug_assert_eq!(self.tabs.len(), self.tab_order.len());
        self.forget_mru(idx);
        if let PaneGroupTabKind::Agent { session_id, .. } = removed.kind {
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_this, _cx| {
                if let Err(err) = runtime.cancel(session_id).await {
                    tracing::warn!(?err, "pane-group close_tab: agent cancel failed");
                }
            })
            .detach();
        }
        // A chat tab may have spawned a companion terminal (its own daemon
        // session); reap it too so toggling to terminal view then closing the tab
        // doesn't orphan a live CLI.
        if let PaneContent::AgentChat(view) = &removed.content
            && let Some(companion) = view.read(cx).companion_session_id()
        {
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_this, _cx| {
                if let Err(err) = runtime.cancel(companion).await {
                    tracing::warn!(?err, "pane-group close_tab: companion terminal cancel failed");
                }
            })
            .detach();
        }
        if self.tabs.is_empty() {
            self.active = 0;
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Close every tab in this group except `keep_idx` and any pinned
    /// tabs. Iterates in reverse so each `close_tab` call sees stable
    /// indices for the untouched portion. No-op when `keep_idx` is out
    /// of range.
    pub fn close_others(&mut self, keep_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if keep_idx >= self.tabs.len() {
            return;
        }
        for idx in (0..self.tabs.len()).rev() {
            if idx == keep_idx {
                continue;
            }
            if self.tabs[idx].pinned {
                continue;
            }
            self.close_tab(idx, window, cx);
        }
    }

    /// Close every tab whose index is greater than `from_idx`, skipping
    /// pinned tabs. Reverse iteration keeps each `close_tab` index
    /// valid against the still-unprocessed tail.
    pub fn close_to_right(&mut self, from_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        if from_idx + 1 >= len {
            return;
        }
        for idx in (from_idx + 1..len).rev() {
            if self.tabs[idx].pinned {
                continue;
            }
            self.close_tab(idx, window, cx);
        }
    }

    /// Close every tab in this group. The empty group is purged by
    /// `ProjectPanes::purge_empty_groups` on the next render frame.
    pub fn close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for idx in (0..self.tabs.len()).rev() {
            self.close_tab(idx, window, cx);
        }
    }

    /// Assign a color tag (or clear with `None`) to the tab at `idx`.
    /// The chip renders a 2px left-edge bar in the chosen color.
    pub fn set_tab_color(&mut self, idx: usize, color: Option<TabColor>, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.color = color;
            cx.notify();
        }
    }

    /// Override the tab's visible title with `title` (or restore the
    /// default by passing `None`). The chip and persistence read
    /// `custom_title.unwrap_or(label)`.
    ///
    /// Title changes can widen the chip; snapshot pin state and schedule
    /// a deferred re-pin so the strip stays anchored to the new active
    /// tab if the user was at the right edge.
    pub fn set_tab_title(
        &mut self,
        idx: usize,
        title: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let was_pinned = self.was_pinned_to_end();
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.custom_title = title;
            cx.notify();
        }
        self.schedule_repin_if_was_pinned(was_pinned, cx);
    }

    /// Resolve the tab's visible title — custom override if set, else
    /// the default label. Used by the chip render and persistence.
    pub fn visible_title(&self, idx: usize) -> Option<SharedString> {
        let tab = self.tabs.get(idx)?;
        Some(
            tab.custom_title
                .clone()
                .unwrap_or_else(|| tab.label.clone()),
        )
    }

    pub fn set_active(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        // Bump MRU first so the switcher overlay reflects the new
        // ordering even when the tab is already active (re-selecting it
        // still confirms it as most-recent).
        self.bump_mru(idx);
        let changed = idx != self.active;
        self.active = idx;
        // Land keyboard focus on the active tab's content so it's ready for
        // input — even when re-selecting the already-active tab. The focus is
        // DEFERRED to the next frame: focusing synchronously here is clobbered
        // by GPUI's post-click focus dispatch when activation comes from a
        // tab-bar or sidebar-rail click, which would otherwise leave the
        // terminal unfocused (a second click was needed before typing).
        let group = cx.entity();
        window.defer(cx, move |window, app| {
            group.update(app, |g, cx| g.focus_active(window, cx));
        });
        if changed {
            cx.notify();
        }
    }

    /// Move `idx` to the front of the MRU queue (deduped). Called from
    /// every path that activates a tab: set_active, new-tab spawn
    /// paths, drag-transfer push. Cheap O(n) — n = visible tab count.
    fn bump_mru(&mut self, idx: usize) {
        self.mru.retain(|&i| i != idx);
        self.mru.insert(0, idx);
    }

    /// Drop `idx` from the MRU queue and decrement every later entry
    /// so MRU indices stay aligned with the post-remove `tabs` Vec.
    /// Called from close_tab.
    fn forget_mru(&mut self, idx: usize) {
        self.mru.retain(|&i| i != idx);
        for entry in self.mru.iter_mut() {
            if *entry > idx {
                *entry -= 1;
            }
        }
    }
}
