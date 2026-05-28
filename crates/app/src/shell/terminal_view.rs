//! TerminalView — single-pane PTY render.
//!
//! Owns a `PortablePtyBackend` and one session. A background polling task
//! ticks every `POLL_INTERVAL_MS`, drains the event queue, copies the latest
//! `TerminalSnapshot` onto the view, and calls `cx.notify()`.
//!
//! Rendering goes through a single `gpui::canvas` paint closure (see
//! `terminal_canvas::paint_grid`): backgrounds as quads, one `shape_line`
//! per row with `force_width` locking glyphs to the cell grid, cursor +
//! selection overlays. The closure also measures the canvas's real bounds
//! to size the PTY — it records the fitting `(cols, rows)` in the shared
//! `canvas_grid` cell and calls `window.refresh()` when that changes; the
//! next `render` reads it back into `target_grid` and `maybe_resize`
//! applies it. Driving the grid from the actual painted bounds (rather
//! than a viewport-minus-chrome estimate) is what keeps full-screen TUIs
//! from rendering their absolute-positioned UI at the wrong width, and it
//! makes split sub-panes size their PTY to their own slice for free.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Task, Window, canvas, div, px,
};
use oximux_agents::SharedBackend;
use oximux_pty::{
    PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId,
    TerminalSnapshot,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::Search;
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::key_input::keystroke_to_bytes;
use crate::shell::terminal_canvas::{PaintParams, grid_dims_for, paint_grid};
use crate::shell::terminal_search_overlay;
use crate::shell::terminal_search_state::{SearchKeyOutcome, SearchState};

/// How often the view drains events + re-snapshots. 16 ms ≈ 60 fps, matches the
/// `< 16 ms PTY → render latency` plan target without over-spending CPU on
/// idle.
const POLL_INTERVAL_MS: u64 = 16;

/// Cursor blink half-period. 530 ms matches the common terminal default and
/// the XTerm `cursorBlink` resource. One toggle per period — full blink cycle is
/// 1.06 s.
const BLINK_INTERVAL_MS: u64 = 530;

/// Default grid size on spawn. Parent-driven resize takes over on the first
/// render.
pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_ROWS: u16 = 32;

/// Set true while the app is shutting down (see the quit/window-close/
/// signal handlers in `main.rs`). `TerminalView::drop` reads it: when
/// set, it leaves the backend session ALIVE instead of closing it. The
/// relay daemon outlives the GUI, so a live PTY left running can be
/// reattached on the next launch — that's what restores a still-running
/// Claude Code / shell session byte-for-byte (raw replay + live repaint)
/// rather than the lossy fresh-spawn path. Tab-close and project-switch
/// run with this clear, so those Drops still tear the PTY down normally.
pub static APP_QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Spawn a local-shell PTY at `cwd` and wrap its backend in the shared-Arc
/// form `TerminalView::mount` expects. Centralizes the three previously-
/// duplicated spawn sites (workspace bootstrap, tab-strip new-tab,
/// pane-grid split). Returns `None` on spawn failure (caller logs + falls
/// back to a placeholder); the `tracing::warn` is emitted here so each
/// caller doesn't repeat the same line.
// Process-wide shared backend, installed once at app boot when the
// relay supervisor brings up a `RelayBackend`. Every pane mints a
// fresh `TerminalSessionId` on this single backend (the relay
// multiplexes the underlying socket). Unset = the relay couldn't
// start; fall back to per-pane `PortablePtyBackend` (today's
// behavior — no survival, but the app still works).
static SHARED_BACKEND: OnceLock<SharedBackend> = OnceLock::new();

pub fn install_shared_backend(backend: SharedBackend) {
    if SHARED_BACKEND.set(backend).is_err() {
        tracing::warn!("shared backend already installed; ignoring");
    }
}

/// Snapshot of the relay daemon's currently-live state. Returned by
/// [`relay_state_snapshot`] so the factory can do all the relay queries
/// once per project switch instead of per leaf.
pub struct RelayStateSnapshot {
    pub live_external_ids: std::collections::HashSet<String>,
    pub session_id: Option<String>,
}

pub fn relay_state_snapshot() -> RelayStateSnapshot {
    let Some(shared) = SHARED_BACKEND.get() else {
        return RelayStateSnapshot {
            live_external_ids: Default::default(),
            session_id: None,
        };
    };
    let guard = shared.lock().expect("shared backend poisoned");
    RelayStateSnapshot {
        live_external_ids: guard.list_external_ids().into_iter().collect(),
        session_id: guard.external_session_id(),
    }
}

/// Try to attach to a daemon-side PTY that's still alive. Returns
/// `None` when no shared backend is installed or the attach failed
/// (caller falls back to spawn + visual prefill).
pub fn attach_pty_existing(external_id: &str) -> Option<(SharedBackend, TerminalSessionId)> {
    let shared = SHARED_BACKEND.get()?;
    let mut guard = shared.lock().expect("shared backend poisoned");
    match guard.attach_existing(external_id) {
        Ok(session_id) => {
            drop(guard);
            Some((Arc::clone(shared), session_id))
        }
        Err(err) => {
            tracing::debug!(?err, external_id, "attach_existing failed");
            None
        }
    }
}

/// Look up the daemon-side identifier for a local session so the
/// caller can persist it for next-launch reconciliation. `None` when
/// no shared backend is installed or the backend has no external id
/// for this session.
pub fn external_id_for_session(id: TerminalSessionId) -> Option<String> {
    let shared = SHARED_BACKEND.get()?;
    shared.lock().ok()?.external_id_of(id)
}

pub fn spawn_local_pty(cwd: PathBuf) -> Option<(SharedBackend, TerminalSessionId)> {
    // Relay-backed path: one shared backend across the whole app.
    if let Some(shared) = SHARED_BACKEND.get() {
        let cfg = SpawnConfig {
            cwd: cwd.clone(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            ..SpawnConfig::default()
        };
        let mut guard = shared.lock().expect("shared backend poisoned");
        match guard.spawn(cfg) {
            Ok(session_id) => {
                drop(guard);
                return Some((Arc::clone(shared), session_id));
            }
            Err(err) => {
                drop(guard);
                tracing::warn!(?err, "relay-backed pty spawn failed; falling back");
                // fall through to the in-process backend
            }
        }
    }
    spawn_fallback_portable(cwd)
}

fn spawn_fallback_portable(cwd: PathBuf) -> Option<(SharedBackend, TerminalSessionId)> {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        cwd,
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        ..SpawnConfig::default()
    };
    let session_id = match backend.spawn(cfg) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "pty spawn failed");
            return None;
        }
    };
    let boxed: Box<dyn TerminalBackend> = Box::new(backend);
    Some((Arc::new(Mutex::new(boxed)), session_id))
}

