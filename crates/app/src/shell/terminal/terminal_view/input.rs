use super::*;

impl TerminalView {
    pub(crate) fn on_search(&mut self, _: &Search, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_search(cx);
    }

    /// Cmd-Shift-I: extract the active selection's text and dispatch a
    /// `SendTextToActiveAgent` payload action up the tree. `WorkspaceRoot`
    /// resolves the destination agent and writes the bytes via the CLI
    /// runtime. No-op (with a debug trace) when the pane has no selection.
    pub(super) fn on_send_selection_to_agent(
        &mut self,
        _: &SendTerminalSelectionToAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sel) = self.selection else {
            tracing::debug!("send-to-agent: no selection");
            return;
        };
        let text = extract_selection_text(&self.snapshot, sel);
        if text.is_empty() {
            return;
        }
        window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
    }

    /// Cmd-Shift-O: extract the most-recent COMPLETED command's output
    /// from the visible viewport — bracketed by the last two
    /// `PromptStart` marks — and dispatch it. Requires at least two
    /// prompt marks (one before, one after the command). Falls back to
    /// a debug trace when shell-integration marks aren't present.
    pub(super) fn on_send_last_command_output_to_agent(
        &mut self,
        _: &SendLastCommandOutputToAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self.last_completed_command_output() else {
            tracing::debug!("send-output-to-agent: no completed command in scope");
            return;
        };
        if text.is_empty() {
            return;
        }
        window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
    }

    /// Plain text of the most-recently COMPLETED command's output,
    /// bracketed by the last two `PromptStart` marks. Returns `None`
    /// when fewer than two marks are present (e.g. shell-integration
    /// not wired).
    ///
    /// The output band is `[prev_prompt.line + 1, last_prompt.line - 1]` in
    /// absolute history-line coords. The full backend grid (history + visible)
    /// is row-indexed by that same absolute line while scrollback hasn't
    /// capped, so the band extracts directly from it — capturing output that
    /// scrolled OFF the visible viewport. Only when the grid is unavailable or
    /// the band falls outside it do we fall back to the viewport-clamped
    /// extraction, warning that the result may be partial.
    fn last_completed_command_output(&self) -> Option<String> {
        let n = self.command_marks.len();
        if n < 2 {
            return None;
        }
        let prev = &self.command_marks[n - 2];
        let last = &self.command_marks[n - 1];
        // Output band is exclusive of both prompt lines themselves.
        let band_start = prev.line.saturating_add(1) as usize;
        let band_end = last.line.saturating_sub(1) as usize;
        if band_end < band_start {
            return None;
        }
        // Preferred path: extract from the full history+visible grid, where
        // row index == absolute mark line (stable until the scrollback caps +
        // evicts, which only touches OLD commands — never the last one).
        let id = self.session_id;
        let grid = self.with_backend(|be| be.search_grid(id));
        if !grid.is_empty() && band_end < grid.len() {
            // Full-width band: end_col `MAX` clamps to each row's real width.
            return Some(extract_selection_text_cells(
                &grid,
                (band_start, 0, band_end, usize::MAX),
            ));
        }

        // Fallback: clamp the band into the visible viewport. If it sits
        // entirely off-screen there's nothing to extract.
        let rows = self.snapshot.cells.len();
        if rows == 0 {
            return None;
        }
        let base = self.snapshot.history_len as i64 - self.snapshot.display_offset as i64;
        let raw_start = band_start as i64 - base;
        let raw_end = band_end as i64 - base;
        if raw_end < 0 || raw_start >= rows as i64 {
            return None;
        }
        let screen_start = raw_start.max(0) as usize;
        let screen_end = raw_end.min((rows - 1) as i64) as usize;
        if raw_start < 0 {
            tracing::warn!(
                "send-last-output: command output scrolled above the retained grid; sending the visible portion only"
            );
        }
        Some(self.snapshot.rows_text(screen_start, screen_end))
    }

    pub(super) fn rerun_search(&mut self, cx: &mut Context<Self>) {
        // Clone the full history+visible grid OFF the main thread: the copy of
        // a large scrollback under the backend mutex would otherwise block the
        // GPUI run loop AND the relay reader (which needs the same lock to
        // drain output). Bump a generation so a slower clone landing out of
        // order can't apply stale matches over a newer scan.
        self.search_scan_gen = self.search_scan_gen.wrapping_add(1);
        let my_gen = self.search_scan_gen;
        let session_id = self.session_id;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let grid = cx
                .background_executor()
                .spawn(async move {
                    backend
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .search_grid(session_id)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                // A newer rerun superseded this clone — drop the stale result.
                if view.search_scan_gen != my_gen {
                    return;
                }
                let visible = view.snapshot.cells.len();
                view.search.rerun(&grid, visible);
                // Find-as-you-type lands on the first hit; jump the viewport to
                // it the same way cycling does.
                view.follow_current_match();
                cx.notify();
            });
        })
        .detach();
    }

    /// Scroll the viewport so the cycled match is visible, then refresh the
    /// snapshot so the very next paint shows the new window (the regular
    /// poll-driven resnapshot would otherwise lag a frame behind the
    /// highlight). No-op when the match is already on screen.
    pub(super) fn follow_current_match(&mut self) {
        let visible = self.snapshot.cells.len();
        let Some(delta) = self
            .search
            .follow_delta(visible, self.snapshot.display_offset)
        else {
            return;
        };
        let id = self.session_id;
        if let Err(err) = self.with_backend(|be| be.scroll(id, delta)) {
            tracing::warn!(?err, "match-follow scroll failed");
            return;
        }
        if let Ok(snapshot) = self.with_backend(|be| be.snapshot(id)) {
            self.snapshot = Arc::new(snapshot);
            self.revalidate_hover();
        }
    }

    /// Cycle to the next search match (wrapping) and follow it. Bound to a
    /// registry chord; a closed overlay makes it a silent no-op so the
    /// chord never surprises outside search mode.
    pub(crate) fn on_find_next(
        &mut self,
        _: &FindNextMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search.active {
            return;
        }
        self.search.next_match();
        self.follow_current_match();
        cx.notify();
    }

    /// Cycle to the previous search match. See [`Self::on_find_next`].
    pub(crate) fn on_find_prev(
        &mut self,
        _: &FindPrevMatch,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.search.active {
            return;
        }
        self.search.prev_match();
        self.follow_current_match();
        cx.notify();
    }

    /// Debounce keystroke-driven reruns: fetching the search grid clones
    /// the entire scrollback out of the emulator under its lock, so doing
    /// it per keystroke makes fast typing churn. Each edit bumps the
    /// generation and arms one short timer; only the newest generation's
    /// timer actually rescans. Open/toggle/next-match paths stay
    /// immediate (single events, no churn).
    fn schedule_debounced_search(&mut self, cx: &mut Context<Self>) {
        const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(60);
        self.search_debounce_gen = self.search_debounce_gen.wrapping_add(1);
        let my_gen = self.search_debounce_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |view, cx| {
                if view.search_debounce_gen == my_gen && view.search.active {
                    view.rerun_search(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Map a window-space pointer position to a `(row, col)` cell, clamped to
    /// the live grid, using the bounds captured by the last paint.
    pub(super) fn cell_at(&self, pos: Point<Pixels>, window: &Window) -> (usize, usize) {
        let metrics = CellMetrics::measure(&self.typography, window);
        let bounds = self.canvas_bounds.get();
        let (row, col) = point_to_cell(pos, bounds, &metrics, self.density.pad_panel);
        let rows = self.snapshot.cells.len();
        if rows == 0 {
            return (0, 0);
        }
        let row = row.min(rows - 1);
        let cols = self.snapshot.cells[row].len();
        (row, col.min(cols.saturating_sub(1)))
    }

    /// Begin (or shift-extend) a mouse selection. Click count picks the
    /// granularity: 1 = char (free drag), 2 = word, 3+ = line.
    pub(super) fn on_select_down(&mut self, ev: &MouseDownEvent, window: &mut Window) {
        let cell = self.cell_at(ev.position, window);
        let kind = match ev.click_count {
            2 => SelectKind::Word,
            n if n >= 3 => SelectKind::Line,
            _ => SelectKind::Char,
        };
        if ev.modifiers.shift && let Some((sr, sc, _, _)) = self.selection {
            // Extend from the existing selection's start.
            self.selecting = Some(SelectDrag {
                anchor: (sr, sc),
                kind,
            });
        } else {
            // Fresh selection. A plain char-click clears any prior highlight
            // and waits for a drag before painting one (matches Terminal.app:
            // a bare click positions focus, it does not select).
            self.selection = None;
            self.selecting = Some(SelectDrag { anchor: cell, kind });
        }
        // Word/line highlight immediately; a shift-extend updates now too.
        // A plain char-click waits for the first drag move.
        if kind != SelectKind::Char || ev.modifiers.shift {
            self.apply_drag(cell);
        }
    }

    /// Recompute `self.selection` from the active drag anchor to `current`.
    pub(super) fn apply_drag(&mut self, current: (usize, usize)) {
        let Some(drag) = self.selecting.as_ref() else {
            return;
        };
        let anchor = drag.anchor;
        let kind = drag.kind;
        let sel = match kind {
            SelectKind::Char => order_points(anchor, current),
            SelectKind::Word => {
                // Union: earliest word-start → latest word-end in reading order.
                let a = self.word_span(anchor);
                let c = self.word_span(current);
                let start = a.0.min(c.0);
                let end = a.1.max(c.1);
                (start.0, start.1, end.0, end.1)
            }
            SelectKind::Line => {
                let r0 = anchor.0.min(current.0);
                let r1 = anchor.0.max(current.0);
                let last_col = (self.snapshot.cols as usize).saturating_sub(1);
                (r0, 0, r1, last_col)
            }
        };
        self.selection = Some(sel);
    }

    /// Inclusive (start, end) cell points of the word at `(row, col)`.
    fn word_span(&self, (row, col): (usize, usize)) -> ((usize, usize), (usize, usize)) {
        match self.snapshot.cells.get(row) {
            Some(cells) => {
                let (s, e) = word_range_at(cells, col);
                ((row, s), (row, e))
            }
            None => ((row, col), (row, col)),
        }
    }

    /// End an in-flight selection. Returns whether a repaint is needed. A
    /// char drag that never left its origin cell leaves no highlight (so a
    /// plain click does not paint a one-cell selection). When `copy_on_select`
    /// is enabled, a non-empty finished selection is auto-copied to the
    /// clipboard (the selection itself stays painted — no Cmd+C needed).
    pub(super) fn finish_select(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.selecting.take() else {
            return false;
        };
        if drag.kind == SelectKind::Char
            && let Some((r0, c0, r1, c1)) = self.selection
            && r0 == r1
            && c0 == c1
        {
            self.selection = None;
        }
        if let Some(sel) = self.selection
            && terminal_settings(cx).copy_on_select
        {
            let text = extract_selection_text(&self.snapshot, sel);
            if !text.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
        }
        true
    }

    /// Forward a mouse event to the child when the app has enabled mouse
    /// reporting and Shift is NOT held (Shift is the escape hatch for local
    /// selection over a mouse-mode app). Returns `true` when it consumed the
    /// event, so the caller skips local selection.
    pub(super) fn report_mouse(
        &mut self,
        button: MouseButton,
        pos: Point<Pixels>,
        modifiers: &Modifiers,
        action: MouseAction,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Latch the gesture: once a LOCAL selection drag is in flight, never
        // forward its continuation (drag/release) to a mouse-reporting app —
        // even if Shift (the local-selection escape hatch) was released
        // mid-drag. Without this, releasing Shift while selecting starts
        // leaking spurious drag/release reports into the app.
        if self.selecting.is_some() && matches!(action, MouseAction::Drag | MouseAction::Release) {
            return false;
        }
        if modifiers.shift {
            return false;
        }
        let id = self.session_id;
        let mode = self.with_backend(|be| be.mouse_mode(id));
        if !mode.any_reporting() {
            return false;
        }
        let Some(btn) = map_btn(button) else {
            return false;
        };
        let cell = self.cell_at(pos, window);
        let mods = mod_bits(modifiers.shift, modifiers.alt, modifiers.control);
        match encode_button(action, btn, cell, mods, &mode) {
            Some(bytes) => {
                self.send_bytes(&bytes, cx);
                true
            }
            None => false,
        }
    }

    /// Find a link at the given cell. OSC 8 explicit hyperlinks (carried on
    /// the snapshot) take priority; otherwise plain-text detection runs over
    /// the row's characters.
    fn link_at(&self, row: usize, col: usize) -> Option<LinkMatch> {
        if let Some(span) = self
            .snapshot
            .links
            .iter()
            .find(|l| l.row == row && col >= l.col_start && col <= l.col_end)
        {
            return Some(LinkMatch {
                target: LinkTarget::Url(span.uri.clone()),
                col_start: span.col_start,
                col_end: span.col_end,
            });
        }
        let cells = self.snapshot.cells.get(row)?;
        let chars: Vec<char> = cells
            .iter()
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect();
        detect_at(&chars, col)
    }

    /// The link token under `(row, col)` as a string the context-menu open
    /// action can carry: URLs verbatim, paths formatted as `path[:line[:col]]`.
    /// `None` when no link sits there. Re-classified on the open side via
    /// [`crate::shell::terminal_links::classify_link`].
    pub(super) fn link_string_at(&self, row: usize, col: usize) -> Option<String> {
        let hit = self.link_at(row, col)?;
        Some(match hit.target {
            LinkTarget::Url(u) => u,
            LinkTarget::Path { path, line, col } => {
                let mut s = path.to_string_lossy().into_owned();
                if let Some(l) = line {
                    s.push(':');
                    s.push_str(&l.to_string());
                    if let Some(c) = col {
                        s.push(':');
                        s.push_str(&c.to_string());
                    }
                }
                s
            }
        })
    }

    /// Resolve a possibly-relative link path against the session's OSC 7 cwd,
    /// falling back to the path as-is when no cwd is known. A leading `~`
    /// component expands to the home directory (common in `ls`/`fd` output).
    fn resolve_path(&mut self, path: &std::path::Path) -> PathBuf {
        if let Ok(rest) = path.strip_prefix("~")
            && let Some(home) = dirs::home_dir()
        {
            return home.join(rest);
        }
        if path.is_absolute() {
            return path.to_path_buf();
        }
        let id = self.session_id;
        match self.with_backend(|be| be.cwd_hint(id)) {
            Some(cwd) => cwd.join(path),
            // No OSC 7 cwd (the shell never emitted one — the default on a
            // bare macOS zsh). Fall back to the shell's live cwd via libproc on
            // its pid, matching how cwd is resolved elsewhere; only if that
            // also fails do we leave the path relative.
            None => match self.os_pid().and_then(crate::shell::cwd_resolver::cwd_of_pid) {
                Some(cwd) => cwd.join(path),
                None => path.to_path_buf(),
            },
        }
    }

    /// True once the async existence check has confirmed `path` on disk.
    /// A cache miss records `Pending` and spawns the stat on the background
    /// executor (never the foreground -- hover/paint must stay IO-free),
    /// then notifies, so the underline lights up without a mouse move once
    /// the answer lands.
    fn path_link_ready(&mut self, path: &std::path::Path, cx: &mut Context<Self>) -> bool {
        let resolved = self.resolve_path(path);
        let now = std::time::Instant::now();
        match self.link_exists.lookup(&resolved, now) {
            Some(Existence::Exists) => true,
            Some(Existence::Pending | Existence::Missing) => false,
            None => {
                // Bound concurrent stat tasks: at the cap, leave the entry
                // UNRECORDED so a later hover retries it instead of pinning a
                // permanent `Pending` (which would suppress the underline).
                if self.link_stat_inflight >= MAX_INFLIGHT_LINK_STATS {
                    return false;
                }
                self.link_exists
                    .record(resolved.clone(), Existence::Pending, now);
                self.link_stat_inflight += 1;
                cx.spawn(async move |this, cx| {
                    let stat_path = resolved.clone();
                    let exists = cx
                        .background_executor()
                        .spawn(async move { stat_path.exists() })
                        .await;
                    let state = if exists {
                        Existence::Exists
                    } else {
                        Existence::Missing
                    };
                    let _ = this.update(cx, |view, cx| {
                        view.link_stat_inflight = view.link_stat_inflight.saturating_sub(1);
                        view.link_exists
                            .record(resolved, state, std::time::Instant::now());
                        if exists {
                            cx.notify();
                        }
                    });
                })
                .detach();
                false
            }
        }
    }

    /// The hovered span filtered to links we will actually act on: URLs
    /// always; paths only once existence is confirmed. Re-derives the
    /// target from the live snapshot (a single-row token scan, no IO), so
    /// the paint path picks up a confirmation that arrived via notify.
    pub(super) fn underlinable_hover(&mut self, cx: &mut Context<Self>) -> Option<(usize, usize, usize)> {
        let span = self.hovered_link?;
        match self.link_at(span.0, span.1)?.target {
            LinkTarget::Url(_) => Some(span),
            LinkTarget::Path { path, .. } => self.path_link_ready(&path, cx).then_some(span),
        }
    }

    /// Open a detected link: URLs via the macOS system handler, paths via the
    /// host pane group's editor (at the parsed line/col).
    fn open_link(&mut self, target: LinkTarget, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            LinkTarget::Url(url) => {
                if let Err(err) = std::process::Command::new("open").arg(&url).spawn() {
                    tracing::warn!(?err, %url, "failed to open url");
                }
            }
            LinkTarget::Path { path, line, col } => {
                let resolved = self.resolve_path(&path);
                if let Some(opener) = self.opener.clone() {
                    let _ = opener.update(cx, |pg, cx| {
                        pg.open_editor_at_position(resolved, line, col, window, cx);
                    });
                }
            }
        }
    }

    /// On Cmd+left-down over a link, open it and report consumption so the
    /// click doesn't also start a selection.
    pub(super) fn try_open_link(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !ev.modifiers.platform {
            return false;
        }
        let (row, col) = self.cell_at(ev.position, window);
        let Some(hit) = self.link_at(row, col) else {
            return false;
        };
        // Path links open only once existence is confirmed -- mirrors the
        // underline gate, so a click can never act on a span that was not
        // showing as clickable. The ready check also kicks the async stat,
        // so an eager click on an unconfirmed path "arms" it for the next.
        if let LinkTarget::Path { path, .. } = &hit.target
            && !self.path_link_ready(&path.clone(), cx)
        {
            return false;
        }
        self.open_link(hit.target, window, cx);
        true
    }

    /// Re-check the hovered link against the current snapshot after a refresh:
    /// keep it (updating the span) if a link still sits at its start cell,
    /// else drop it. Avoids underlining stale content after the grid changes
    /// while also not flickering off a still-valid link during streaming output.
    pub(super) fn revalidate_hover(&mut self) {
        if let Some((row, c0, _)) = self.hovered_link {
            self.hovered_link = self
                .link_at(row, c0)
                .map(|hit| (row, hit.col_start, hit.col_end));
        }
    }

    /// Update the Cmd-hover link underline. Called on mouse-move; clears the
    /// highlight when Cmd isn't held or the pointer isn't over a link.
    pub(super) fn update_hover(&mut self, ev: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let next = if ev.modifiers.platform {
            let (row, col) = self.cell_at(ev.position, window);
            match self.link_at(row, col) {
                Some(hit) => {
                    // Kick (or refresh) the async existence check for path
                    // targets; the paint-side gate owns the underline call.
                    if let LinkTarget::Path { path, .. } = &hit.target {
                        let _ = self.path_link_ready(&path.clone(), cx);
                    }
                    Some((row, hit.col_start, hit.col_end))
                }
                None => None,
            }
        } else {
            None
        };
        if next != self.hovered_link {
            self.hovered_link = next;
            cx.notify();
        }
    }

    /// Wheel handling, in priority order: forward to a mouse-reporting app;
    /// else translate to arrow keys on the alt-screen (less/man); else scroll
    /// local scrollback (Phase 3).
    pub(super) fn on_wheel(&mut self, ev: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let metrics = CellMetrics::measure(&self.typography, window);
        let line_height = metrics.line_height;
        let mult = terminal_settings(cx).scroll_multiplier;
        // A new gesture starts fresh so a leftover sub-line remainder from the
        // previous one can't bias its first step.
        if matches!(ev.touch_phase, TouchPhase::Started) {
            self.scroll_px = 0.0;
        }
        let delta_px = f32::from(ev.delta.pixel_delta(px(line_height)).y) * mult;
        let lines = accumulate_scroll_lines(&mut self.scroll_px, delta_px, line_height);
        if lines == 0 {
            return;
        }
        let id = self.session_id;
        let mode = self.with_backend(|be| be.mouse_mode(id));
        let up = lines > 0;
        let count = lines.unsigned_abs() as usize;

        if mode.any_reporting() {
            let cell = self.cell_at(ev.position, window);
            let mods = mod_bits(ev.modifiers.shift, ev.modifiers.alt, ev.modifiers.control);
            if let Some(bytes) = encode_scroll(up, cell, mods, &mode) {
                self.send_bytes(&bytes.repeat(count), cx);
                return;
            }
        }

        if mode.alt_screen && mode.alternate_scroll {
            let app_cursor = self.with_backend(|be| be.input_mode(id)).app_cursor;
            let arrow: &[u8] = match (up, app_cursor) {
                (true, false) => b"\x1b[A",
                (true, true) => b"\x1bOA",
                (false, false) => b"\x1b[B",
                (false, true) => b"\x1bOB",
            };
            self.send_bytes(&arrow.repeat(count), cx);
            return;
        }

        self.scroll_viewport(lines, cx);
    }

    /// Scroll the local viewport by `delta` lines (+ = back into history, - =
    /// toward the live tail) and repaint this frame. Shared by the wheel,
    /// scrollbar-drag, and keyboard scroll paths. Re-fetches the snapshot
    /// because the poll loop (`tick`) only resnapshots on new PTY output — so on
    /// an idle pane (e.g. after `cat` of a long file finishes draining) the grid
    /// and the `↑ N lines` chip would otherwise freeze and scrolling look dead.
    /// A zero/over-scroll delta is a no-op (alacritty clamps to the history).
    fn scroll_viewport(&mut self, delta: i32, cx: &mut Context<Self>) {
        if delta == 0 {
            return;
        }
        let id = self.session_id;
        if let Err(err) = self.with_backend(|be| be.scroll(id, delta)) {
            tracing::warn!(?err, "pty scroll failed");
            return;
        }
        if let Ok(snapshot) = self.with_backend(|be| be.snapshot(id)) {
            self.snapshot = Arc::new(snapshot);
            self.revalidate_hover();
        }
        cx.notify();
    }

    /// One page of scrollback in lines, with a single line of overlap kept for
    /// context across the jump. Floors at 1 so a tiny pane still advances.
    fn page_lines(&self) -> i32 {
        (self.snapshot.cells.len() as i32 - 1).max(1)
    }

    /// Run a keyboard scroll command against the local scrollback. Only reached
    /// on the main screen (the caller forwards these keys to the app on the
    /// alt-screen / mouse-reporting modes, where scrollback doesn't apply).
    fn handle_scroll_command(&mut self, cmd: ScrollCmd, cx: &mut Context<Self>) {
        match cmd {
            ScrollCmd::PageUp => self.scroll_viewport(self.page_lines(), cx),
            ScrollCmd::PageDown => self.scroll_viewport(-self.page_lines(), cx),
            ScrollCmd::Tail => self.scroll_to_tail(cx),
        }
    }

    /// Snap the viewport back to the live tail and repaint immediately. Wired to
    /// the scrolled-up indicator so a click jumps to the bottom; keyboard input
    /// reaches the tail for free via `send_bytes` (the echo resnapshots), so it
    /// doesn't need this. Re-fetches the snapshot for the same reason `on_wheel`
    /// does — no PTY output is in flight to drive the poll-loop resnapshot.
    pub(super) fn scroll_to_tail(&mut self, cx: &mut Context<Self>) {
        let id = self.session_id;
        self.scroll_px = 0.0;
        if let Err(err) = self.with_backend(|be| be.scroll_to_bottom(id)) {
            tracing::warn!(?err, "pty scroll-to-bottom failed");
            return;
        }
        if let Ok(snapshot) = self.with_backend(|be| be.snapshot(id)) {
            self.snapshot = Arc::new(snapshot);
            self.revalidate_hover();
        }
        cx.notify();
    }

    /// Apply an in-progress scrollbar-thumb drag: map the cursor's vertical
    /// travel to an absolute display offset and scroll there. The track height
    /// is the viewport in pixels (`visible_rows * line_height`) — the canvas
    /// derives its row count from that same height, so no element measurement
    /// is needed. No-op when not dragging or when the offset doesn't change.
    pub(super) fn drag_scrollbar(&mut self, mouse_y: Pixels, window: &Window, cx: &mut Context<Self>) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        let history = self.snapshot.history_len;
        let visible = self.snapshot.cells.len();
        let line_height = CellMetrics::measure(&self.typography, window).line_height;
        let track_px = visible as f32 * line_height;
        let dy = f32::from(mouse_y) - drag.start_y;
        let new_offset = drag_to_offset(drag, dy, track_px, history, visible);
        let delta = new_offset as i32 - self.snapshot.display_offset as i32;
        self.scroll_viewport(delta, cx);
    }

    /// Overlay scrollbar on the right edge, present only when there's scrollback
    /// to traverse. The thumb is sized + positioned from history / viewport /
    /// offset (pure math in `terminal_scrollbar`); its mouse-down captures the
    /// drag anchor, and the root move/up handlers carry the drag.
    pub(super) fn render_scrollbar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let history = self.snapshot.history_len;
        if history == 0 {
            return None;
        }
        let (top, height) =
            thumb_geometry(history, self.snapshot.display_offset, self.snapshot.cells.len());
        let thumb_color = if self.scrollbar_drag.is_some() {
            theme.fg_muted
        } else {
            theme.border_inactive
        };
        Some(
            div()
                .id("oximux-terminal-scrollbar")
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(SCROLLBAR_WIDTH))
                .child(
                    div()
                        .id("oximux-terminal-scrollbar-thumb")
                        .absolute()
                        .top(relative(top))
                        .h(relative(height))
                        .right(px(1.0))
                        .w(px(SCROLLBAR_WIDTH - 2.0))
                        .rounded_full()
                        .bg(thumb_color)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.fg_muted))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _window, cx| {
                                // Anchor the drag and stop propagation so the
                                // grid underneath doesn't also start a selection.
                                this.scrollbar_drag = Some(ScrollbarDrag {
                                    start_y: f32::from(ev.position.y),
                                    start_offset: this.snapshot.display_offset,
                                });
                                cx.stop_propagation();
                            }),
                        ),
                ),
        )
    }

    pub(super) fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        input_trace(&format!("key_down key={}", event.keystroke.key));
        match self.search.handle_key(event) {
            SearchKeyOutcome::Pass => {}
            SearchKeyOutcome::Consumed => return,
            SearchKeyOutcome::Dismissed => {
                cx.notify();
                return;
            }
            SearchKeyOutcome::CurrentChanged => {
                self.follow_current_match();
                cx.notify();
                return;
            }
            SearchKeyOutcome::QueryChanged => {
                self.schedule_debounced_search(cx);
                cx.notify();
                return;
            }
        }

        let ks = &event.keystroke;

        // Cmd combos are app-level — `keystroke_to_bytes` already swallows
        // them. We intercept the terminal-specific ones here where the view
        // has access to `App` (clipboard) and to the backend (session-
        // specific bracketed-paste state).
        //
        // Cmd+A: select all visible cells (no PTY mode equivalent, this is
        //   a pure renderer-side affordance for "copy a chunk of output").
        // Cmd+C: copy the active selection if any; otherwise fall through
        //   to SIGINT (0x03). The fallback matches common terminal
        //   behavior when no selection is set.
        // Cmd+V: paste with bracketed-paste wrapping when the shell has
        //   DECSET 2004 on; otherwise straight paste.
        //
        // Shift is excluded so `Cmd+Shift+C` (often "copy as plain text"
        // or other variants in different terminals) doesn't silently
        // intercept the SIGINT fallback here.
        if ks.modifiers.platform
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.shift
        {
            match ks.key.as_str() {
                "v" => {
                    self.paste_from_clipboard(cx);
                    return;
                }
                "c" => {
                    // Copy the selection if any; otherwise fall through to
                    // SIGINT (^C) — the common terminal behavior with no
                    // selection set.
                    if self.copy_selection(cx) {
                        return;
                    }
                    self.send_bytes(b"\x03", cx);
                    return;
                }
                "a" => {
                    self.select_all(cx);
                    return;
                }
                _ => {}
            }
        }

        // Local scrollback keys (PageUp/Down, Cmd+Up = page up, Cmd+Down = tail).
        // Only act on the main screen: on the alt-screen or while a mouse-
        // reporting app is active there is no scrollback, so these fall through
        // to the byte encoder and the app receives the normal escape sequences
        // (e.g. PageUp in less/vim) instead of moving a buffer that isn't there.
        if let Some(cmd) = scroll_key_command(ks) {
            let mode = self.with_backend(|be| be.mouse_mode(self.session_id));
            if !mode.alt_screen && !mode.any_reporting() {
                self.handle_scroll_command(cmd, cx);
                return;
            }
        }

        // Any non-handled keystroke clears the selection so typing in the
        // shell resumes immediately. Without this, the previous Cmd+A
        // highlight would linger and the user would think the keystrokes
        // are still being captured by some selection mode.
        if self.selection.is_some() {
            self.selection = None;
            cx.notify();
        }

        // Read DECCKM (app-cursor) live so cursor keys pick CSI vs SS3; apps
        // toggle it dynamically, so fetch per keystroke rather than caching.
        let session_id = self.session_id;
        let mode = self.with_backend(|be| be.input_mode(session_id));
        // Plain printable text and any in-progress composition belong to the
        // platform input method (delivered via `TerminalInputHandler` →
        // `commit_ime_text`), so the byte encoder must not also forward them:
        // doing both double-types the character and bypasses multi-keystroke
        // composition (e.g. Vietnamese Telex `as`→`á`, `dd`→`đ`). The IME is
        // turned off on the alt-screen (full-screen TUIs), where keys must
        // reach the app raw, so the deferral is skipped there.
        let alt_screen = self.with_backend(|be| be.mouse_mode(session_id).alt_screen);
        if !alt_screen && (self.ime_marked.is_some() || is_ime_text_key(ks)) {
            return;
        }
        // When Option-as-Meta is OFF, strip the Alt modifier so the encoder
        // emits the composed platform character (e.g. `å`) instead of an
        // ESC-prefixed Meta sequence. ON (default) keeps the Meta behavior.
        let bytes = if terminal_settings(cx).option_as_meta || !ks.modifiers.alt {
            keystroke_to_bytes(ks, mode)
        } else {
            let mut stripped = ks.clone();
            stripped.modifiers.alt = false;
            keystroke_to_bytes(&stripped, mode)
        };
        self.send_bytes(&bytes, cx);
    }

    fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        // Pending-attach window: the real shell doesn't exist yet, so input
        // has nowhere meaningful to go. Drop it quietly (the window is
        // typically a few ms after first paint) instead of spamming
        // "pty write failed" per keystroke against the placeholder grid.
        if self.pending_attach {
            return;
        }
        // F3.4: paste / cmd-shortcut paths bypass the focus-in respawn
        // (no focus transition fires). Treat the keystroke itself as
        // implicit "wake this pane" — without this guard the write
        // below would surface a dormant-session error and the bytes
        // would silently drop on the floor.
        if self.is_dormant() {
            // We have no `&mut Window` in scope here; the respawn path
            // doesn't actually need it (see `respawn_if_dormant`),
            // and the dummy below keeps the call shape consistent
            // even if the helper grows window-dependent work later.
            let _ = self.dormant_cwd.is_some();
            // Force the wake through the same code path as on_focus.
            // We construct a synthetic window-less wake by inlining
            // the body without the closure / focus event.
            self.wake_dormant_inline(cx);
        }
        let session_id = self.session_id;
        // Typing snaps the viewport back to the live tail so the user sees
        // their input even if they had scrolled up into history. No-op when
        // already at the bottom.
        let _ = self.with_backend(|be| be.scroll_to_bottom(session_id));
        if let Err(err) = self.with_backend(|be| be.write(session_id, bytes)) {
            tracing::warn!(?err, "pty write failed");
            return;
        }
        input_trace(&format!("send_bytes n={} visible={}", bytes.len(), self.visible));
        // Force cursor visible on input — otherwise a blink-off tick at the
        // moment of keypress hides the cursor when the user most wants to
        // see it.
        self.cursor_visible = true;
        // Keep the render loop self-scheduling for a short window so a straggler
        // echo paints within one frame even if the run loop would otherwise doze
        // before the event-driven wake lands — this removes the latency tail.
        if self.visible {
            self.drain_frames = POST_INPUT_DRAIN_FRAMES;
        }
        cx.notify();
    }

    /// Store the IME's in-progress composition ("marked"/preedit) text so the
    /// canvas overlays it under the cursor. An empty string clears it.
    pub(super) fn set_ime_marked(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            self.clear_ime_marked(cx);
            return;
        }
        input_trace(&format!("ime_mark len={}", text.len()));
        self.ime_marked = Some(text);
        // IME composition (Vietnamese Telex / CJK) updates the preedit overlay
        // per keystroke. Like send_bytes, keep the render loop self-scheduling so
        // each composition step paints within one frame instead of stalling on a
        // dozing run loop — otherwise the composing text lags behind the keys.
        if self.visible {
            self.drain_frames = POST_INPUT_DRAIN_FRAMES;
        }
        cx.notify();
    }

    /// Drop any in-progress composition (commit, cancel, or focus loss).
    pub(super) fn clear_ime_marked(&mut self, cx: &mut Context<Self>) {
        if self.ime_marked.take().is_some() {
            if self.visible {
                self.drain_frames = POST_INPUT_DRAIN_FRAMES;
            }
            cx.notify();
        }
    }

    /// Commit finalized IME text: clear the preedit and write the composed
    /// bytes to the PTY exactly as if they had been typed.
    pub(super) fn commit_ime_text(&mut self, text: &str, cx: &mut Context<Self>) {
        input_trace(&format!("ime_commit len={}", text.len()));
        self.clear_ime_marked(cx);
        if !text.is_empty() {
            self.send_bytes(text.as_bytes(), cx);
        }
    }

    /// Insert dictated text into the PTY as if it had been typed. Unlike
    /// `paste_text` there is no bracketed-paste envelope and no ESC stripping —
    /// dictation output is plain UTF-8 with no control bytes. Wakes a dormant
    /// (restored) pane first, same as any typed input. Used by voice dictation
    /// when a terminal is the focused pane.
    pub(crate) fn insert_dictation_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.send_bytes(text.as_bytes(), cx);
        }
    }

    /// F3.4: window-less variant of `respawn_if_dormant` used from
    /// `send_bytes`. The promote-to-live + poll-task arm steps don't
    /// actually need a `&mut Window` (no focus changes, no platform
    /// integration). Kept separate so the focus path stays the same.
    fn wake_dormant_inline(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.dormant_cwd.take() else {
            return;
        };
        let mut cfg = SpawnConfig {
            // Clone so the cwd survives a failed promote (see
            // `respawn_if_dormant`): the pane stays dormant + retryable.
            cwd: cwd.clone(),
            // Re-inject the SAME context ids on the inline wake path too.
            env: self.ids.env(),
            cols: self.target_grid.0.max(DEFAULT_COLS),
            rows: self.target_grid.1.max(DEFAULT_ROWS),
            scrollback: spawn_scrollback(),
            ..SpawnConfig::default()
        };
        crate::shell::terminal::shell_integration::augment_spawn_config(&mut cfg);
        let session_id = self.session_id;
        let promote_result = self
            .backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .promote_to_live(session_id, cfg);
        if let Err(err) = promote_result {
            tracing::warn!(?err, "wake_dormant promote_to_live failed");
            // Stay dormant + retryable rather than dropping into limbo.
            self.dormant_cwd = Some(cwd);
            return;
        }
        self._poll_task = Some(Self::start_poll_task(cx));
    }

    pub(crate) fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        // Auto-wrap with bracketed-paste markers when the shell asked for them.
        self.paste_clipboard(false, cx);
    }

    /// Paste the clipboard WITHOUT bracketed-paste wrapping, even when the
    /// shell has DECSET 2004 on. Surfaced as the context menu's "Paste Text"
    /// row for programs that mis-handle the `\e[200~`/`\e[201~` envelope or
    /// when the user wants the raw bytes inserted verbatim. ESC stripping still
    /// applies (the same clipboard-injection guard as the normal paste path).
    pub(crate) fn paste_text(&mut self, cx: &mut Context<Self>) {
        self.paste_clipboard(true, cx);
    }

    /// Shared clipboard paste. `force_plain` skips the bracketed-paste wrap
    /// regardless of the shell's DECSET 2004 state (the "Paste Text" path).
    fn paste_clipboard(&mut self, force_plain: bool, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        // Security: drop every ESC byte from the clipboard payload before
        // it hits the PTY. Two attacks this defeats:
        //
        //   (a) In bracketed mode, a payload containing `\x1b[201~`
        //       prematurely closes the envelope; everything that follows
        //       runs raw (e.g. `\rrm -rf ~\r` executes both lines).
        //   (b) In non-bracketed mode, an embedded `\x1b[?2004l` would
        //       disable bracketed paste mid-stream and chain into (a).
        //
        // Stripping `\x1b` wholesale kills both vectors with one rule and
        // never leaks escape-sequence-shaped text into the shell. Cost:
        // pastes that legitimately contain ESC (e.g. captured terminal
        // recordings) lose that byte. Acceptable for v1; if a real use
        // case for raw-paste appears, add an explicit opt-in action.
        let sanitized: Vec<u8> = text.bytes().filter(|b| *b != 0x1b).collect();
        if sanitized.is_empty() {
            return;
        }

        // When the shell has DECSET 2004 on, wrap so readline/zle treat the
        // chunk as a single insertion (no per-line execution, no autocomplete
        // expansion). Plain `cat` etc. leave it off — we'd just leak the
        // escape bytes as literal text, so passthrough is correct there.
        // `force_plain` (the "Paste Text" row) always skips the wrap.
        let session_id = self.session_id;
        let wrap = !force_plain
            && self
                .with_backend(|be| be.bracketed_paste(session_id))
                .unwrap_or(false);
        let mut out = Vec::with_capacity(sanitized.len() + if wrap { 12 } else { 0 });
        if wrap {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(&sanitized);
        if wrap {
            out.extend_from_slice(b"\x1b[201~");
        }
        self.send_bytes(&out, cx);
    }

    /// Copy the active selection to the system clipboard and clear it.
    /// Returns whether there WAS a selection to consume — Cmd+C uses this to
    /// decide between copy and its SIGINT fallback (no selection → ^C), and
    /// the context menu's Copy row drives the same path. An empty-text
    /// selection still counts as consumed (no SIGINT), matching the prior
    /// inline behavior.
    pub(crate) fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(sel) = self.selection.take() else {
            return false;
        };
        // A selection whose end row runs past the viewport came from Select
        // All over the full scrollback — re-extract from the backend grid so
        // Copy yields the scrolled-off content, not just the visible rows.
        let visible_rows = self.snapshot.cells.len();
        let text = if sel.2 >= visible_rows {
            let id = self.session_id;
            let grid = self.with_backend(|be| be.search_grid(id));
            if grid.is_empty() {
                extract_selection_text(&self.snapshot, sel)
            } else {
                extract_selection_text_cells(&grid, sel)
            }
        } else {
            extract_selection_text(&self.snapshot, sel)
        };
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
        cx.notify();
        true
    }

    /// Capture text for an agent context chip: the active selection if the user
    /// has one, otherwise the last `max_lines` lines of the full scrollback.
    /// Returns `(text, truncated)` — `truncated` marks that earlier scrollback was
    /// clipped by the cap (only meaningful on the no-selection path; an explicit
    /// selection is honored verbatim). Read-only: unlike [`copy_selection`] it
    /// never writes the clipboard or clears the selection.
    pub(crate) fn capture_agent_context(&self, max_lines: usize) -> (String, bool) {
        // A live selection is an explicit "attach exactly this" — re-extract from
        // the backend grid when it runs past the viewport (the Select-All case),
        // mirroring `copy_selection`.
        if let Some(sel) = self.selection {
            let visible_rows = self.snapshot.cells.len();
            let text = if sel.2 >= visible_rows {
                let grid = self.with_backend(|be| be.search_grid(self.session_id));
                if grid.is_empty() {
                    extract_selection_text(&self.snapshot, sel)
                } else {
                    extract_selection_text_cells(&grid, sel)
                }
            } else {
                extract_selection_text(&self.snapshot, sel)
            };
            if !text.trim().is_empty() {
                return (text, false);
            }
        }
        // No selection: take the TAIL of the full scrollback, capped to
        // `max_lines`. Fall back to the visible snapshot when the backend has no
        // grid (relay attach mid-handshake).
        let grid = self.with_backend(|be| be.search_grid(self.session_id));
        let rows: &[Vec<oximux_pty::Cell>] =
            if grid.is_empty() { &self.snapshot.cells } else { &grid };
        if rows.is_empty() {
            return (String::new(), false);
        }
        let total = rows.len();
        let start = total.saturating_sub(max_lines);
        let end_col = rows
            .iter()
            .map(|r| r.len())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let tail = &rows[start..];
        let text = extract_selection_text_cells(tail, (0, 0, tail.len() - 1, end_col));
        (text, total > max_lines)
    }

    /// Select the FULL scrollback (history + visible), so a follow-up Copy
    /// yields everything the pane retains — not just the on-screen rows.
    /// Driven by Cmd+A and the context menu's Select All row. The selection
    /// coords live in the full-grid space; [`copy_selection`](Self::copy_selection)
    /// detects an end row past the viewport and re-extracts from the backend
    /// grid. Grid-less / empty backends fall back to the visible extent.
    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        let id = self.session_id;
        let grid = self.with_backend(|be| be.search_grid(id));
        let (end_row, end_col) = if grid.is_empty() {
            let rows = self.snapshot.cells.len();
            if rows == 0 {
                return;
            }
            let ec = self
                .snapshot
                .cells
                .iter()
                .map(|r| r.len())
                .max()
                .unwrap_or(0)
                .saturating_sub(1);
            (rows - 1, ec)
        } else {
            let ec = grid
                .iter()
                .map(|r| r.len())
                .max()
                .unwrap_or(0)
                .saturating_sub(1);
            (grid.len() - 1, ec)
        };
        self.selection = Some((0, 0, end_row, end_col));
        cx.notify();
    }

    /// Set the selection to the word under `(row, col)`. Used by the terminal
    /// grid right-click so Copy / Send-to-Agent act on a token even when the
    /// user hasn't dragged a selection. Blank / whitespace cells are skipped —
    /// there's no meaningful word to grab there.
    pub(super) fn select_word_at(&mut self, row: usize, col: usize) {
        let ch = self
            .snapshot
            .cells
            .get(row)
            .and_then(|r| r.get(col))
            .map(|c| c.ch)
            .unwrap_or('\0');
        if ch == '\0' || ch.is_whitespace() {
            return;
        }
        let ((sr, sc), (er, ec)) = self.word_span((row, col));
        self.selection = Some((sr, sc, er, ec));
    }

    /// Clear the visible grid AND scrollback (the standard terminal "Clear"
    /// affordance), drop any stale selection/hover, and repaint. The shell's
    /// prompt redraws on the next keystroke, so the terminal stays usable.
    pub(crate) fn clear_terminal(&mut self, cx: &mut Context<Self>) {
        let id = self.session_id;
        if let Err(err) = self.with_backend(|be| be.clear(id)) {
            tracing::warn!(?err, "terminal clear failed");
            return;
        }
        self.selection = None;
        self.hovered_link = None;
        self.scroll_px = 0.0;
        if let Ok(snapshot) = self.with_backend(|be| be.snapshot(id)) {
            self.snapshot = Arc::new(snapshot);
        }
        // If search is open, re-scan the now-empty grid so stale match
        // highlights don't linger over the cleared terminal.
        if self.search.active {
            self.rerun_search(cx);
        }
        cx.notify();
    }

    /// Open the scrollback search overlay. Shared by the `Search` action and
    /// the context menu's Search row.
    pub(crate) fn open_search(&mut self, cx: &mut Context<Self>) {
        self.search.open();
        self.rerun_search(cx);
        cx.notify();
    }

    /// Send the active selection to the active agent (context menu row).
    /// Mirrors the Cmd+Shift+I action handler.
    pub(crate) fn send_selection_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selection else {
            return;
        };
        let text = extract_selection_text(&self.snapshot, sel);
        if text.is_empty() {
            return;
        }
        window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
    }

    /// Send the last completed command's output to the active agent (context
    /// menu row). Mirrors the Cmd+Shift+O action handler.
    pub(crate) fn send_last_output_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.last_completed_command_output() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
    }

    /// Resolve the link the right-click captured (URL or `path:line:col`) and
    /// open it: URLs via the system handler, paths in the in-app editor —
    /// the same destinations as a Cmd-click. No-op when the token doesn't
    /// classify as a link.
    pub(crate) fn open_link_string(&mut self, s: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = crate::shell::terminal_links::classify_link(s) {
            self.open_link(target, window, cx);
        }
    }
}
