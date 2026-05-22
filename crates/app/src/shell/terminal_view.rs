//! TerminalView — single-pane PTY render (Phase 1 step 4 + polish + step 5).
//!
//! Owns a `PortablePtyBackend` and one session. A background polling task
//! ticks every `POLL_INTERVAL_MS`, drains the event queue, copies the latest
//! `TerminalSnapshot` onto the view, and calls `cx.notify()`. Render walks
//! `snapshot.cells`, groups consecutive same-styled cells into runs, and
//! emits one styled `div` per run inside one row `div` per line.
//!
//! Polish slice added color (per-run fg+bg against the charcoal theme),
//! cursor (cell under `snapshot.cursor` forced to `inverse` so the block
//! cursor renders for free), and resize.
//!
//! Step 5 moves resize math out of this module. The view no longer measures
//! the window itself — the parent (`MainPane`) computes each leaf's slice
//! of the visible area, divides by cell metrics, and calls
//! `set_target_grid(cols, rows)`. The view stages that target and applies
//! it on the next `maybe_resize` tick. This lets one window host an
//! arbitrary tree of splits without each leaf double-counting chrome.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Task, Window, div, px,
};
use oximux_agents::SharedBackend;
use oximux_pty::{
    PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId,
    TerminalSnapshot,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::Search;
use crate::shell::cell_metrics::LINE_HEIGHT_EXTRA;
use crate::shell::key_input::keystroke_to_bytes;
use crate::shell::terminal_row::build_row;
use crate::shell::terminal_search_overlay;
use crate::shell::terminal_search_state::{SearchKeyOutcome, SearchState};

/// How often the view drains events + re-snapshots. 16 ms ≈ 60 fps, matches the
/// `< 16 ms PTY → render latency` plan target without over-spending CPU on
/// idle.
const POLL_INTERVAL_MS: u64 = 16;

/// Cursor blink half-period. 530 ms matches Terminal.app's default and the
/// XTerm `cursorBlink` resource. One toggle per period — full blink cycle is
/// 1.06 s.
const BLINK_INTERVAL_MS: u64 = 530;

/// Default grid size on spawn. Parent-driven resize takes over on the first
/// render.
pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_ROWS: u16 = 32;

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
    /// Per-pane search overlay state. See `terminal_search_state.rs` for
    /// the state machine + key dispatch. The view owns I/O (grid fetch +
    /// `cx.notify`) and delegates everything else.
    search: SearchState,
    /// Latest OSC 2 title from the PTY (`TerminalEvent::TitleChange`). `None`
    /// until the shell emits one. Reserved for future use by the workspace
    /// tab strip — current labels are static `"Terminal N"` slugs.
    title: Option<String>,
    _poll_task: Task<()>,
    _blink_task: Task<()>,
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
            search: SearchState::new(),
            title: None,
            _poll_task: poll_task,
            _blink_task: blink_task,
        }
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

    /// Stage a new target grid for the next resize tick. Called by the
    /// parent layout (`MainPane`) per render with the leaf's slice of the
    /// visible area, already divided by cell metrics.
    pub fn set_target_grid(&mut self, cols: u16, rows: u16) {
        self.target_grid = (cols, rows);
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
        // them. We intercept the two terminal-specific ones here, where the
        // view has access to `App` (clipboard) and to the backend (session-
        // specific bracketed-paste state).
        //
        // Cmd+C → SIGINT (0x03) is a placeholder until mouse text selection
        // lands. Matches Terminal.app / iTerm2 fallback when nothing is
        // selected. Once selection exists, branch on selection-empty.
        //
        // Shift is excluded so `Cmd+Shift+C` (often "copy as plain text" or
        // "interrupt" in other terminals) doesn't silently send SIGINT here.
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
                    self.send_bytes(b"\x03", cx);
                    return;
                }
                _ => {}
            }
        }

        let bytes = keystroke_to_bytes(ks);
        self.send_bytes(&bytes, cx);
    }

    fn send_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
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
                _ => {}
            }
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
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        // Source line-height from `cell_metrics::LINE_HEIGHT_EXTRA` so this
        // never drifts from `MainPane`'s grid math. Tight ratio (≈ 1.21×)
        // sits inside Menlo's em-square at 14 pt and keeps half-block
        // glyphs (▀ ▄) tiling — the Claude Code mascot is the canonical
        // regression case.
        let line_height = px(self.typography.t_body_lg + LINE_HEIGHT_EXTRA);
        let pad = px(self.density.pad_panel);
        let focus_handle = self.focus_handle.clone();

        // Match buckets per visible row. History_len was captured at scan
        // time in `SearchState::rerun` — if PTY output has scrolled history
        // since the scan, highlights may drift one paint but self-correct
        // on the next keystroke.
        let visible_rows = self.snapshot.cells.len();
        let buckets = self.search.render_buckets(visible_rows);

        let rows: Vec<gpui::Div> = self
            .snapshot
            .cells
            .iter()
            .enumerate()
            .map(|(row_idx, row)| {
                let match_cols = buckets.get(row_idx).map(|v| v.as_slice());
                build_row(
                    row,
                    row_idx,
                    cursor,
                    match_cols,
                    &theme,
                    line_height,
                    pane_focused,
                )
            })
            .collect();

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

        let mut root = div()
            .id("oximux-terminal-view")
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            // `.font(...)` over `.font_family(...)` so the configured
            // fallback chain (SF Mono → Menlo → Monaco) takes effect when
            // the primary face (Geist Mono) isn't installed. `font_family`
            // takes a single literal name and never cascades.
            .font(self.typography.mono_font())
            .text_size(px(self.typography.t_body_lg))
            .px(pad)
            .py(pad)
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
            .children(rows);
        if let Some(o) = overlay {
            root = root.child(o);
        }
        root
    }
}