/// F3.4: spawn a dormant session — grid emulator wired up but no PTY
/// child. The caller is expected to prefill the grid with restored
/// scrollback and then call `TerminalView::respawn` when the user
/// first interacts with the pane. Mirrors `spawn_local_pty`'s
/// relay-then-fallback pattern.
pub fn spawn_local_pty_dormant(cols: u16, rows: u16) -> Option<(SharedBackend, TerminalSessionId)> {
    if let Some(shared) = SHARED_BACKEND.get() {
        let mut guard = shared.lock().expect("shared backend poisoned");
        match guard.spawn_dormant(cols, rows) {
            Ok(session_id) => {
                drop(guard);
                return Some((Arc::clone(shared), session_id));
            }
            Err(err) => {
                drop(guard);
                tracing::debug!(?err, "shared backend rejected spawn_dormant; falling back");
            }
        }
    }
    let mut backend = PortablePtyBackend::new();
    let session_id = match backend.spawn_dormant(cols, rows) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "spawn_dormant failed");
            return None;
        }
    };
    let boxed: Box<dyn TerminalBackend> = Box::new(backend);
    Some((Arc::new(Mutex::new(boxed)), session_id))
}

pub struct TerminalView {
    /// `Arc<Mutex<Box<dyn TerminalBackend>>>` — shared with whoever spawned
    /// the session. For local terminals the renderer is the only holder;
    /// for agent tabs `CliRuntime` holds the same Arc inside its
    /// `SessionEntry` and the poll task races on this lock. Lock window is
    /// the duration of a single non-blocking trait call.
    backend: SharedBackend,
    session_id: TerminalSessionId,
    snapshot: TerminalSnapshot,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    target_grid: (u16, u16),
    last_resize: (u16, u16),
    /// Toggled by `_blink_task`. Render combines this with focus state to
    /// decide whether to overlay the cursor on `snapshot.cursor`. Reset to
    /// `true` on input + PTY output so the cursor doesn't blink invisible
    /// mid-typing.
    cursor_visible: bool,
    /// Mirrors `focus_handle.is_focused(window)` for the blink task, which
    /// runs in a `Context<Self>`-only closure with no `&Window` access. Kept
    /// in sync via `cx.on_focus` / `cx.on_blur` observers registered in
    /// `mount`. When `false`, the blink task skips `cx.notify()` so unfocused
    /// panes don't burn a repaint every 530 ms.
    focused: bool,
    /// Set when an UNFOCUSED pane raises a signal — today a terminal BEL
    /// (`TerminalEvent::Bell`); later also agent WaitingForInput/NeedsApproval
    /// and `oximux notify`. Drives the blue attention ring overlay; cleared
    /// when the pane gains focus (`on_focus`).
    attention: bool,
    /// Per-pane search overlay state. See `terminal_search_state.rs` for
    /// the state machine + key dispatch. The view owns I/O (grid fetch +
    /// `cx.notify`) and delegates everything else.
    search: SearchState,
    /// Latest OSC 2 title from the PTY (`TerminalEvent::TitleChange`). `None`
    /// until the shell emits one. Reserved for future use by the workspace
    /// tab strip — current labels are static `"Terminal N"` slugs.
    title: Option<String>,
    /// F3.4 dormant state: when `Some(cwd)`, this view holds a grid
    /// emulator with prefilled scrollback but NO live PTY child. The
    /// next focus-in / keystroke calls `respawn` to spawn a shell at
    /// `cwd` and arm the poll task. `None` = live (default).
    dormant_cwd: Option<PathBuf>,
    /// PTY-drain timer. `None` while the view is dormant — there's no
    /// PTY to drain. Armed by `mount` (live path) or `respawn`
    /// (post-dormancy).
    _poll_task: Option<Task<()>>,
    _blink_task: Task<()>,
    /// Desired grid size derived from the canvas's real painted bounds.
    /// The canvas paint closure writes this `(cols, rows)` each frame and
    /// calls `window.refresh()` when it changes; `render`'s top reads it
    /// back into `target_grid` so `maybe_resize` applies it. Shared via
    /// `Rc<Cell<_>>` (not entity state) so the paint closure never has to
    /// re-borrow this entity mid-paint — sizing the PTY from the actual
    /// bounds is what keeps full-screen TUIs from rendering scrambled.
    canvas_grid: Rc<Cell<(u16, u16)>>,
    /// Active text selection in cell coordinates: `(start_row, start_col,
    /// end_row, end_col)`. End is inclusive on both axes. `None` = no
    /// selection. Today this is set only by Cmd+A (select-all) since
    /// the canvas paint doesn't yet expose pixel-to-cell hit testing for
    /// drag-select; that ships in a follow-up slice. Cmd+C copies the
    /// extracted text when set, then clears it.
    selection: Option<(usize, usize, usize, usize)>,
}

