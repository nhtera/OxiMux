use super::*;

impl TerminalView {
    /// Stable surface (leaf) id — read by the persistence layer to round-
    /// trip `OXIMUX_SURFACE_ID` across restarts.
    pub fn surface_id(&self) -> &str {
        &self.ids.surface_id
    }

    /// Stable terminal id — read by the persistence layer to round-trip
    /// `OXIMUX_TAB_ID` across restarts.
    pub fn tab_id(&self) -> &str {
        &self.ids.tab_id
    }

    /// Wire the host pane group so Cmd-click on a `path:line:col` link can
    /// open it in an editor tab. Called by `PaneGroup` right after `mount`.
    pub fn set_opener(&mut self, opener: WeakEntity<PaneGroup>) {
        self.opener = Some(opener);
    }
    /// Build a view around an already-spawned backend + session, grabbing
    /// focus so the user can type immediately. Use for interactive spawns
    /// (new tab, live split). Restore paths use [`mount_background`] instead
    /// so re-creating many panes doesn't ping-pong focus before the restore
    /// orchestrator focuses the active one.
    #[allow(clippy::too_many_arguments)]
    pub fn mount(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        ids: SurfaceIds,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::mount_inner(
            backend, session_id, ids, theme, density, typography, true, window, cx,
        )
    }

    /// Like [`mount`](Self::mount) but does NOT grab focus on construction.
    /// Restore builds every split leaf with this so N panes don't each fire
    /// a focus transition; the restore orchestrator (`focus_active`) sets the
    /// final focus once.
    #[allow(clippy::too_many_arguments)]
    pub fn mount_background(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        ids: SurfaceIds,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::mount_inner(
            backend, session_id, ids, theme, density, typography, false, window, cx,
        )
    }

