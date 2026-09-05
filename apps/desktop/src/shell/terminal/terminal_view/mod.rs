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
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, InputHandler,
    InteractiveElement, IntoElement, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollWheelEvent, Styled,
    Task, TouchPhase, UTF16Selection, WeakEntity, Window, canvas, div, point, px, relative, size,
};
use oximux_agents::SharedBackend;
use oximux_pty::{
    CommandMarkKind, PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent,
    TerminalSessionId, TerminalSnapshot,
};
use oximux_settings::{BellStyle, Density, TerminalSettings, Theme, Typography};

use crate::actions::{
    FindNextMatch, FindPrevMatch, OpenTerminalContextMenuAt, Search, SendLastCommandOutputToAgent,
    SendTerminalSelectionToAgent, SendTextToActiveAgent,
};
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::context_env::SurfaceIds;
use crate::shell::key_input::{is_ime_text_key, keystroke_to_bytes};
use crate::shell::mouse_report::{MouseAction, MouseBtn, encode_button, encode_scroll, mod_bits};
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_scrollbar::{ScrollbarDrag, drag_to_offset, thumb_geometry};
use crate::shell::terminal_links::{Existence, ExistenceCache, LinkMatch, LinkTarget, detect_at};
use crate::shell::terminal_canvas::{
    Alphas, PaintParams, grid_dims_for, paint_grid, point_to_cell,
};
use crate::shell::terminal_search_overlay;
use crate::shell::terminal_search_state::{SearchKeyOutcome, SearchState};

/// How often the view drains events + re-snapshots. 16 ms ≈ 60 fps, matches the
/// `< 16 ms PTY → render latency` plan target without over-spending CPU on
/// idle.
const POLL_INTERVAL_MS: u64 = 16;
/// Drain cadence for a hidden (non-visible) tab. Output isn't lost — it
/// buffers in the PTY/relay channel and drains in larger batches; we just wake
/// far less often so N background agents don't each schedule a 60fps repaint.
const BACKGROUND_POLL_INTERVAL_MS: u64 = 100;

/// Frames a keystroke keeps the render loop self-scheduling so a straggler echo
/// renders within one frame even if the run loop would otherwise doze between
/// sparse keystrokes. ~10 frames ≈ 160ms at 60fps — covers a slow program's
/// echo without pinning the CPU once typing pauses.
const POST_INPUT_DRAIN_FRAMES: u8 = 10;

/// PTY-drain cadence for a view given its on-screen state. Visible tabs drain
/// at ~60fps; hidden tabs drain far less often (output buffers, not dropped).
const fn poll_interval_ms(visible: bool) -> u64 {
    if visible {
        POLL_INTERVAL_MS
    } else {
        BACKGROUND_POLL_INTERVAL_MS
    }
}

/// Debug-only keystroke→echo latency probe. Off unless `OXIMUX_INPUT_TRACE` is
/// set in the environment; when on, appends `<unix_micros> <msg>` lines to
/// `/tmp/oximux_input_trace.log` at the input-arrival and echo-render points,
/// so a reproduction can be timed without sample-window coordination — the gaps
/// between `key_down`/`ime_commit`/`send_bytes` and `echo_render` are the felt
/// latency. Compiled out of release builds.
#[cfg(debug_assertions)]
fn input_trace(msg: &str) {
    use std::io::Write as _;
    use std::sync::OnceLock;
    // Opt-in via `OXIMUX_INPUT_TRACE=1` so normal debug runs pay nothing. When
    // set, appends the input→echo→frame timeline to `/tmp/oximux_input_trace.log`
    // for latency profiling. Compiled out of release entirely.
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("OXIMUX_INPUT_TRACE").is_some()) {
        return;
    }
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/oximux_input_trace.log")
    {
        let _ = writeln!(f, "{micros} {msg}");
    }
}
#[cfg(not(debug_assertions))]
#[inline]
fn input_trace(_msg: &str) {}

/// Default grid size on spawn. Parent-driven resize takes over on the first
/// render.
pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_ROWS: u16 = 32;

/// Key-dispatch context set on the focused terminal's root element. Used only
/// to shadow the host's global Tab / Shift+Tab focus-navigation bindings (see
/// [`register_terminal_key_bindings`]) so those keys reach the shell instead
/// of cycling UI focus.
const TERMINAL_KEY_CONTEXT: &str = "OximuxTerminal";

/// Bind Tab / Shift+Tab to no-ops within the terminal's key context.
///
/// The component host installs a global `tab` → focus-next (and `shift-tab` →
/// focus-prev) binding so keyboard users can walk the UI's focusable controls.
/// That binding consumes the keystroke before it can reach the focused
/// terminal, so pressing Tab at a shell prompt jumped focus to a side panel
/// instead of triggering completion (`cd <Tab>` should list directories).
///
/// Binding the same chords to a no-op in a context that only the terminal
/// carries wins on dispatch-depth (a descendant context outranks the root's),
/// and a no-op binding leaves the keystroke unhandled — so it falls through to
/// the view's `on_key_down`, which forwards `\t` / backtab to the PTY. The
/// context only participates in dispatch while focus is inside the terminal
/// subtree, so when another surface is focused the host's navigation bindings
/// resolve as before.
///
/// Call once at boot. Order vs. the host's own `bind_keys` is irrelevant —
/// precedence is by dispatch depth, not registration order.
pub fn register_terminal_key_bindings(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("tab", gpui::NoAction, Some(TERMINAL_KEY_CONTEXT)),
        gpui::KeyBinding::new("shift-tab", gpui::NoAction, Some(TERMINAL_KEY_CONTEXT)),
    ]);
}

/// Read the live terminal settings global, falling back to defaults when it
/// isn't installed (headless tests, early startup before `set_global`).
pub fn terminal_settings(cx: &App) -> TerminalSettings {
    cx.try_global::<TerminalSettings>().cloned().unwrap_or_default()
}