impl TerminalView {
    /// Build a view around an already-spawned backend + session. The spawn
    /// is done outside `cx.new` because the entity builder closure is
    /// infallible; this keeps spawn errors at the caller where they can be
    /// logged + fall back to a placeholder.
    pub fn mount(
        backend: SharedBackend,
        session_id: TerminalSessionId,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = backend
            .lock()
            .expect("pty backend mutex poisoned")
            .snapshot(session_id)
            .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS));

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
        // owner and keystrokes are dropped until the first click.
        focus_handle.focus(window, cx);

        let poll_task = Self::start_poll_task(cx);
        let blink_task = Self::start_blink_task(cx);

        Self {
            backend,
            session_id,
            snapshot,
            theme,
            density,
            typography,
            focus_handle,
            target_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            last_resize: (DEFAULT_COLS, DEFAULT_ROWS),
            cursor_visible: true,
            // Init to false; the on_focus callback fires for the focus() above
            // and flips this true for whichever pane actually wins focus.
            // Multiple panes constructed in the same effect run will each see
            // their focus() call land, last one wins, on_blur clears the rest.
            focused: false,
            attention: false,
            search: SearchState::new(),
            title: None,
            dormant_cwd: None,
            _poll_task: Some(poll_task),
            _blink_task: blink_task,
            canvas_grid: Rc::new(Cell::new((DEFAULT_COLS, DEFAULT_ROWS))),
            selection: None,
        }
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
    pub fn mount_dormant(
        backend: SharedBackend,
        session_id: TerminalSessionId,
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
            let mut guard = backend.lock().expect("pty backend mutex poisoned");
            if let Err(err) = guard.prefill_grid(session_id, prefill_bytes) {
                tracing::warn!(?err, "dormant prefill_grid failed");
            }
        }
        let snapshot = backend
            .lock()
            .expect("pty backend mutex poisoned")
            .snapshot(session_id)
            .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS));

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
            snapshot,
            theme,
            density,
            typography,
            focus_handle,
            target_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            last_resize: (DEFAULT_COLS, DEFAULT_ROWS),
            cursor_visible: true,
            focused: false,
            attention: false,
            search: SearchState::new(),
            title: None,
            dormant_cwd: Some(cwd),
            _poll_task: None,
            _blink_task: blink_task,
            canvas_grid: Rc::new(Cell::new((DEFAULT_COLS, DEFAULT_ROWS))),
            selection: None,
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
            cols: self.target_grid.0.max(DEFAULT_COLS),
            rows: self.target_grid.1.max(DEFAULT_ROWS),
            ..SpawnConfig::default()
        };
        let session_id = self.session_id;
        let promote_result = self
            .backend
            .lock()
            .expect("pty backend mutex poisoned")
            .promote_to_live(session_id, cfg);
        if let Err(err) = promote_result {
            tracing::warn!(?err, "respawn promote_to_live failed; staying dormant");
            // Leaving dormant_cwd unset; user will see no shell. They
            // can still scroll through the prefilled grid.
            return;
        }
        self._poll_task = Some(Self::start_poll_task(cx));
        cx.notify();
    }

    /// 16 ms PTY-drain timer. Drains the event channel + re-snapshots + one
    /// `cx.notify()` per non-empty tick. Single notify per tick gives the
    /// "max 1 invalidation per frame" coalesce the perf plan asks for.
    fn start_poll_task(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone())
                else {
                    return;
                };
                executor
                    .timer(Duration::from_millis(POLL_INTERVAL_MS))
                    .await;
                if this.update(cx, |view, cx| view.tick(cx)).is_err() {
                    return;
                }
            }
        })
    }

    /// 530 ms cursor blink. Independent of the PTY poll so a chatty TUI
    /// doesn't bury the toggle and an idle shell still pulses. The toggle
    /// always runs (state stays truthful across focus changes); `cx.notify()`
    /// is gated on `view.focused` so unfocused panes contribute zero repaints
    /// per second instead of ~1.9 Hz.
    fn start_blink_task(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(executor) = this.read_with(cx, |_, cx| cx.background_executor().clone())
                else {
                    return;
                };
                executor
                    .timer(Duration::from_millis(BLINK_INTERVAL_MS))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        view.cursor_visible = !view.cursor_visible;
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
    fn with_backend<R>(&self, f: impl FnOnce(&mut dyn TerminalBackend) -> R) -> R {
        let mut be = self.backend.lock().expect("pty backend mutex poisoned");
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

    /// Backend's external identifier for this pane's session (e.g.,
    /// the relay daemon's PTY id), if any. Used by the phase-06
    /// reconciliation capture path. `None` for in-process backends.
    pub fn external_id(&self) -> Option<String> {
        let id = self.session_id;
        self.with_backend(|be| be.external_id_of(id))
    }

    /// OS pid of the shell child the backend spawned for this session.
    /// `None` for remote/relay backends and for already-exited shells.
    /// Sub-pane split path uses this to query the current shell CWD
    /// via libproc and inherit it for the new pane.
    pub fn os_pid(&self) -> Option<u32> {
        let id = self.session_id;
        self.with_backend(|be| be.os_pid(id))
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

    fn on_search(&mut self, _: &Search, _window: &mut Window, cx: &mut Context<Self>) {
        self.search.open();
        self.rerun_search();
        cx.notify();
    }

    fn rerun_search(&mut self) {
        let session_id = self.session_id;
        let grid = self.with_backend(|be| be.search_grid(session_id));
        let visible = self.snapshot.cells.len();
        self.search.rerun(&grid, visible);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match self.search.handle_key(event) {
            SearchKeyOutcome::Pass => {}
            SearchKeyOutcome::Consumed => return,
            SearchKeyOutcome::Dismissed | SearchKeyOutcome::CurrentChanged => {
                cx.notify();
                return;
            }
            SearchKeyOutcome::QueryChanged => {
                self.rerun_search();
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
                    if let Some(sel) = self.selection.take() {
                        let text = extract_selection_text(&self.snapshot, sel);
                        if !text.is_empty() {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        }
                        cx.notify();
                        return;
                    }
                    self.send_bytes(b"\x03", cx);
                    return;
                }
                "a" => {
                    // Select every visible cell. End col is the widest
                    // populated row's last col — saturating at 0 so an
                    // empty snapshot doesn't underflow.
                    let rows = self.snapshot.cells.len();
                    if rows == 0 {
                        return;
                    }
                    let end_row = rows - 1;
                    let end_col = self
                        .snapshot
                        .cells
                        .iter()
                        .map(|r| r.len())
                        .max()
                        .unwrap_or(0)
                        .saturating_sub(1);
                    self.selection = Some((0, 0, end_row, end_col));
                    cx.notify();
                    return;
                }
                _ => {}
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

        let bytes = keystroke_to_bytes(ks);
        self.send_bytes(&bytes, cx);
    }

    fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
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
        if let Err(err) = self.with_backend(|be| be.write(session_id, bytes)) {
            tracing::warn!(?err, "pty write failed");
            return;
        }
        // Force cursor visible on input — otherwise a blink-off tick at the
        // moment of keypress hides the cursor when the user most wants to
        // see it.
        self.cursor_visible = true;
        cx.notify();
    }

    /// F3.4: window-less variant of `respawn_if_dormant` used from
    /// `send_bytes`. The promote-to-live + poll-task arm steps don't
    /// actually need a `&mut Window` (no focus changes, no platform
    /// integration). Kept separate so the focus path stays the same.
    fn wake_dormant_inline(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.dormant_cwd.take() else {
            return;
        };
        let cfg = SpawnConfig {
            cwd,
            cols: self.target_grid.0.max(DEFAULT_COLS),
            rows: self.target_grid.1.max(DEFAULT_ROWS),
            ..SpawnConfig::default()
        };
        let session_id = self.session_id;
        let promote_result = self
            .backend
            .lock()
            .expect("pty backend mutex poisoned")
            .promote_to_live(session_id, cfg);
        if let Err(err) = promote_result {
            tracing::warn!(?err, "wake_dormant promote_to_live failed");
            return;
        }
        self._poll_task = Some(Self::start_poll_task(cx));
    }

    fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
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
        let session_id = self.session_id;
        let wrap = self
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

    fn tick(&mut self, cx: &mut Context<Self>) {
        // Use the per-session drain so panes can't steal each other's
        // events from a shared backend (e.g., the relay). The global
        // `drain_events` is reserved for tests + cleanup paths.
        let session_id_for_drain = self.session_id;
        let events = self.with_backend(|be| be.drain_events_for(session_id_for_drain));
        if events.is_empty() {
            return;
        }
        // Resnapshot on `Output` (new bytes landed in the grid) AND `Resize`
        // (Term::resize reflowed existing rows + may have shrunk row count).
        // Skipping `Resize` here was the cause of the post-split clipping
        // regression: the cached snapshot kept its pre-resize dimensions
        // and overflowed the narrower pane until the shell echoed again.
        // `Exit` still falls through — no grid mutation, no resnap needed,
        // and avoids pinning the cursor visible on a dead session.
        let mut needs_snapshot = false;
        let mut had_output = false;
        let mut got_bell = false;
        let mut latest_title: Option<String> = None;
        for ev in &events {
            match ev {
                TerminalEvent::Output { .. } => {
                    needs_snapshot = true;
                    had_output = true;
                }
                TerminalEvent::Resize { .. } => needs_snapshot = true,
                TerminalEvent::TitleChange { title, .. } => {
                    latest_title = Some(title.clone());
                }
                // A BEL while this pane is NOT focused raises attention. A bell
                // in the pane you're already looking at is just noise.
                TerminalEvent::Bell { .. } if !self.focused => got_bell = true,
                _ => {}
            }
        }
        if got_bell {
            self.attention = true;
        }
        let session_id = self.session_id;
        if needs_snapshot && let Ok(snapshot) = self.with_backend(|be| be.snapshot(session_id)) {
            self.snapshot = snapshot;
        }
        if had_output {
            self.cursor_visible = true;
        }
        if let Some(title) = latest_title {
            self.title = Some(title);
        }
        cx.notify();
    }

    /// Latest OSC 2 title the shell emitted, if any. Exposed for future use
    /// by the workspace tab strip.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// True when this pane has a pending attention signal (an unfocused-pane
    /// BEL today). Read by the tab strip so a bell in a BACKGROUND tab lights
    /// that tab — the pane ring alone is invisible when the pane isn't shown.
    /// Cleared when the pane gains focus.
    pub fn attention(&self) -> bool {
        self.attention
    }

    fn maybe_resize(&mut self) {
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
            self.snapshot = snapshot;
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
    fn pull_canvas_grid(&mut self) {
        self.target_grid = self.canvas_grid.get();
    }
}

impl Drop for TerminalView {
    /// Tear down the PTY session when the entity is dropped (tab close,
    /// project switch, or content replace in `MainPane::open_editor_*`).
    ///
    /// Without this, every dropped view leaks the backend session: the
    /// portable-pty path retains a watcher thread + master fd until the
    /// backend Arc count hits zero, and the relay path retains the daemon-
    /// side PTY indefinitely. `TerminalBackend::close` is idempotent, so
    /// a co-owner (e.g., `CliRuntime::cancel`) racing this Drop is safe.
    ///
    /// The close path can stall up to `CANCEL_GRACE` (5 s) while it joins
    /// the watcher thread, so we run it on a detached OS thread instead
    /// of blocking the GPUI main thread (which is where Drop fires). The
    /// `SharedBackend` Arc is cloned into the thread, keeping the backend
    /// alive until close completes. `drain_events` runs first so the
    /// watcher thread isn't blocked on a full event-channel send during
    /// the join (mirrors the deadlock fix in agent runtime cancel).
    ///
    /// Mutex poisoning is treated as best-effort: log + skip rather than
    /// panic inside Drop (a panic in Drop aborts the process).
    fn drop(&mut self) {
        // App-quit path: leave the backend session alive. The relay
        // daemon survives the GUI process, so a still-running PTY can be
        // reattached on next launch and its live screen replayed byte-
        // for-byte. Closing here would SIGTERM the child (e.g. a running
        // agent) and downgrade restore to the lossy fresh-spawn path.
        // This branch only matters for graceful quit (where GPUI drops
        // every view); tab-close / project-switch keep the normal
        // teardown below. In-process portable PTYs die with the process
        // regardless, so skipping their close on quit is harmless.
        if APP_QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let id = self.session_id;
        let backend = self.backend.clone();
        std::thread::spawn(move || match backend.lock() {
            Ok(mut be) => {
                let _ = be.drain_events();
                if let Err(err) = be.close(id) {
                    tracing::warn!(
                        ?err,
                        ?id,
                        "terminal-view: backend.close failed in drop helper"
                    );
                }
            }
            Err(_) => {
                tracing::warn!(?id, "terminal-view: backend mutex poisoned at drop");
            }
        });
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Adopt the grid size the canvas measured from its real bounds
        // last paint, then apply it. `maybe_resize` resizes the PTY +
        // refetches the snapshot only when the size actually changed.
        self.pull_canvas_grid();
        self.maybe_resize();

        // Cursor visibility rules:
        //   - Focused + blink on  → solid inverse block (active cursor)
        //   - Focused + blink off → hidden (mid-blink phase)
        //   - Unfocused           → ghost cursor: inverted but bg dimmed to
        //                           `UNFOCUSED_CURSOR_ALPHA` so the user can
        //                           still see where the shell's caret sits
        //                           without it competing with the focused pane.
        // Out-of-grid sentinels keep `build_row`'s cursor-col check fall-
        // through silently in the hidden case — no `if` gating in the hot
        // path. Pane-focus also drives the inactive FG dim
        // (`terminal_row::UNFOCUSED_FG_ALPHA`).
        let pane_focused = self.focus_handle.is_focused(window);
        let cursor_visible = !pane_focused || self.cursor_visible;
        let cursor = if cursor_visible {
            (
                self.snapshot.cursor.0 as usize,
                self.snapshot.cursor.1 as usize,
            )
        } else {
            (usize::MAX, usize::MAX)
        };
        let theme = self.theme;
        let pad = self.density.pad_panel;
        let focus_handle = self.focus_handle.clone();

        // Match buckets per visible row. History_len was captured at scan
        // time in `SearchState::rerun` — if PTY output has scrolled history
        // since the scan, highlights may drift one paint but self-correct
        // on the next keystroke.
        let visible_rows = self.snapshot.cells.len();
        let buckets = self.search.render_buckets(visible_rows);

        // Build owned paint params (`FnOnce + 'static` requires no
        // borrows). Clone is cheap: snapshot is a Vec<Vec<Cell>> already
        // sized to the visible grid, buckets are tiny per-row vecs of
        // MatchHit (Copy), theme/typography/cursor are POD-sized.
        let paint_params = PaintParams {
            snapshot: self.snapshot.clone(),
            theme,
            typography: self.typography.clone(),
            cursor,
            buckets,
            pane_focused,
            pad,
            selection: self.selection,
        };

        let overlay = if self.search.active {
            let badge = self.search.count_badge();
            let query = self.search.query.clone();
            let typography = self.typography.clone();
            let options = self.search.options;
            // Caret blinks in lock-step with the terminal cursor — the
            // existing 530ms blink_task already drives `cursor_visible`,
            // so the overlay caret needs no second timer.
            let caret_on = self.cursor_visible;
            // Toggle handlers flip the bit and rerun the scan. Rerun
            // touches the backend so it has to live inside the listener
            // (we have `&mut Self` + `&mut Context` here).
            let on_toggle_case = Box::new(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.search.toggle_case_sensitive();
                this.rerun_search();
                cx.notify();
            })) as terminal_search_overlay::ToggleHandler;
            let on_toggle_word = Box::new(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.search.toggle_whole_word();
                this.rerun_search();
                cx.notify();
            })) as terminal_search_overlay::ToggleHandler;
            let on_toggle_regex = Box::new(cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                this.search.toggle_regex();
                this.rerun_search();
                cx.notify();
            })) as terminal_search_overlay::ToggleHandler;
            let on_prev = Box::new(cx.listener(|this, _, _, cx| {
                this.search.prev_match();
                cx.notify();
            })) as terminal_search_overlay::ClickHandler;
            let on_next = Box::new(cx.listener(|this, _, _, cx| {
                this.search.next_match();
                cx.notify();
            })) as terminal_search_overlay::ClickHandler;
            let on_close = Box::new(cx.listener(|this, _, _, cx| {
                this.search.close();
                cx.notify();
            })) as terminal_search_overlay::ClickHandler;
            Some(terminal_search_overlay::build(
                terminal_search_overlay::Params {
                    query: &query,
                    badge,
                    caret_on,
                    options,
                    theme: &theme,
                    typography: &typography,
                    on_toggle_case,
                    on_toggle_word,
                    on_toggle_regex,
                    on_prev,
                    on_next,
                    on_close,
                },
            ))
        } else {
            None
        };

        // F3.4 slice 3: surface dormancy. A restored sub-pane keeps its
        // prefilled scrollback but holds no shell child yet — the user
        // could otherwise see static text and wonder why nothing reacts.
        // The badge auto-clears once `respawn_if_dormant` flips the
        // backend live + `cx.notify()` re-renders.
        let dormant_badge = self.is_dormant().then(|| build_dormant_badge(&theme));

        // The grid is painted into a canvas child filling the pane body.
        // `canvas(prepaint, paint)` defers everything to a single paint
        // closure: we measure cell metrics, group runs, and emit
        // `paint_quad` + `shape_line` calls directly — no flex layout
        // round-trip per cell. The outer div keeps its existing role as
        // the focus owner, action target, and click-to-focus surface.
        //
        // The paint closure ALSO drives the PTY resize: it derives
        // (cols, rows) from the canvas's real `bounds` and records them in
        // the shared `canvas_grid` cell, then asks for a repaint via
        // `window.refresh()` when the size changed. The next `render`
        // reads `canvas_grid` back into `target_grid` and resizes the PTY.
        //
        // Recording into an `Rc<Cell<_>>` (instead of calling
        // `entity.update` here) keeps the paint phase free of any re-borrow
        // of this entity — `window.refresh()` is documented as safe to
        // call while drawing (it no-ops if already mid-draw and otherwise
        // marks the window dirty for the next frame).
        let dims_typography = self.typography.clone();
        let canvas_grid = Rc::clone(&self.canvas_grid);
        let grid_canvas = canvas(
            // Prepaint: no per-paint state to capture; return unit.
            |_bounds, _window, _cx| (),
            move |bounds, _: (), window, cx| {
                let metrics = CellMetrics::measure(&dims_typography, window);
                let dims = grid_dims_for(bounds, &metrics, paint_params.pad);
                if canvas_grid.get() != dims {
                    canvas_grid.set(dims);
                    // Schedule a frame so `render` applies the new size +
                    // refetches the reflowed snapshot. Needed because an
                    // idle terminal emits no PTY output to trigger a
                    // repaint on its own.
                    window.refresh();
                }
                paint_grid(bounds, &paint_params, window, cx);
            },
        )
        .size_full();

        let mut root = div()
            .id("oximux-terminal-view")
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            // Anchor for absolute-positioned overlays (dormant badge,
            // search overlay) — the canvas child stays in flex flow.
            .relative()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            // `.font(...)` over `.font_family(...)` so the configured
            // fallback chain (SF Mono → Menlo → Monaco) takes effect when
            // the primary face (Geist Mono) isn't installed. `font_family`
            // takes a single literal name and never cascades. The canvas
            // paint reads typography directly, but keeping the font on
            // the root is the right default for any non-canvas children
            // (overlays, error banners) that may inherit it later.
            .font(self.typography.mono_font())
            .text_size(px(self.typography.t_body_lg))
            .on_action(cx.listener(Self::on_search))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    // Notify so `MainPane`'s observer can re-sync the focused
                    // PaneId and repaint the active-pane ring on the next
                    // frame. Without this, click-to-focus is invisible until
                    // the next Cmd-* action.
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .child(grid_canvas);
        if let Some(o) = overlay {
            root = root.child(o);
        }
        if let Some(badge) = dormant_badge {
            root = root.child(badge);
        }
        // Attention ring: a blue inset stroke when an unfocused pane has
        // signalled (terminal BEL today; agent-waiting / `oximux notify`
        // later). Absolute inset_0 so it overlays without shifting the grid
        // layout, and gated on `!pane_focused` so it vanishes the instant the
        // user looks at the pane (belt-and-braces with the on_focus clear).
        if self.attention && !pane_focused {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .border_2()
                    .border_color(theme.status_info),
            );
        }
        root
    }
}

