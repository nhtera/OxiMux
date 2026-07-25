use super::*;

impl TerminalView {
    pub(super) fn tick(&mut self, cx: &mut Context<Self>) {
        // Use the per-session drain so panes can't steal each other's
        // events from a shared backend (e.g., the relay). The global
        // `drain_events` is reserved for tests + cleanup paths.
        let session_id_for_drain = self.session_id;
        let events = self.with_backend(|be| be.drain_events_for(session_id_for_drain));
        if events.is_empty() {
            return;
        }
        let settings = terminal_settings(cx);
        // Resnapshot on `Output` (new bytes landed in the grid) AND `Resize`
        // (Term::resize reflowed existing rows + may have shrunk row count).
        // Skipping `Resize` here was the cause of the post-split clipping
        // regression: the cached snapshot kept its pre-resize dimensions
        // and overflowed the narrower pane until the shell echoed again.
        // `Exit` still falls through — no grid mutation, no resnap needed,
        // and avoids pinning the cursor visible on a dead session.
        let mut needs_snapshot = false;
        let mut had_output = false;
        let mut exit_changed = false;
        let mut got_bell = false;
        let mut bell_rang = false;
        let mut latest_title: Option<String> = None;
        let mut clipboard_text: Option<String> = None;
        let mut pty_replies: Vec<Vec<u8>> = Vec::new();
        for ev in &events {
            match ev {
                TerminalEvent::Output { bytes, .. } => {
                    needs_snapshot = true;
                    had_output = true;
                    // Decode any OSC-9999 status sideband the global hooks
                    // emitted onto this terminal's stream. Cheap for a plain
                    // shell (skipped unless a marker is present); gives a
                    // hand-typed agent the same hook-driven status as a spawned
                    // one. The relay also leaves these private-OSC bytes for the
                    // emulator, which ignores them — so nothing is displayed.
                    self.agent_scan.feed(bytes, std::time::Instant::now());
                }
                // The child process died. Record the code so render shows a
                // "process exited" banner; without it a dead leader (e.g. a
                // program run with `exec`, with no shell to fall back to) just
                // freezes the final frame and is indistinguishable from a hang.
                // A clean exit (status 0) ALSO emits `CleanExit` so a lone-view
                // tab auto-closes (the group decides); a non-zero/signalled exit
                // keeps the banner. `None` (signal/detach) maps to the `-1`
                // sentinel — a real Unix status is 0..=255, so it can't collide
                // and never reads as clean.
                TerminalEvent::Exit { code, .. } => {
                    let code = code.unwrap_or(-1);
                    self.exited = Some(code);
                    exit_changed = true;
                    if code == 0 {
                        cx.emit(TerminalViewEvent::CleanExit {
                            session_id: self.session_id,
                        });
                    }
                }
                TerminalEvent::Resize { .. } => needs_snapshot = true,
                TerminalEvent::TitleChange { title, .. } => {
                    latest_title = Some(title.clone());
                }
                TerminalEvent::Bell { .. } => {
                    bell_rang = true;
                    // A BEL while this pane is NOT focused raises attention
                    // (unless the bell is disabled). A bell in the pane
                    // you're already looking at is just noise.
                    if !self.focused && settings.bell != BellStyle::Off {
                        got_bell = true;
                    }
                }
                // OSC 52: the child asked to set the system clipboard. Keep the
                // last in the batch; written once below.
                TerminalEvent::Clipboard { text, .. } => clipboard_text = Some(text.clone()),
                // Device/color query replies (DSR, DA, OSC 11) — write back to
                // the PTY after the loop so probing tools don't stall.
                TerminalEvent::PtyReply { bytes, .. } => pty_replies.push(bytes.clone()),
                // Shell-integration command marks drive the prompt gutter badge.
                TerminalEvent::CommandMark {
                    kind, exit, line, ..
                } => self.apply_command_mark(*kind, *exit, *line),
                // OSC 9;4 progress. state 0 clears; error/warning raises
                // attention on an unfocused pane like a bell.
                TerminalEvent::Progress { state, value, .. } => {
                    self.progress = if *state == 0 {
                        None
                    } else {
                        Some((*state, *value))
                    };
                    if matches!(*state, 2 | 4) && !self.focused {
                        got_bell = true;
                    }
                }
                _ => {}
            }
        }
        if got_bell {
            self.attention = true;
        }
        // Notify routes the bell to the OS pipeline on top of the visual
        // attention. The dispatcher owns the policy gates (master/source
        // enables, visible-pane suppression, focus gate, burst collapse);
        // this end only rate-limits BEL storms per pane. Deliberately NOT
        // gated on `self.focused`: a focused pane in a backgrounded window
        // is the "ran a command, switched apps, BEL on completion" case the
        // Notify setting exists for, and a frontmost window is already
        // silenced by the dispatcher's visible-pane rule.
        if bell_rang && settings.bell == BellStyle::Notify {
            self.maybe_notify_bell(cx);
        }
        let session_id = self.session_id;
        if needs_snapshot && let Ok(snapshot) = self.with_backend(|be| be.snapshot(session_id)) {
            self.snapshot = Arc::new(snapshot);
            self.revalidate_hover();
        }
        if had_output {
            self.cursor_visible = true;
            // Persist the ambient-agent reading (keyed by this pane's PTY id)
            // whenever it changes, so a warm re-attach after a quit re-seeds it
            // and the rail lists the still-running agent immediately. Written
            // only on change → no SQLite churn on a steady output stream; a
            // plain shell never produces a reading, so it never writes.
            let reading = self.agent_scan.current(std::time::Instant::now());
            if reading != self.last_persisted_ambient {
                if let Some(sb) = &reading
                    && let Some(pty) = self.external_id()
                {
                    let (status, detail) = (sb.status.clone(), sb.detail.clone());
                    cx.background_executor()
                        .spawn(async move {
                            crate::shell::ambient_state::persist(&pty, &status, &detail);
                        })
                        .detach();
                }
                self.last_persisted_ambient = reading;
            }
        }
        if let Some(title) = latest_title {
            self.title = Some(title);
        }
        if let Some(text) = clipboard_text
            && settings.osc52_clipboard
        {
            // SECURITY: OSC 52 lets terminal OUTPUT set the system clipboard.
            // For remote/relay panes that means a remote process can silently
            // overwrite your clipboard (injection surface on the next paste).
            // The `osc52_clipboard` setting is the allow-list gate; there is no
            // separate remote-vs-local distinction yet.
            //
            // FIXME(osc52-remote-local): gate remote/SSH-backed sessions
            // separately from local ones so a remote process can't write the
            // clipboard even when local OSC 52 is allowed. Deferred — low
            // likelihood, lowest-priority research item; tracked here so the
            // split isn't lost.
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
        for bytes in pty_replies {
            if let Err(err) = self.with_backend(|be| be.write(session_id, &bytes)) {
                tracing::warn!(?err, "pty reply write failed");
            }
        }
        // Repaint when visible. A hidden tab skips the repaint for plain output
        // (its snapshot is already updated above, so it's current the instant
        // it's shown) but still repaints on an attention edge (bell / error
        // progress) so a background tab's chip can light up.
        if had_output {
            input_trace(&format!(
                "echo_render had_output visible={} events={}",
                self.visible,
                events.len()
            ));
        }
        if self.visible || got_bell || exit_changed {
            cx.notify();
        }
    }

    /// Forward a bell to the pane group's notification dispatch, at most
    /// once per debounce window. The group contributes the context this
    /// view can't see (tab label, workspace key, window-active flag).
    fn maybe_notify_bell(&mut self, cx: &mut Context<Self>) {
        const BELL_BANNER_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);
        let now = std::time::Instant::now();
        if self
            .last_bell_banner
            .is_some_and(|t| now.duration_since(t) < BELL_BANNER_DEBOUNCE)
        {
            return;
        }
        self.last_bell_banner = Some(now);
        let session = self.session_id;
        let pane_visible = self.is_visible();
        if let Some(opener) = self.opener.clone() {
            let _ = opener.update(cx, |pg, cx| {
                pg.notify_terminal_bell(session, pane_visible, cx);
            });
        }
    }