mod spawn_settings;
pub use spawn_settings::{
    set_shell_integration_enabled, set_spawn_scrollback, set_spawn_shell, set_spawn_shell_resolved,
};
pub(crate) use spawn_settings::shell_integration_enabled;
use spawn_settings::{shell_spawn_config, spawn_scrollback};

/// Width (px) of the overlay scrollbar gutter on the terminal's right edge.
const SCROLLBAR_WIDTH: f32 = 10.0;

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

/// Clone of the process-wide relay backend, if the daemon came up at boot.
/// Handed to `CliRuntime` so agent PTYs spawn through the same surviving
/// daemon as plain terminals (enables agent-tab re-attach across restarts).
/// `None` when no relay is installed — agents then use a private in-process
/// PTY that dies with the app.
pub fn shared_backend() -> Option<SharedBackend> {
    SHARED_BACKEND.get().map(Arc::clone)
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
    // `list_external_ids` is a synchronous daemon round-trip on whichever
    // thread calls it (the post-paint reconcile runs it on the background
    // executor; the project-switch capture path still calls it from the
    // main thread). Hold off App Nap so the system can't wedge the calling
    // thread while it's in flight.
    let _nap = crate::app_nap::prevent("relay list ptys");
    let guard = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
    // Attach is a synchronous daemon round-trip on the calling thread
    // (background executor from the restore reconcile; main thread from
    // tear-off). Hold off App Nap for its duration.
    let _nap = crate::app_nap::prevent("relay attach");
    let mut guard = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// The relay daemon's session id from the cached handshake — a lock and
/// a clone, NO daemon round-trip (unlike `relay_state_snapshot`, whose
/// ListPtys is a wire RPC). For periodic callers (layout autosave) that
/// need to scope `pane_relay_ids` rows without paying for a snapshot.
pub fn relay_session_id_cached() -> Option<String> {
    let shared = SHARED_BACKEND.get()?;
    shared.lock().ok()?.external_session_id()
}

pub fn spawn_local_pty(
    cwd: PathBuf,
    env: Vec<(String, String)>,
) -> Option<(SharedBackend, TerminalSessionId)> {
    spawn_local_pty_sized(cwd, env, None)
}

/// `spawn_local_pty` with explicit initial PTY dimensions. The cold
/// restore path passes the dead PTY's checkpointed (cols, rows) so the
/// replacement shell's first paint wraps for the size the restored
/// content used — the pane's normal resize takes over right after
/// adopt, so this only matters for that first prompt.
pub fn spawn_local_pty_sized(
    cwd: PathBuf,
    env: Vec<(String, String)>,
    dims: Option<(u16, u16)>,
) -> Option<(SharedBackend, TerminalSessionId)> {
    let (cols, rows) = dims.unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    // Relay-backed path: one shared backend across the whole app.
    if let Some(shared) = SHARED_BACKEND.get() {
        let mut cfg = shell_spawn_config(cwd.clone(), env.clone(), cols, rows);
        super::shell_integration::augment_spawn_config(&mut cfg);
        // Spawn is a synchronous daemon round-trip on the calling thread
        // (background executor from the restore reconcile; main thread for
        // interactive new-tab/split spawns). Hold off App Nap so it can't
        // wedge mid-request.
        let _nap = crate::app_nap::prevent("relay spawn");
        let mut guard = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
    spawn_fallback_portable(cwd, env, (cols, rows))
}

fn spawn_fallback_portable(
    cwd: PathBuf,
    env: Vec<(String, String)>,
    (cols, rows): (u16, u16),
) -> Option<(SharedBackend, TerminalSessionId)> {
    let mut backend = PortablePtyBackend::new();
    let mut cfg = shell_spawn_config(cwd, env, cols, rows);
    super::shell_integration::augment_spawn_config(&mut cfg);
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

/// Spawn an ACP embedded-terminal command — a specific program + argv, not an
/// interactive login shell — through the same relay-then-fallback path as
/// [`spawn_local_pty`]. The independent status-event stream is enabled
/// (`capture_status_events`) so the ACP terminal host can observe raw output +
/// exit off the renderer's queue, and shell integration is deliberately NOT
/// injected (this is a one-shot command the agent drives, not a shell that wants
/// prompt marks). Returns the shared backend + session id the caller mounts a
/// `TerminalView` on and polls for output.
pub fn spawn_embedded_command(
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
) -> Option<(SharedBackend, TerminalSessionId)> {
    let build_cfg = || SpawnConfig {
        shell: command.clone(),
        args: args.clone(),
        cwd: cwd.clone(),
        env: env.clone(),
        scrollback: spawn_scrollback(),
        capture_status_events: true,
        ..SpawnConfig::default()
    };
    if let Some(shared) = SHARED_BACKEND.get() {
        let _nap = crate::app_nap::prevent("acp embedded terminal spawn");
        let mut guard = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.spawn(build_cfg()) {
            Ok(session_id) => {
                drop(guard);
                return Some((Arc::clone(shared), session_id));
            }
            Err(err) => {
                drop(guard);
                tracing::warn!(?err, "relay embedded-terminal spawn failed; falling back");
            }
        }
    }
    let mut backend = PortablePtyBackend::new();
    let session_id = match backend.spawn(build_cfg()) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "embedded-terminal pty spawn failed");
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
        let mut guard = shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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

/// In-process dormant grid used as the paint-first restore placeholder:
/// zero daemon round-trips, no PTY child. The post-paint reconcile pass
/// (`project_panes_factory::spawn_attach_reconcile`) later swaps in the
/// real session via [`TerminalView::adopt_live_session`]. Deliberately
/// never touches `SHARED_BACKEND` — the whole point is that nothing here
/// can block on the relay socket.
pub fn spawn_pending_placeholder_grid() -> Option<(SharedBackend, TerminalSessionId)> {
    let mut backend = PortablePtyBackend::new();
    let session_id = match backend.spawn_dormant(DEFAULT_COLS, DEFAULT_ROWS) {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(?err, "pending placeholder spawn_dormant failed");
            return None;
        }
    };
    let boxed: Box<dyn TerminalBackend> = Box::new(backend);
    Some((Arc::new(Mutex::new(boxed)), session_id))
}

/// Mouse-selection granularity, chosen from the mouse-down click count.
/// `Char` = free cell drag, `Word` = double-click (whitespace/word-char
/// bounds), `Line` = triple-click (full visual row).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectKind {
    Char,
    Word,
    Line,
}

/// In-flight drag: the cell where the press began plus its granularity.
/// `selection` (the painted rect) is recomputed from `anchor` → current
/// cell on every move. Cleared on mouse-up.
struct SelectDrag {
    anchor: (usize, usize),
    kind: SelectKind,
}

/// Cap on retained shell-integration command marks — only recent prompts get
/// a gutter badge, and the oldest age out of the scrollback anyway.
const MAX_COMMAND_MARKS: usize = 256;

/// Cap on concurrent in-flight filesystem-stat tasks for link existence
/// checks. A fast Cmd-hover sweep across a row full of distinct `path:line`
/// tokens could otherwise fan out an unbounded burst of `exists()` probes; at
/// the cap, further misses are simply left un-recorded and retried on the next
/// hover (the per-path cache already collapses repeats).
const MAX_INFLIGHT_LINK_STATS: usize = 8;

/// A shell-integration command mark anchored to an absolute history line.
/// `exit` is `None` while the command runs and `Some(code)` once it finishes.
#[derive(Clone, Copy)]
struct CommandMark {
    line: u64,
    exit: Option<i32>,
}

/// Events a `TerminalView` raises to its host pane group. Today the only one
/// is a clean child exit (status 0): the group decides whether to auto-close
/// the hosting tab (a lone-view terminal tab) or leave the exit banner in
/// place (split / stacked panes). A non-zero or signalled exit is NOT emitted
/// — it always keeps the banner so the failure stays on screen.
#[derive(Clone, Copy)]
pub enum TerminalViewEvent {
    CleanExit { session_id: TerminalSessionId },
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

pub struct TerminalView {
    /// `Arc<Mutex<Box<dyn TerminalBackend>>>` — shared with whoever spawned
    /// the session. For local terminals the renderer is the only holder;
    /// for agent tabs `CliRuntime` holds the same Arc inside its
    /// `SessionEntry` and the poll task races on this lock. Lock window is
    /// the duration of a single non-blocking trait call.
    backend: SharedBackend,
    session_id: TerminalSessionId,
    /// Latest grid snapshot, refreshed at tick time only when output /
    /// resize events arrived. `Arc` so the per-frame `PaintParams` build
    /// is a pointer bump — never a full-grid deep clone.
    snapshot: Arc<TerminalSnapshot>,
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
    /// Sub-line wheel accumulator (pixels). Trackpad gestures deliver fractions
    /// of a line per event; rounding each one to whole lines would drop every
    /// delta below one line, so a slow scroll would move nothing. We accumulate
    /// here and emit whole-line scrolls as the carry crosses a line boundary,
    /// keeping the remainder for the next event.
    scroll_px: f32,
    /// `Some` while the overlay scrollbar thumb is being dragged. Holds the drag
    /// anchor so each mouse-move maps absolute cursor travel to a display offset.
    scrollbar_drag: Option<ScrollbarDrag>,
    /// Mirrors `focus_handle.is_focused(window)` for the blink task, which
    /// runs in a `Context<Self>`-only closure with no `&Window` access. Kept
    /// in sync via `cx.on_focus` / `cx.on_blur` observers registered in
    /// `mount`. When `false`, the blink task skips `cx.notify()` so unfocused
    /// panes don't burn a repaint every 530 ms.
    focused: bool,
    /// Whether this view's body is currently on screen (the active tab of its
    /// group, in the active leaf of any split). Pushed down by the owning
    /// `PaneGroup` each render. Hidden tabs still drain their PTY (so the
    /// snapshot stays current and device-query replies / bells are answered)
    /// but poll on a slower cadence and skip the repaint for plain output —
    /// the win that keeps tab switching fast under many concurrent agents.
    /// Defaults to `true` so a view always polls fast until told otherwise.
    visible: bool,
    /// Set when an UNFOCUSED pane raises a signal — today a terminal BEL
    /// (`TerminalEvent::Bell`); later also agent WaitingForInput/NeedsApproval
    /// and `oximux notify`. Drives the blue attention ring overlay; cleared
    /// when the pane gains focus (`on_focus`).
    attention: bool,
    /// Per-pane search overlay state. See `terminal_search_state.rs` for
    /// the state machine + key dispatch. The view owns I/O (grid fetch +
    /// `cx.notify`) and delegates everything else.
    search: SearchState,
    /// Monotonic generation for keystroke-debounced search reruns: each
    /// query edit bumps it and arms a short timer; only the timer whose
    /// generation is still current rescans, so fast typing never clones
    /// the full scrollback grid per keystroke.
    search_debounce_gen: u64,
    /// Monotonic generation for the OFF-THREAD search-grid clone. Each
    /// `rerun_search` bumps it; a clone that finishes out of order (a slower
    /// large-grid fetch landing after a newer one) is dropped instead of
    /// applying stale matches. Decouples the lock-held grid copy from the
    /// GPUI main thread so a big scrollback search never stalls the relay
    /// reader.
    search_scan_gen: u64,
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
    /// Event-driven drain task: wakes on a backend output signal (relay pump)
    /// and drains on arrival, so echo paints ~one frame after the bytes exist
    /// instead of waiting for the macOS-throttled poll timer. `None` for backends
    /// that don't signal (in-process / dormant) — those fall back to the poll.
    _output_drain_task: Option<Task<()>>,
    /// Frames to keep self-scheduling after a keystroke. The run loop can doze
    /// between sparse keystrokes; a straggler echo that lands just after a frame
    /// then waits for the next wake. Each keystroke sets this, and `render`
    /// re-requests a frame while it's positive, so the drain runs every frame
    /// for a short window — guaranteeing the echo paints within ~one frame of
    /// arriving. Decrements to zero (idle) so it costs nothing when not typing.
    drain_frames: u8,
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
    /// In-progress IME composition ("marked"/preedit) text, e.g. the Roman
    /// letters shown underlined while the OS input method composes a
    /// Vietnamese Telex syllable or a CJK character. `None` when not
    /// composing. Set/cleared by the platform input handler
    /// (`TerminalInputHandler`); the committed result arrives separately and
    /// is written to the PTY. Rendered as an underlined overlay at the cursor.
    ime_marked: Option<String>,
    /// Canvas bounds (window coords) captured by the paint closure each
    /// frame. Read by the mouse handlers to map a pixel position back to a
    /// cell via `point_to_cell`. Shared via `Rc<Cell<_>>` for the same
    /// reason as `canvas_grid`: the paint closure must not re-borrow the
    /// entity mid-paint.
    canvas_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// In-flight mouse drag-select, `None` when no button is held. See
    /// [`SelectDrag`].
    selecting: Option<SelectDrag>,
    /// Host pane group, used to open an editor tab when the user Cmd-clicks a
    /// `path:line:col` link. Weak so the terminal never keeps the group alive.
    /// `None` until `set_opener` runs (e.g. headless test mounts).
    opener: Option<WeakEntity<PaneGroup>>,
    /// `(row, col_start, col_end)` of the link under the pointer while Cmd is
    /// held, so the canvas can underline it. Cleared when the pointer leaves a
    /// link or Cmd is released.
    hovered_link: Option<(usize, usize, usize)>,
    /// Last bell-banner dispatch instant, rate-limiting BEL storms (a
    /// misbehaving child can emit thousands per second) to one banner
    /// request per window. The dispatcher's per-workspace burst gate
    /// collapses further, but that one is shared across panes -- this
    /// keeps a single pane from monopolizing it.
    last_bell_banner: Option<std::time::Instant>,
    /// Async path-existence answers for detected `path:line` links, keyed by
    /// resolved absolute path. Hover/paint/click consult it without touching
    /// the filesystem; misses spawn a background stat (see
    /// [`Self::path_link_ready`]). Underline and open both require a
    /// confirmed `Exists`, which is what keeps version-string-shaped false
    /// positives from ever lighting up.
    link_exists: ExistenceCache,
    /// Count of filesystem-stat tasks currently in flight for link existence
    /// checks. Bounded by [`MAX_INFLIGHT_LINK_STATS`] so a fast hover sweep
    /// can't spawn an unbounded burst; incremented before each spawn,
    /// decremented when it lands.
    link_stat_inflight: usize,
    /// Shell-integration command marks (OSC 133/633), newest last. Each holds
    /// the absolute history line of its prompt and the exit code once the
    /// command finishes — the canvas paints a gutter badge (red on non-zero).
    /// Bounded to the most recent `MAX_COMMAND_MARKS`.
    command_marks: Vec<CommandMark>,
    /// Latest OSC 9;4 progress `(state, value)` the child reported, if any.
    /// Surfaced for a future progress affordance; an error/warning state also
    /// raises pane attention when unfocused.
    progress: Option<(u8, u8)>,
    /// Stable identity triple (workspace / surface / tab). Injected into
    /// the spawn env as `OXIMUX_*`, persisted alongside the pane layout,
    /// and re-injected verbatim when a dormant pane respawns its shell so
    /// the ids survive an app quit -> reattach for the same surface.
    ids: SurfaceIds,
    /// True while this view awaits the post-paint attach reconcile: it
    /// renders an empty placeholder grid (no daemon round-trips happened
    /// at construction) until [`adopt_live_session`](Self::adopt_live_session)
    /// swaps in the real relay/spawned session. Input is dropped while
    /// pending — the shell it would reach doesn't exist yet.
    pending_attach: bool,
    /// The persisted relay PTY id this slot pointed at, carried so a
    /// quit-save that fires mid-reconcile re-persists the hint instead of
    /// dropping the row (which would orphan the daemon PTY on next boot).
    /// Read only by [`relay_id_for_capture`](Self::relay_id_for_capture).
    pending_relay_hint: Option<String>,
    /// `Some(code)` once the PTY's child process exits (`TerminalEvent::Exit`);
    /// `None` while it runs. Drives the "process exited" banner so a pane whose
    /// leader has died — e.g. a program run with `exec`, leaving no shell to
    /// fall back to — reads as finished rather than hung instead of freezing on
    /// its final frame. Cleared whenever the view is made live again
    /// ([`adopt_live_session`](Self::adopt_live_session) /
    /// [`respawn_if_dormant`](Self::respawn_if_dormant)).
    exited: Option<i32>,
    /// Consumes the OSC-9999 status sideband the global hooks emit into THIS
    /// terminal's output. A hand-typed `claude`/`codex`/… in a plain terminal
    /// has no `AgentRuntime` to decode its hook packets; this gives such an
    /// ambient agent the same stable, hook-driven status as a spawned one. Read
    /// by [`ambient_agent`](Self::ambient_agent); idle for a plain shell.
    agent_scan: crate::shell::ambient_agent_scan::AmbientAgentScan,
    /// Last ambient reading written to the persistent per-PTY store, so an
    /// unchanged status doesn't re-hit SQLite every output frame. `None` until
    /// the first agent reading (a plain shell never writes).
    last_persisted_ambient: Option<crate::shell::ambient_agent_scan::AmbientSideband>,
    /// Names the agent CLI running in this terminal by walking the shell's
    /// process tree. The sideband above covers only the CLI OxiMux installs
    /// hooks for, and both it and the title are events — this is the presence
    /// signal that holds while an agent sits idle. Read by
    /// [`agent_process`](Self::agent_process).
    proc_scan: crate::shell::agent_process_scan::AgentProcessScan,
}

mod input;
mod lifecycle;
mod render;
mod state;

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
                // Per-session drain (NOT the global `drain_events`): on the
                // shared relay backend a global drain would also consume
                // OTHER sessions' queued events — including the one-shot
                // synthetic Exits seeded after a daemon-crash backend swap,
                // silently starving an agent status poller of its
                // termination signal. The unblock effect for THIS session's
                // teardown is identical.
                let _ = be.drain_events_for(id);
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

/// Map a GPUI mouse button to a reportable terminal button. Returns `None`
/// for buttons the mouse protocol doesn't encode (back/forward/etc.).
fn map_btn(button: MouseButton) -> Option<MouseBtn> {
    match button {
        MouseButton::Left => Some(MouseBtn::Left),
        MouseButton::Middle => Some(MouseBtn::Middle),
        MouseButton::Right => Some(MouseBtn::Right),
        _ => None,
    }
}

/// Order two cell points into a normalized `(start_row, start_col, end_row,
/// end_col)` rectangle in reading order. `extract_selection_text` and the
/// canvas overlay both expect start ≤ end, so the drag handler normalizes
/// here regardless of drag direction.
fn order_points(a: (usize, usize), b: (usize, usize)) -> (usize, usize, usize, usize) {
    if a <= b {
        (a.0, a.1, b.0, b.1)
    } else {
        (b.0, b.1, a.0, a.1)
    }
}

/// Inclusive column span of the word at `col`. A word is a run of
/// alphanumeric or `_` cells; any other glyph (whitespace, punctuation,
/// `\0` blanks) yields a single-cell span.
fn word_range_at(row: &[oximux_pty::Cell], col: usize) -> (usize, usize) {
    if col >= row.len() {
        return (col, col);
    }
    let is_word = |i: usize| {
        let ch = row[i].ch;
        ch.is_alphanumeric() || ch == '_'
    };
    if !is_word(col) {
        return (col, col);
    }
    let mut start = col;
    while start > 0 && is_word(start - 1) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < row.len() && is_word(end + 1) {
        end += 1;
    }
    (start, end)
}

/// Extract the text covered by a cell-coordinate selection from the
/// current snapshot. End coords are inclusive. Each row is right-trimmed
/// of trailing whitespace then joined with `\n` — matching the
/// common terminal "copy preserves visual newlines but not visual
/// padding" convention. Out-of-range coordinates clamp silently to grid.
fn extract_selection_text(
    snapshot: &TerminalSnapshot,
    sel: (usize, usize, usize, usize),
) -> String {
    extract_selection_text_cells(&snapshot.cells, sel)
}

/// Like [`extract_selection_text`] but over a bare row-major cell grid — used
/// to copy a full-scrollback selection (Cmd+A / "Select All") and to extract
/// the last command's output from the history grid, neither of which fits in
/// the visible snapshot.
fn extract_selection_text_cells(
    rows: &[Vec<oximux_pty::Cell>],
    (start_row, start_col, end_row, end_col): (usize, usize, usize, usize),
) -> String {
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
            // Wide-char spacers carry no real character — their column is
            // already painted by the adjacent wide glyph. Skip them so copy
            // yields one char per glyph instead of "char + space".
            if cell.wide_spacer {
                continue;
            }
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

/// Platform IME bridge for a terminal surface. Built and registered every
/// paint (via `window.handle_input`) while the pane is focused; macOS routes
/// composed and marked text through it (the `NSTextInputClient` protocol).
/// This is the path that makes multi-keystroke input methods work in the
/// terminal — Vietnamese Telex, CJK, and dead keys — which a bare
/// `on_key_down`-to-bytes pipeline cannot do. `on_key_down` deliberately
/// defers plain text to it. Returning `false` from `apple_press_and_hold_enabled`
/// also disables the macOS press-and-hold accent popup, giving the terminal
/// raw key-repeat (the popup otherwise stalls held keys, which reads as lag).
struct TerminalInputHandler {
    view: Entity<TerminalView>,
    /// Cursor cell bounds in window coordinates, baked at paint time, used to
    /// anchor the OS candidate window at the caret.
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        // Disable the IME on the alt-screen (full-screen TUIs — vim, less,
        // htop — must read keys raw). Off the alt-screen, present a
        // zero-length selection at the caret so the OS routes composition here.
        let view = self.view.read(cx);
        let sid = view.session_id;
        let alt_screen = view.with_backend(|be| be.mouse_mode(sid).alt_screen);
        if alt_screen {
            None
        } else {
            Some(UTF16Selection {
                range: 0..0,
                reversed: false,
            })
        }
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        let len = self
            .view
            .read(cx)
            .ime_marked
            .as_ref()?
            .encode_utf16()
            .count();
        Some(0..len)
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        // The terminal exposes no queryable text buffer to the IME.
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let text = text.to_owned();
        self.view
            .update(cx, |view, cx| view.commit_ime_text(&text, cx));
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let new_text = new_text.to_owned();
        self.view
            .update(cx, |view, cx| view.set_ime_marked(new_text, cx));
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, cx| view.clear_ime_marked(cx));
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        // Shift the candidate window by the composition offset; one cell width
        // is `bounds.size.width` (the cursor cell).
        bounds.origin.x += bounds.size.width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

/// F3.4 slice 3: tiny corner chip indicating a restored-dormant sub-pane.
/// Renders as an absolutely-positioned tag in the top-right of the pane
/// body. Click/focus anywhere in the pane already wakes it via the
/// `on_focus` observer wired in `mount_dormant`, so the badge is purely
/// informational — no click handler needed.
fn build_dormant_badge(theme: &Theme, density: Density, typo: &Typography) -> gpui::Div {
    div()
        .absolute()
        .top(px(6.0))
        .right(px(10.0))
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_overlay)
        .text_color(theme.fg_muted)
        .text_size(px(typo.t_body_sm))
        .border_1()
        .border_color(theme.border_inactive)
        // `↻` = U+21BB Clockwise Open Circle Arrow. Hint copy mirrors
        // the dormancy contract: the shell isn't running yet; first
        // focus or keystroke wakes it at the saved cwd.
        .child("↻ restored — click to wake")
}

/// Centered top strip shown once a pane's PTY child has exited. Marks the
/// pane as finished (not hung) and points the user at the close shortcut —
/// this is a terminating affordance, so there is no inline restart. Exit code
/// `0` reads as a clean end; any non-zero code is shown so a crash is visible.
fn build_exit_banner(theme: &Theme, code: i32, density: Density, typo: &Typography) -> gpui::Div {
    // `0` = clean, `>0` = real status, `<0` = signal/unknown (the `-1`
    // sentinel from a `None` exit code) where no number is meaningful.
    let label = if code > 0 {
        format!("process exited (code {code})")
    } else {
        "process exited".to_string()
    };
    div()
        .absolute()
        .top(px(6.0))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .px(px(10.0))
                .py(px(3.0))
                .rounded(px(density.r_xs))
                .bg(theme.bg_overlay)
                .text_color(theme.fg_muted)
                .text_size(px(typo.t_body_sm))
                .border_1()
                .border_color(theme.border_inactive)
                // `⏻` = U+23FB Power Symbol.
                .child(format!("⏻ {label} · ⌘W to close")),
        )
}

/// A keyboard scrollback command, resolved from a keystroke by
/// [`scroll_key_command`] and applied by `TerminalView::handle_scroll_command`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScrollCmd {
    PageUp,
    PageDown,
    /// Jump to the live tail (bottom).
    Tail,
}