/// Extract the text covered by a cell-coordinate selection from the
/// current snapshot. End coords are inclusive. Each row is right-trimmed
/// of trailing whitespace then joined with `\n` — matching the
/// common terminal "copy preserves visual newlines but not visual
/// padding" convention. Out-of-range coordinates clamp silently to grid.
fn extract_selection_text(
    snapshot: &TerminalSnapshot,
    (start_row, start_col, end_row, end_col): (usize, usize, usize, usize),
) -> String {
    let rows = &snapshot.cells;
    if rows.is_empty() {
        return String::new();
    }
    let last_row = rows.len() - 1;
    let r0 = start_row.min(last_row);
    let r1 = end_row.min(last_row);
    let mut out = String::with_capacity((r1 - r0 + 1) * 80);
    for (row_idx, row) in rows.iter().enumerate().skip(r0).take(r1 - r0 + 1) {
        // Column window depends on row position within the rectangle.
        // For the single-row case (r0 == r1), use the start..=end col
        // range; for multi-row, the first row runs start_col..=end of
        // row, intermediate rows run 0..=end of row, last row runs
        // 0..=end_col.
        let last_col_in_row = row.len().saturating_sub(1);
        let (c0, c1) = if r0 == r1 {
            (start_col.min(last_col_in_row), end_col.min(last_col_in_row))
        } else if row_idx == r0 {
            (start_col.min(last_col_in_row), last_col_in_row)
        } else if row_idx == r1 {
            (0, end_col.min(last_col_in_row))
        } else {
            (0, last_col_in_row)
        };
        if c1 < c0 {
            out.push('\n');
            continue;
        }
        let mut line = String::with_capacity(c1 - c0 + 1);
        for cell in &row[c0..=c1] {
            let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
            line.push(ch);
        }
        // Right-trim trailing spaces — matches user intuition that a
        // selected line ending in blank padding shouldn't paste 80 spaces.
        let trimmed = line.trim_end();
        out.push_str(trimmed);
        // Emit `\n` between rows in the rect, but not after the last row.
        if row_idx < r1 {
            out.push('\n');
        }
    }
    out
}