    /// The spawn is done outside `cx.new` because the entity builder closure
    /// is infallible; this keeps spawn errors at the caller where they can be
    /// logged + fall back to a placeholder. `grab_focus` gates the initial
    /// `focus()` so restore can build panes without stealing focus.
    #[allow(clippy::too_many_arguments)]
    fn mount_inner(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        ids: SurfaceIds,
        theme: Theme,
        density: Density,
        typography: Typography,
        grab_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = Arc::new(
            backend
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot(session_id)
                .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS)),
        );

        let focus_handle = cx.focus_handle();
        // Register on_focus / on_blur BEFORE calling focus() so the initial
        // focus transition fires the callback. Without this ordering the
        // struct had to init `focused: true` as a workaround, which left
        // every pane reporting "I'm focused" — when two panes were created
        // during workspace restore, MainPane's observer saw both as focused
        // and ping-ponged `self.focused` between them at the cursor-blink
        // cadence. `on_focus` also resets `cursor_visible` so a pane gaining
        // focus never waits a full 530 ms for the cursor to reappear.
        cx.on_focus(&focus_handle, window, |view, _, cx| {
            view.focused = true;
            view.cursor_visible = true;
            // Focusing the pane means the user is now looking — clear any
            // pending attention ring.
            view.attention = false;
            cx.notify();
        })
        .detach();
        cx.on_blur(&focus_handle, window, |view, _, cx| {
            view.focused = false;
            cx.notify();
        })
        .detach();
        // Grab focus on mount so the user can type into the shell immediately
        // without first clicking. Without this the window opens with no focus
        // owner and keystrokes are dropped until the first click. Restore
        // skips this (`grab_focus == false`) so building many split leaves
        // doesn't ping-pong focus before `focus_active` sets it once.
        if grab_focus {
            focus_handle.focus(window, cx);
        }

        let poll_task = Self::start_poll_task(cx);
        let blink_task = Self::start_blink_task(cx);
        let output_drain_task = Self::start_output_drain_task(&backend, session_id, cx);

        Self {
            backend,
            session_id,
            ids,
            snapshot,
            theme,
            density,
            typography,
            focus_handle,
            target_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            last_resize: (DEFAULT_COLS, DEFAULT_ROWS),
            cursor_visible: true,
            scroll_px: 0.0,
            scrollbar_drag: None,
            // Init to false; the on_focus callback fires for the focus() above
            // and flips this true for whichever pane actually wins focus.
            // Multiple panes constructed in the same effect run will each see
            // their focus() call land, last one wins, on_blur clears the rest.
            focused: false,
            visible: true,
            attention: false,
            search: SearchState::new(),
            search_debounce_gen: 0,
            title: None,
            dormant_cwd: None,
            _poll_task: Some(poll_task),
            _blink_task: blink_task,
            _output_drain_task: output_drain_task,
            drain_frames: 0,
            canvas_grid: Rc::new(Cell::new((DEFAULT_COLS, DEFAULT_ROWS))),
            canvas_bounds: Rc::new(Cell::new(Bounds::default())),
            selection: None,
            ime_marked: None,
            selecting: None,
            opener: None,
            hovered_link: None,
            last_bell_banner: None,
            link_exists: ExistenceCache::new(),
            command_marks: Vec::new(),
            progress: None,
            pending_attach: false,
            pending_relay_hint: None,
            exited: None,
            agent_scan: crate::shell::ambient_agent_scan::AmbientAgentScan::new(),
            last_persisted_ambient: None,
        }
    }

    /// Paint-first restore placeholder: a view over an in-process dormant
    /// grid (from [`spawn_pending_placeholder_grid`]) that issued ZERO
    /// daemon round-trips at construction. It renders an empty terminal
    /// surface — the same blank canvas a freshly-spawned shell shows for
    /// its first frames — until the post-paint reconcile delivers the real
    /// session via [`adopt_live_session`](Self::adopt_live_session).
    ///
    /// Unlike [`mount_dormant`](Self::mount_dormant), focus-in/keystrokes
    /// do NOT wake anything here (`dormant_cwd` stays `None`): waking the
    /// placeholder would spawn an in-process shell racing the reconcile's
    /// relay attach. Input during the pending window is dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn mount_pending(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        ids: SurfaceIds,
        relay_hint: Option<String>,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::mount_inner(
            backend, session_id, ids, theme, density, typography, false, window, cx,
        );
        view.pending_attach = true;
        view.pending_relay_hint = relay_hint;
        // No live session to drain yet — `adopt_live_session` arms the poll
        // task once the real session arrives. Dropping the just-armed task
        // cancels it before its first 16 ms tick fires.
        view._poll_task = None;
        // No live session yet — the event-driven drain is armed on the live
        // swap in `adopt_live_session`.
        view._output_drain_task = None;
        view
    }

    /// True while the view awaits its post-paint session delivery.
    pub fn is_pending_attach(&self) -> bool {
        self.pending_attach
    }

    /// Swap the pending placeholder grid for the live session the
    /// reconcile pass attached/spawned: re-snapshot, force the next
    /// render's `maybe_resize` to push the already-painted pane size to
    /// the new session, and arm the drain task. Returns `false` (and
    /// does nothing) when the view is not pending — the caller must then
    /// close the delivered session itself or it leaks.
    pub fn adopt_live_session(
        &mut self,
        backend: SharedBackend,
        session_id: TerminalSessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.pending_attach {
            return false;
        }
        self.pending_attach = false;
        self.pending_relay_hint = None;
        // A fresh live session is replacing the placeholder — drop any stale
        // exit banner so a swap (e.g. post-attach reconcile) never leaves the
        // "process exited" marker over a now-running shell.
        self.exited = None;
        let old_backend = std::mem::replace(&mut self.backend, backend);
        let old_id = std::mem::replace(&mut self.session_id, session_id);
        // The placeholder owns no child process, but close it anyway so the
        // in-process backend's session map doesn't accumulate dead entries.
        // Detached thread mirrors `Drop` — close may briefly block.
        std::thread::spawn(move || {
            if let Ok(mut be) = old_backend.lock() {
                let _ = be.close(old_id);
            }
        });
        self.snapshot = Arc::new(
            self.backend
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot(session_id)
                .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS)),
        );
        // The session was attached/spawned at its own size; (0,0) can never
        // equal a real canvas grid, so the next render always resizes it to
        // the painted bounds.
        self.last_resize = (0, 0);
        self._poll_task = Some(Self::start_poll_task(cx));
        // Arm the event-driven drain on the now-live (relay) session so echo
        // renders on arrival — the responsive path for a reattached terminal.
        self._output_drain_task = Self::start_output_drain_task(&self.backend, session_id, cx);
        cx.notify();
        true
    }

    /// F3.4: build a view backed by a DORMANT session — grid emulator
    /// populated by `prefill_grid` with restored scrollback, but no
    /// shell child running. The view renders the snapshot identically
    /// to a live view; the first user interaction (focus-in or
    /// keystroke) calls `respawn` to spawn a shell at `cwd` and arm the
    /// PTY-drain task.
    ///
    /// `backend` + `session_id` come from `spawn_local_pty_dormant` —
    /// the caller checks that result before invoking `cx.new`. Keeping
    /// the spawn outside this constructor lets `cx.new` stay
    /// infallible (its builder closure isn't allowed to fail).
    #[allow(clippy::too_many_arguments)]
    pub fn mount_dormant(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        ids: SurfaceIds,
        cwd: PathBuf,
        prefill_bytes: &[u8],
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Apply the saved scrollback to the dormant grid emulator.
        if !prefill_bytes.is_empty() {
            let mut guard = backend.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(err) = guard.prefill_grid(session_id, prefill_bytes) {
                tracing::warn!(?err, "dormant prefill_grid failed");
            }
        }
        let snapshot = Arc::new(
            backend
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot(session_id)
                .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS)),
        );

        let focus_handle = cx.focus_handle();
        // On focus-in: flip the live-cursor state, then wake the PTY.
        // The respawn is a no-op when the view is already live (e.g.
        // user clicks back into a sub-pane they already woke), so it's
        // safe to fire unconditionally.
        cx.on_focus(&focus_handle, window, |view, window, cx| {
            view.focused = true;
            view.cursor_visible = true;
            view.attention = false;
            view.respawn_if_dormant(window, cx);
            cx.notify();
        })
        .detach();
        cx.on_blur(&focus_handle, window, |view, _, cx| {
            view.focused = false;
            cx.notify();
        })
        .detach();
        // NOTE: NO `focus_handle.focus(window, cx)` here. The active
        // sub-pane is focused explicitly by the restore orchestrator
        // (PaneGroup::focus_active). Dormant non-active sub-panes stay
        // unfocused — they wake when the user clicks/types into them.

        let blink_task = Self::start_blink_task(cx);

        Self {
            backend,
            session_id,
            ids,
            snapshot,
            theme,
            density,
            typography,
            focus_handle,
            target_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            last_resize: (DEFAULT_COLS, DEFAULT_ROWS),
            cursor_visible: true,
            scroll_px: 0.0,
            scrollbar_drag: None,
            focused: false,
            visible: true,
            attention: false,
            search: SearchState::new(),
            search_debounce_gen: 0,
            title: None,
            dormant_cwd: Some(cwd),
            _poll_task: None,
            _blink_task: blink_task,
            // Local dormant placeholder doesn't signal output; the poll drains
            // it once woken. Event-driven drain is armed on the live swap.
            _output_drain_task: None,
            drain_frames: 0,
            canvas_grid: Rc::new(Cell::new((DEFAULT_COLS, DEFAULT_ROWS))),
            canvas_bounds: Rc::new(Cell::new(Bounds::default())),
            selection: None,
            ime_marked: None,
            selecting: None,
            opener: None,
            hovered_link: None,
            last_bell_banner: None,
            link_exists: ExistenceCache::new(),
            command_marks: Vec::new(),
            progress: None,
            pending_attach: false,
            pending_relay_hint: None,
            exited: None,
            agent_scan: crate::shell::ambient_agent_scan::AmbientAgentScan::new(),
            last_persisted_ambient: None,
        }
    }

    /// True when this view is in the F3.4 dormant state (grid populated
    /// from snapshot, no PTY child yet). Public so the render layer can
    /// optionally surface an indicator badge.
    pub fn is_dormant(&self) -> bool {
        self.dormant_cwd.is_some()
    }

    /// F3.4: promote the dormant PTY session to live. Spawns a shell
    /// child at the saved cwd, wires it to the existing grid emulator
    /// (preserving prefilled scrollback), and arms the PTY-drain task.
    /// No-op when the view is already live.
    pub fn respawn_if_dormant(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(cwd) = self.dormant_cwd.take() else {
            return;
        };
        let cfg = SpawnConfig {
            cwd,
            // Re-inject the SAME context ids so a respawned shell keeps
            // its OXIMUX_SURFACE_ID / TAB_ID across the dormant cycle.
            env: self.ids.env(),
            cols: self.target_grid.0.max(DEFAULT_COLS),
            rows: self.target_grid.1.max(DEFAULT_ROWS),
            scrollback: spawn_scrollback(),
            ..SpawnConfig::default()
        };
        let session_id = self.session_id;
        let promote_result = self
            .backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .promote_to_live(session_id, cfg);
        if let Err(err) = promote_result {
            tracing::warn!(?err, "respawn promote_to_live failed; staying dormant");
            // Leaving dormant_cwd unset; user will see no shell. They
            // can still scroll through the prefilled grid.
            return;
        }
        // The shell is alive again at the dormant cwd — clear any exit marker.
        self.exited = None;
        self._poll_task = Some(Self::start_poll_task(cx));
        cx.notify();
    }

    /// 16 ms PTY-drain timer. Drains the event channel + re-snapshots + one
    /// `cx.notify()` per non-empty tick. Single notify per tick gives the
    /// "max 1 invalidation per frame" coalesce the perf plan asks for.
    pub(super) fn start_poll_task(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                // Read the cadence live each iteration: a hidden tab drains on
                // the slow interval, a visible one at ~60fps. An edit to
                // visibility takes effect on the next wake.
                let Ok((executor, interval)) = this.read_with(cx, |view, cx| {
                    (cx.background_executor().clone(), poll_interval_ms(view.visible))
                }) else {
                    return;
                };
                executor.timer(Duration::from_millis(interval)).await;
                if this.update(cx, |view, cx| view.tick(cx)).is_err() {
                    return;
                }
            }
        })
    }

    /// Event-driven drain: register a waker the backend fires the instant it
    /// enqueues output, and `tick` on that signal. This is the responsive path
    /// — it renders the echo ~one frame after the bytes arrive, where the timer
    /// poll stalls 100ms–1s because macOS throttles background-executor timers
    /// when the run loop idles between keystrokes. Returns `None` for backends
    /// that don't signal (the in-process fallback / dormant placeholder): there
    /// the registered waker is dropped immediately, the channel closes, and the
    /// task exits — the poll keeps draining those.
    fn start_output_drain_task(
        backend: &SharedBackend,
        session_id: TerminalSessionId,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        use futures::StreamExt as _;
        // Capacity 1 + the single retained sender coalesces a burst of arrivals
        // into one wake; one drain consumes the whole queued batch.
        let (tx, mut rx) = futures::channel::mpsc::channel::<()>(1);
        let tx = std::sync::Mutex::new(tx);
        let waker: oximux_pty::OutputWaker = std::sync::Arc::new(move || {
            if let Ok(mut tx) = tx.lock() {
                // Full (a wake is already pending) or closed (view gone) → drop.
                let _ = tx.try_send(());
            }
        });
        backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_output_waker(session_id, waker);
        Some(cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                if this.update(cx, |view, cx| view.tick(cx)).is_err() {
                    return;
                }
            }
        }))
    }

    /// Cursor blink. Independent of the PTY poll so a chatty TUI doesn't bury
    /// the toggle and an idle shell still pulses. Period + on/off come from
    /// `TerminalSettings` live (next tick picks up an edit). When blink is off
    /// the cursor is pinned visible. The toggle always runs (state stays
    /// truthful across focus changes); `cx.notify()` is gated on `view.focused`
    /// so unfocused panes contribute zero repaints per second.
    fn start_blink_task(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok((executor, interval)) = this.read_with(cx, |_, cx| {
                    (
                        cx.background_executor().clone(),
                        terminal_settings(cx).blink_interval_ms,
                    )
                }) else {
                    return;
                };
                executor.timer(Duration::from_millis(interval)).await;
                if this
                    .update(cx, |view, cx| {
                        let blink = terminal_settings(cx).cursor_blink;
                        if blink {
                            view.cursor_visible = !view.cursor_visible;
                        } else if !view.cursor_visible {
                            // Blink turned off mid-cycle on the hidden phase —
                            // restore a steady cursor.
                            view.cursor_visible = true;
                        }
                        if view.focused {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    /// Lock the shared backend briefly and run `f` against the trait. The
    /// lock is held for one non-blocking call only — never await across,
    /// never sleep within. Returns whatever `f` returns.
    pub(super) fn with_backend<R>(&self, f: impl FnOnce(&mut dyn TerminalBackend) -> R) -> R {
        let mut be = self.backend.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut **be)
    }

    /// Serialize this pane's grid + scrollback to ANSI bytes for the
    /// persistence path (Phase 4 step 16). Caps output at `max_bytes` via
    /// the backend's binary-search wrapper. Empty Vec when the backend
    /// can't serialize (fixture / replay backends).
    pub fn serialize_buffer(&self, max_bytes: usize) -> Vec<u8> {
        let id = self.session_id;
        self.with_backend(|be| be.serialize_buffer(id, max_bytes))
    }

    /// Whether this view's focus handle currently holds platform focus.
    /// Public so `MainPane`'s observer can mirror per-pane focus state
    /// into `self.focused` without a `&Window` (the field is kept up to
    /// date by the `cx.on_focus` / `cx.on_blur` observers in `mount`).
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Push the on-screen state down from the owning `PaneGroup`. Becoming
    /// visible repaints immediately so the just-shown tab reflects any output
    /// that landed while it was throttled; the poll loop picks up the faster
    /// cadence on its next iteration.
    /// Whether this pane is currently the on-screen tab of a rendered
    /// group. Hidden tabs and inactive projects' panes report false; the
    /// notification dispatcher uses it for visible-pane suppression.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            cx.notify();
        }
    }

    /// Backend's external identifier for this pane's session (e.g.,
    /// the relay daemon's PTY id), if any. Used by the phase-06
    /// reconciliation capture path. `None` for in-process backends —
    /// including the pending-attach placeholder, which deliberately
    /// reports `None` here so tear-off (and anything else gating on a
    /// live relay id) stays disabled until the real session arrives.
    pub fn external_id(&self) -> Option<String> {
        let id = self.session_id;
        self.with_backend(|be| be.external_id_of(id))
    }

    /// Whether this pane's program currently has DECSET-2004 (bracketed paste)
    /// enabled. The composer queries it so a drafted prompt is wrapped only
    /// when the program will honor the brackets — the same gate the paste
    /// handler uses. `false` on a missing/pending backend.
    pub fn bracketed_paste_active(&self) -> bool {
        let id = self.session_id;
        self.with_backend(|be| be.bracketed_paste(id)).unwrap_or(false)
    }

    /// Relay id to PERSIST for this pane. Same as [`external_id`]
    /// (Self::external_id) for live views, but a still-pending view
    /// answers with its persisted hint — so a quit-save racing the
    /// post-paint reconcile re-persists the original row instead of
    /// dropping it (which would orphan the daemon PTY: alive, but no
    /// row points at it on the next boot).
    pub fn relay_id_for_capture(&self) -> Option<String> {
        // An exited session is dead in the daemon — persisting its relay id
        // would cold-restore a frozen, input-less pane on the next launch. Drop
        // the reattach hint so a dead session is never revived as a corpse;
        // lone clean-exit tabs are already auto-closed before capture, and any
        // other exited leaf respawns fresh instead of restoring dead.
        if self.exited.is_some() {
            return None;
        }
        if self.pending_attach {
            return self.pending_relay_hint.clone();
        }
        self.external_id()
    }

    /// Detach the relay session so the daemon PTY survives without an active
    /// subscriber. Used by the cross-window tear-off handoff: the source window
    /// calls this BEFORE the destination attaches, satisfying the required
    /// detach-then-attach ordering (the relay client multiplexes a single
    /// output subscription per PTY id, so both attachments must never coexist).
    ///
    /// After detach the local session record is removed from the backend; the
    /// view's `Drop` → `close` call becomes a no-op, leaving the daemon PTY
    /// alive for the destination window's `attach_pty_existing`.
    ///
    /// The in-process portable PTY backend has no relay to detach from, so it
    /// falls back to `close` — tearing off an in-process terminal effectively
    /// closes it. That case is excluded at the menu level (`can_tear_off` is
    /// only `true` when `external_id()` is `Some`).
    pub fn detach(&self) {
        let id = self.session_id;
        self.with_backend(|be| {
            if let Err(err) = be.detach(id) {
                tracing::warn!(?err, session_id = ?id, "terminal detach failed");
            }
        });
    }

    /// OS pid of the shell child driving this session. Sub-pane split
    /// path uses this to query the current shell CWD via libproc and
    /// inherit it for the new pane.
    ///
    /// In-process backends answer directly. Daemon-backed sessions get
    /// a fallback: the daemon seeds the child pid into its checkpoint
    /// meta at spawn, and daemon + child run on the same host as the
    /// app, so the kernel cwd lookup downstream is just as valid. A
    /// pre-checkpoint daemon (no meta) or an exited shell yields `None`
    /// and callers fall back exactly as before.
    pub fn os_pid(&self) -> Option<u32> {
        let id = self.session_id;
        self.with_backend(|be| be.os_pid(id)).or_else(|| {
            let pty_id = self.external_id()?;
            let dir = crate::relay_cold_restore::default_checkpoints_dir()?;
            crate::relay_cold_restore::read_checkpoint_pid(&dir, &pty_id)
        })
    }

    /// F4.7: shell-tracked CWD via OSC 7. Returns `None` when the shell
    /// hasn't emitted an OSC 7 sequence yet (fresh spawn before first
    /// prompt) — callers fall back to libproc-on-`os_pid`. Reading this
    /// hint avoids a syscall per Cmd+D / Cmd+Shift+D and is what makes
    /// rapid-fire splits feel instant.
    pub fn cwd_hint(&self) -> Option<std::path::PathBuf> {
        let id = self.session_id;
        self.with_backend(|be| be.cwd_hint(id))
    }

    /// Replay captured bytes into this pane's grid BEFORE the live PTY
    /// produces output, so prior scrollback is visible on restart.
    pub fn prefill_grid(&self, bytes: &[u8]) {
        let id = self.session_id;
        self.with_backend(|be| {
            if let Err(err) = be.prefill_grid(id, bytes) {
                tracing::warn!(?err, "prefill_grid failed");
            }
        });
    }

}