/// Classify a keystroke as a local-scrollback command, or `None` to let it
/// reach the PTY. Bindings: plain PageUp/PageDown page the viewport; Cmd+Up
/// pages up (matches mature terminals); Cmd+Down jumps to the tail. Ctrl/Alt
/// are excluded so app chords (Ctrl+Up word-nav, etc.) pass through untouched.
fn scroll_key_command(ks: &gpui::Keystroke) -> Option<ScrollCmd> {
    let m = &ks.modifiers;
    if m.control || m.alt {
        return None;
    }
    match ks.key.as_str() {
        "pageup" if !m.platform => Some(ScrollCmd::PageUp),
        "pagedown" if !m.platform => Some(ScrollCmd::PageDown),
        "up" if m.platform => Some(ScrollCmd::PageUp),
        "down" if m.platform => Some(ScrollCmd::Tail),
        _ => None,
    }
}

/// Fold a pixel delta into the sub-line accumulator and return the whole lines
/// to scroll, keeping the fractional remainder in `*acc`. This carry is what
/// lets a slow trackpad drag — each event under one line tall — eventually
/// advance the viewport instead of every delta truncating to zero and being
/// dropped. A non-positive `line_height` is a no-op guard against div-by-zero.
fn accumulate_scroll_lines(acc: &mut f32, delta_px: f32, line_height: f32) -> i32 {
    if line_height <= 0.0 {
        return 0;
    }
    *acc += delta_px;
    let lines = (*acc / line_height).trunc() as i32;
    *acc -= lines as f32 * line_height;
    lines
}