/// F3.4 slice 3: tiny corner chip indicating a restored-dormant sub-pane.
/// Renders as an absolutely-positioned tag in the top-right of the pane
/// body. Click/focus anywhere in the pane already wakes it via the
/// `on_focus` observer wired in `mount_dormant`, so the badge is purely
/// informational — no click handler needed.
fn build_dormant_badge(theme: &Theme) -> gpui::Div {
    div()
        .absolute()
        .top(px(6.0))
        .right(px(10.0))
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .bg(theme.bg_overlay)
        .text_color(theme.fg_muted)
        .text_size(px(11.0))
        .border_1()
        .border_color(theme.border_inactive)
        // `↻` = U+21BB Clockwise Open Circle Arrow. Hint copy mirrors
        // the dormancy contract: the shell isn't running yet; first
        // focus or keystroke wakes it at the saved cwd.
        .child("↻ restored — click to wake")
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use oximux_pty::{Cell, CellColor};

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            fg: CellColor::Default,
            bg: CellColor::Default,
            inverse: false,
            dim: false,
        }
    }

    fn snap(rows: &[&str]) -> TerminalSnapshot {
        let cells: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| row.chars().map(cell).collect::<Vec<_>>())
            .collect();
        let cols = cells.iter().map(|r| r.len()).max().unwrap_or(0) as u16;
        let rows_n = cells.len() as u16;
        TerminalSnapshot {
            cols,
            rows: rows_n,
            cursor: (0, 0),
            cells,
        }
    }

    #[test]
    fn extract_single_row_substring() {
        let s = snap(&["hello world"]);
        let txt = extract_selection_text(&s, (0, 0, 0, 4));
        assert_eq!(txt, "hello");
    }

    #[test]
    fn extract_full_grid_joins_with_newlines() {
        let s = snap(&["foo", "bar"]);
        let txt = extract_selection_text(&s, (0, 0, 1, 2));
        assert_eq!(txt, "foo\nbar");
    }

    #[test]
    fn extract_right_trims_trailing_spaces() {
        let s = snap(&["hi   ", "ok"]);
        let txt = extract_selection_text(&s, (0, 0, 1, 4));
        assert_eq!(txt, "hi\nok");
    }

    #[test]
    fn extract_empty_snapshot_returns_empty_string() {
        let s = snap(&[]);
        let txt = extract_selection_text(&s, (0, 0, 5, 5));
        assert_eq!(txt, "");
    }

    #[test]
    fn extract_clamps_out_of_range_indices() {
        let s = snap(&["abc"]);
        // Asking for end_row=10 on a 1-row snapshot should clamp to row 0.
        let txt = extract_selection_text(&s, (0, 0, 10, 10));
        assert_eq!(txt, "abc");
    }

    #[test]
    fn extract_middle_row_uses_full_width() {
        let s = snap(&["aa", "bbbb", "cc"]);
        // Multi-row select: first row clips start col, middle row is
        // full width, last row clips end col.
        let txt = extract_selection_text(&s, (0, 1, 2, 0));
        assert_eq!(txt, "a\nbbbb\nc");
    }
}