    /// Fold a shell-integration command mark into the gutter-badge list. A
    /// prompt-start opens a new mark at its anchor line; a command-end attaches
    /// the exit code to the most recent open mark. Intermediate phases
    /// (B/C / output-start) carry no badge of their own.
    fn apply_command_mark(&mut self, kind: CommandMarkKind, exit: Option<i32>, line: u64) {
        match kind {
            CommandMarkKind::PromptStart => {
                self.command_marks.push(CommandMark { line, exit: None });
                if self.command_marks.len() > MAX_COMMAND_MARKS {
                    let overflow = self.command_marks.len() - MAX_COMMAND_MARKS;
                    self.command_marks.drain(0..overflow);
                }
            }
            CommandMarkKind::CommandEnd => {
                if let Some(last) = self.command_marks.last_mut() {
                    last.exit = exit;
                }
            }
            CommandMarkKind::CommandStart | CommandMarkKind::OutputStart => {}
        }
    }

    /// Command-mark badges for the rows currently visible: `(screen_row,
    /// is_error)`. Maps each mark's absolute history line through the live
    /// snapshot's `history_len`/`display_offset`, dropping marks scrolled out
    /// of view. Only finished commands (a known exit code) get a badge.
    pub(super) fn visible_command_badges(&self) -> Vec<(usize, bool)> {
        let rows = self.snapshot.rows as i64;
        if rows == 0 {
            return Vec::new();
        }
        let base = self.snapshot.history_len as i64 - self.snapshot.display_offset as i64;
        let mut out = Vec::new();
        for mark in &self.command_marks {
            let Some(exit) = mark.exit else { continue };
            let screen_row = mark.line as i64 - base;
            if (0..rows).contains(&screen_row) {
                out.push((screen_row as usize, exit != 0));
            }
        }
        out
    }