/// Faint top-right chip shown while the viewport is scrolled up off the live
/// tail (`display_offset > 0`). Click it to jump back to the bottom; any
/// keystroke also snaps down (`send_bytes` → `scroll_to_bottom`). The caller
/// wires the click handler — this only builds the chip + pointer affordance.
fn build_scroll_indicator(
    theme: &Theme,
    offset: usize,
    density: Density,
    typo: &Typography,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("oximux-scroll-to-tail")
        .absolute()
        .top(px(6.0))
        .right(px(10.0))
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_overlay)
        .text_color(theme.fg_muted)
        .text_size(px(typo.t_body_sm))
        .border_1()
        .border_color(theme.border_inactive)
        .cursor_pointer()
        // `↑` = U+2191 Upwards Arrow. `⤓` hints the click jumps to the tail.
        .child(format!("↑ {offset} lines · ⤓"))
}

#[cfg(test)]
mod poll_interval_tests {
    use super::*;

    #[test]
    fn hidden_tab_polls_slower_than_visible() {
        assert_eq!(poll_interval_ms(true), POLL_INTERVAL_MS);
        assert_eq!(poll_interval_ms(false), BACKGROUND_POLL_INTERVAL_MS);
        assert!(poll_interval_ms(false) > poll_interval_ms(true));
    }
}

#[cfg(test)]
mod scroll_key_tests {
    use super::{ScrollCmd, scroll_key_command};
    use gpui::Keystroke;

    fn cmd(chord: &str) -> Option<ScrollCmd> {
        scroll_key_command(&Keystroke::parse(chord).expect("valid chord"))
    }

    #[test]
    fn plain_page_keys_scroll_pages() {
        assert_eq!(cmd("pageup"), Some(ScrollCmd::PageUp));
        assert_eq!(cmd("pagedown"), Some(ScrollCmd::PageDown));
    }

    #[test]
    fn cmd_up_pages_up_and_cmd_down_jumps_to_tail() {
        assert_eq!(cmd("cmd-up"), Some(ScrollCmd::PageUp));
        assert_eq!(cmd("cmd-down"), Some(ScrollCmd::Tail));
    }

    #[test]
    fn plain_arrows_and_app_chords_pass_through() {
        // Bare arrows belong to the shell/app, not scrollback.
        assert_eq!(cmd("up"), None);
        assert_eq!(cmd("down"), None);
        // Ctrl/Alt arrow combos are app navigation — never intercepted.
        assert_eq!(cmd("ctrl-up"), None);
        assert_eq!(cmd("alt-up"), None);
        // Cmd+PageUp isn't one of ours either (plain PageUp is).
        assert_eq!(cmd("cmd-pageup"), None);
    }
}