    /// Latest OSC 2 title the shell emitted, if any. Exposed for future use
    /// by the workspace tab strip.
    /// The PTY session this view renders. Bell-banner routing matches on
    /// it to find the owning tab.
    pub fn session_id(&self) -> TerminalSessionId {
        self.session_id
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Hook-derived agent status for this terminal, decoded from the OSC-9999
    /// sideband, or `None` for a plain shell / an agent that has not yet emitted
    /// a hook. Richer and more stable than the title heuristic; the ambient
    /// aggregation prefers it when present.
    pub fn ambient_agent(
        &self,
        now: std::time::Instant,
    ) -> Option<crate::shell::ambient_agent_scan::AmbientSideband> {
        self.agent_scan.current(now)
    }

    /// On a warm re-attach, re-prime the ambient scan from the persisted reading
    /// for this pane's surviving PTY id. The hook sideband is never stored in
    /// the byte ring, so a still-running agent would otherwise vanish from the
    /// rail until its next hook fires (an agent idle at its prompt fires none).
    /// No-op for a plain terminal (nothing persisted) or a stale reading (the
    /// store enforces the freshness TTL). Call only after the live session is
    /// adopted so `external_id()` resolves to the re-attached PTY.
    pub fn seed_ambient_from_persisted(&mut self) {
        let Some(pty) = self.external_id() else {
            return;
        };
        if let Some((status, detail)) = crate::shell::ambient_state::load(&pty) {
            self.agent_scan
                .seed(status, detail, std::time::Instant::now());
            self.last_persisted_ambient = self.agent_scan.current(std::time::Instant::now());
            tracing::debug!(pty_id = %pty, "seeded ambient agent from persisted reading");
        }
    }

    /// Latest OSC 9;4 progress `(state, value)` the child reported, if any.
    /// `state`: 1 set, 2 error, 3 indeterminate, 4 warning; `value` is 0..=100.
    /// Exposed for a future progress affordance on the tab strip.
    pub fn progress(&self) -> Option<(u8, u8)> {
        self.progress
    }

    /// True when this pane has a pending attention signal (an unfocused-pane
    /// BEL today). Read by the tab strip so a bell in a BACKGROUND tab lights
    /// that tab — the pane ring alone is invisible when the pane isn't shown.
    /// Cleared when the pane gains focus.
    pub fn attention(&self) -> bool {
        self.attention
    }

    /// Raise the attention ring for an agent lifecycle edge (NeedsApproval /
    /// WaitingForInput), the same channel a background BEL uses. Only fires on
    /// an unfocused pane — a focused pane is already in view, so a ring would
    /// be noise. Cleared on the next `on_focus`. Called by the per-tab agent
    /// status watcher (`agent_status_task`) on a genuine status edge.
    pub fn raise_agent_attention(&mut self, cx: &mut Context<Self>) {
        if self.focused {
            return;
        }
        self.attention = true;
        // Notify unconditionally — deliberately NOT gated on `visible` (unlike
        // `tick`'s plain-output repaint). A background tab's attention edge
        // MUST reach the group so its chip can light up even while hidden.
        cx.notify();
    }

    pub(super) fn maybe_resize(&mut self) {
        if self.target_grid == self.last_resize {
            return;
        }
        let session_id = self.session_id;
        let (cols, rows) = self.target_grid;
        if let Err(err) = self.with_backend(|be| be.resize(session_id, cols, rows)) {
            tracing::warn!(?err, "pty resize failed");
            return;
        }
        self.last_resize = self.target_grid;
        // Pull a fresh snapshot immediately. Without this, the next paint
        // still uses the pre-resize grid (old cols/rows) inside the new
        // pane bounds — wide rows overflow + clip, and reflow that
        // `Term::resize` already performed isn't visible until the shell
        // next emits output. The render that triggered `maybe_resize`
        // proceeds with up-to-date cell data this same frame.
        if let Ok(snapshot) = self.with_backend(|be| be.snapshot(session_id)) {
            self.snapshot = Arc::new(snapshot);
            self.revalidate_hover();
        }
    }

    /// Pull the canvas-derived grid size into `target_grid` so the next
    /// `maybe_resize` applies it. Called at the top of `render`. The
    /// canvas paint closure (which runs in the paint phase, after render)
    /// is what WRITES `canvas_grid` from the real painted bounds and
    /// schedules a repaint via `window.refresh()` when it changes — so by
    /// the time this read runs on the following frame, `canvas_grid`
    /// holds the size that exactly matches the cells we paint. This is
    /// the single source of truth for grid size; it replaced the old
    /// `viewport − hardcoded_chrome` estimate that drifted and made
    /// full-screen TUIs render their absolute-positioned UI scrambled.
    pub(super) fn pull_canvas_grid(&mut self) {
        self.target_grid = self.canvas_grid.get();
    }
}