#[cfg(test)]
mod scroll_accumulator_tests {
    use super::accumulate_scroll_lines;

    // A run of sub-line nudges must eventually advance a line — the regression
    // this guards is "slow trackpad scroll moves nothing" (each delta < 1 line
    // truncating to zero). Four 6px nudges over a 20px line = 24px → one line,
    // 4px carried.
    #[test]
    fn subline_deltas_carry_into_a_whole_line() {
        let mut acc = 0.0;
        let lh = 20.0;
        assert_eq!(accumulate_scroll_lines(&mut acc, 6.0, lh), 0);
        assert_eq!(accumulate_scroll_lines(&mut acc, 6.0, lh), 0);
        assert_eq!(accumulate_scroll_lines(&mut acc, 6.0, lh), 0);
        assert_eq!(accumulate_scroll_lines(&mut acc, 6.0, lh), 1);
        assert!((acc - 4.0).abs() < 1e-3, "remainder kept for next event");
    }

    // A fast flick crosses several lines in one event; the remainder still
    // carries so cadence stays smooth.
    #[test]
    fn large_delta_emits_multiple_lines() {
        let mut acc = 0.0;
        assert_eq!(accumulate_scroll_lines(&mut acc, 65.0, 20.0), 3);
        assert!((acc - 5.0).abs() < 1e-3);
    }

    // Negative deltas scroll toward the tail and carry symmetrically.
    #[test]
    fn negative_deltas_scroll_toward_tail() {
        let mut acc = 0.0;
        assert_eq!(accumulate_scroll_lines(&mut acc, -50.0, 20.0), -2);
        assert!((acc + 10.0).abs() < 1e-3);
    }

    // Defensive: a zero/garbage line height never divides by zero.
    #[test]
    fn nonpositive_line_height_is_a_noop() {
        let mut acc = 7.0;
        assert_eq!(accumulate_scroll_lines(&mut acc, 100.0, 0.0), 0);
        assert_eq!(acc, 7.0);
    }
}

/// Restore lifecycle: a tab reconnected on app reopen must come back
/// responsive. These pin the placeholder → live-session handoff and the
/// visibility-driven drain cadence so a restored terminal never gets stuck
/// at the slow background poll rate (the cause of laggy post-restore typing).
#[cfg(test)]
mod restore_lifecycle_tests {
    use super::*;
    use gpui::TestAppContext;

    fn dormant() -> (SharedBackend, TerminalSessionId) {
        spawn_local_pty_dormant(80, 24).expect("dormant spawn (PTY fallback)")
    }

    fn restored_ids() -> SurfaceIds {
        SurfaceIds::restored("/proj".to_string(), "surface-1".to_string(), "tab-1".to_string())
    }

    fn mount_pending_view(cx: &mut TestAppContext) -> gpui::WindowHandle<TerminalView> {
        let (backend, sid) = dormant();
        cx.add_window(|win, cx| {
            TerminalView::mount_pending(
                backend,
                sid,
                restored_ids(),
                None,
                Theme::default(),
                Density::default(),
                Typography::default(),
                win,
                cx,
            )
        })
    }

    // A pending restore placeholder is fast-poll eligible the moment it mounts
    // (`visible == true`), so once the reconcile delivers the live session the
    // restored tab drains at ~60 fps rather than the background cadence.
    #[gpui::test]
    async fn pending_restore_view_is_fast_poll_eligible(cx: &mut TestAppContext) {
        let window = mount_pending_view(cx);
        cx.run_until_parked();

        cx.read(|app| {
            let v = window.read(app).expect("view alive");
            assert!(v.is_pending_attach(), "fresh placeholder must be pending");
            assert!(v.is_visible(), "placeholder defaults visible → polls fast");
            assert_eq!(poll_interval_ms(v.is_visible()), POLL_INTERVAL_MS);
        });
    }

    // `adopt_live_session` swaps the placeholder for the live session, clears
    // the pending flag, keeps the view fast-poll eligible, and succeeds exactly
    // once — a second (duplicate) delivery is a no-op so it can't corrupt state
    // or double-arm the drain task.
    #[gpui::test]
    async fn adopt_live_session_clears_pending_and_is_idempotent(cx: &mut TestAppContext) {
        let window = mount_pending_view(cx);
        cx.run_until_parked();

        let (live_backend, live_sid) = dormant();
        let adopted = window
            .update(cx, |view, _win, cx| {
                view.adopt_live_session(live_backend, live_sid, cx)
            })
            .expect("window update");
        assert!(adopted, "first adopt must succeed");

        // Second adopt on a now-live view is rejected (not pending anymore).
        let (other_backend, other_sid) = dormant();
        let twice = window
            .update(cx, |view, _win, cx| {
                view.adopt_live_session(other_backend, other_sid, cx)
            })
            .expect("window update");
        assert!(!twice, "second adopt on a live view must be a no-op");

        cx.run_until_parked();
        cx.read(|app| {
            let v = window.read(app).expect("view alive");
            assert!(!v.is_pending_attach(), "adopt must clear pending");
            assert_eq!(v.session_id(), live_sid, "adopt must swap to the live session");
            assert!(v.is_visible(), "adopted view stays fast-poll eligible");
            assert_eq!(poll_interval_ms(v.is_visible()), POLL_INTERVAL_MS);
        });
    }

    // A selection / hover painted over the pending placeholder grid must NOT
    // survive the swap to the live session — otherwise stale interaction state
    // leaks onto unrelated content. `adopt_live_session` clears it.
    #[gpui::test]
    async fn adopt_clears_stale_selection_and_hover(cx: &mut TestAppContext) {
        let window = mount_pending_view(cx);
        cx.run_until_parked();

        // Paint placeholder-pane interaction state.
        window
            .update(cx, |view, _win, _cx| {
                view.selection = Some((0, 0, 1, 1));
                view.selecting = Some(SelectDrag {
                    anchor: (0, 0),
                    kind: SelectKind::Char,
                });
                view.hovered_link = Some((0, 0, 3));
            })
            .expect("window update");

        let (live_backend, live_sid) = dormant();
        let adopted = window
            .update(cx, |view, _win, cx| {
                view.adopt_live_session(live_backend, live_sid, cx)
            })
            .expect("window update");
        assert!(adopted, "adopt must succeed on a pending view");

        cx.run_until_parked();
        cx.read(|app| {
            let v = window.read(app).expect("view alive");
            assert!(v.selection.is_none(), "adopt clears the stale selection");
            assert!(v.selecting.is_none(), "adopt clears the in-flight drag");
            assert!(v.hovered_link.is_none(), "adopt clears the stale hover");
        });
    }

    // Visibility flips drive the PTY-drain cadence: an on-screen restored tab
    // drains at the foreground rate (low echo latency); a backgrounded one
    // throttles. This is the invariant that keeps typing into the focused
    // restored terminal responsive while idle background agents don't each
    // schedule a 60 fps repaint.
    #[gpui::test]
    async fn visibility_drives_poll_cadence(cx: &mut TestAppContext) {
        let window = mount_pending_view(cx);
        cx.run_until_parked();

        window
            .update(cx, |view, _win, cx| view.set_visible(false, cx))
            .expect("window update");
        cx.read(|app| {
            let v = window.read(app).expect("view alive");
            assert!(!v.is_visible());
            assert_eq!(poll_interval_ms(v.is_visible()), BACKGROUND_POLL_INTERVAL_MS);
        });

        window
            .update(cx, |view, _win, cx| view.set_visible(true, cx))
            .expect("window update");
        cx.read(|app| {
            let v = window.read(app).expect("view alive");
            assert!(v.is_visible());
            assert_eq!(poll_interval_ms(v.is_visible()), POLL_INTERVAL_MS);
        });
    }
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
            ..Cell::default()
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
            display_offset: 0,
            cursor_shape: oximux_pty::CursorShapeKind::Block,
            history_len: 0,
            links: Vec::new(),
        }
    }

    #[test]
    fn word_range_selects_alphanumeric_run() {
        let s = snap(&["foo bar_baz!"]);
        let row = &s.cells[0];
        // "foo" = cols 0..=2.
        assert_eq!(word_range_at(row, 1), (0, 2));
        // space at col 3 → single cell.
        assert_eq!(word_range_at(row, 3), (3, 3));
        // "bar_baz" = cols 4..=10 (underscore is a word char).
        assert_eq!(word_range_at(row, 5), (4, 10));
        // '!' at col 11 → single cell.
        assert_eq!(word_range_at(row, 11), (11, 11));
    }

    #[test]
    fn word_range_selects_unicode_words() {
        // `char::is_alphanumeric()` already spans the full Unicode set, so a
        // double-click word selection grabs CJK and accented tokens whole —
        // not just ASCII. Guards against a regression to an ASCII-only
        // predicate (and documents that the boundary is already Unicode-aware).
        //
        // NB: this `snap()` helper lays one cell per char, so each CJK glyph
        // occupies a single cell. In the real grid a wide glyph spans two cells
        // (the second is a `\0` wide-spacer that correctly stops the run); this
        // test pins the predicate's Unicode-awareness, not double-width layout.
        let s = snap(&["café 你好 δοκιμή"]);
        let row = &s.cells[0];
        // "café" = cols 0..=3 (precomposed é is a word char).
        assert_eq!(word_range_at(row, 0), (0, 3));
        assert_eq!(word_range_at(row, 3), (0, 3));
        // space at col 4 → single cell.
        assert_eq!(word_range_at(row, 4), (4, 4));
        // "你好" = cols 5..=6 (CJK ideographs).
        assert_eq!(word_range_at(row, 5), (5, 6));
        // Greek "δοκιμή" = cols 8..=13.
        assert_eq!(word_range_at(row, 8), (8, 13));
        assert_eq!(word_range_at(row, 13), (8, 13));
    }

    #[test]
    fn order_points_normalizes_reading_order() {
        assert_eq!(order_points((1, 2), (3, 4)), (1, 2, 3, 4));
        assert_eq!(order_points((3, 4), (1, 2)), (1, 2, 3, 4));
        assert_eq!(order_points((0, 5), (0, 1)), (0, 1, 0, 5));
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
    fn extract_selection_collapses_wide_char_with_its_spacer() {
        // A row holding `a`, then a wide `你` paired with its spacer, then
        // `b`. The selection spans all four columns; the spacer must drop
        // out so the copied text reads `a你b` (3 chars), not `a你 b`.
        let mut wide = cell('你');
        wide.wide = true;
        let mut spacer = cell(' ');
        spacer.wide_spacer = true;
        let row = vec![cell('a'), wide, spacer, cell('b')];
        let snapshot = TerminalSnapshot {
            cols: 4,
            rows: 1,
            cursor: (0, 0),
            cells: vec![row],
            display_offset: 0,
            cursor_shape: oximux_pty::CursorShapeKind::Block,
            history_len: 0,
            links: Vec::new(),
        };
        let txt = extract_selection_text(&snapshot, (0, 0, 0, 3));
        assert_eq!(txt, "a你b");
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

    #[test]
    fn extract_cells_over_full_grid_band() {
        // The bare-grid extractor (used by full-scrollback Select All and the
        // send-last-output band) pulls a multi-row window with `usize::MAX`
        // clamping to each row's real width — capturing rows that live in the
        // history grid, outside the visible snapshot.
        let cells: Vec<Vec<Cell>> = ["old prompt", "output one", "output two", "new prompt"]
            .iter()
            .map(|r| r.chars().map(cell).collect::<Vec<_>>())
            .collect();
        // Band = the two output rows, full width.
        let txt = extract_selection_text_cells(&cells, (1, 0, 2, usize::MAX));
        assert_eq!(txt, "output one\noutput two");
        // Whole grid.
        let all = extract_selection_text_cells(&cells, (0, 0, 3, usize::MAX));
        assert_eq!(all, "old prompt\noutput one\noutput two\nnew prompt");
    }
}

#[cfg(test)]
mod poison_recovery_tests {
    use std::sync::{Arc, Mutex};

    // The terminal view's backend-lock sites recover a poisoned mutex via
    // `unwrap_or_else(|p| p.into_inner())` instead of `expect`, so a panic
    // while another holder had the lock can never cascade into a second panic
    // (which inside a spawn/Drop would abort the process). This pins the idiom.
    #[test]
    fn poisoned_mutex_recovers_via_into_inner() {
        let m = Arc::new(Mutex::new(42u32));
        let m2 = Arc::clone(&m);
        // Poison the mutex by panicking while it's held.
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(m.lock().is_err(), "mutex is poisoned after the panicking holder");
        // The recovery pattern must yield the guarded value, never panic.
        let v = *m.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(v, 42, "into_inner recovers the value past poison");
    }
}
