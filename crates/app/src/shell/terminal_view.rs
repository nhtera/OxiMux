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
    App, Bounds, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Point, Render, ScrollWheelEvent, Styled, Task, WeakEntity, Window, canvas, div, px,
};
use oximux_agents::SharedBackend;
use oximux_pty::{
    CommandMarkKind, PortablePtyBackend, SpawnConfig, TerminalBackend, TerminalEvent,
    TerminalSessionId, TerminalSnapshot,
};
use oximux_settings::{BellStyle, Density, TerminalSettings, Theme, Typography};

use crate::actions::{
    Search, SendLastCommandOutputToAgent, SendTerminalSelectionToAgent, SendTextToActiveAgent,
};
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::context_env::SurfaceIds;
use crate::shell::key_input::keystroke_to_bytes;
use crate::shell::mouse_report::{MouseAction, MouseBtn, encode_button, encode_scroll, mod_bits};
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_links::{LinkMatch, LinkTarget, detect_at};
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

/// PTY-drain cadence for a view given its on-screen state. Visible tabs drain
/// at ~60fps; hidden tabs drain far less often (output buffers, not dropped).
const fn poll_interval_ms(visible: bool) -> u64 {
    if visible {
        POLL_INTERVAL_MS
    } else {
        BACKGROUND_POLL_INTERVAL_MS
    }
}

/// Default grid size on spawn. Parent-driven resize takes over on the first
/// render.
pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_ROWS: u16 = 32;

/// Read the live terminal settings global, falling back to defaults when it
/// isn't installed (headless tests, early startup before `set_global`).
pub fn terminal_settings(cx: &App) -> TerminalSettings {
    cx.try_global::<TerminalSettings>().copied().unwrap_or_default()
}

/// Process-wide mirror of `TerminalSettings::scrollback_lines`. The PTY spawn
/// helpers are `cx`-less free functions, so they read scrollback here instead
/// of threading the global through every call site. The settings loader and
/// the live-reload watcher (both of which hold `cx`) keep it in sync.
static SPAWN_SCROLLBACK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(5000);

/// Update the spawn-scrollback mirror from settings. Called once at startup and
/// on every settings reload.
pub fn set_spawn_scrollback(lines: usize) {
    SPAWN_SCROLLBACK.store(lines, std::sync::atomic::Ordering::Relaxed);
}

fn spawn_scrollback() -> usize {
    SPAWN_SCROLLBACK.load(std::sync::atomic::Ordering::Relaxed)
}

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
    // Attach is a synchronous daemon round-trip on the calling thread
    // (background executor from the restore reconcile; main thread from
    // tear-off). Hold off App Nap for its duration.
    let _nap = crate::app_nap::prevent("relay attach");
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
        let cfg = SpawnConfig {
            cwd: cwd.clone(),
            env: env.clone(),
            cols,
            rows,
            scrollback: spawn_scrollback(),
            ..SpawnConfig::default()
        };
        // Spawn is a synchronous daemon round-trip on the calling thread
        // (background executor from the restore reconcile; main thread for
        // interactive new-tab/split spawns). Hold off App Nap so it can't
        // wedge mid-request.
        let _nap = crate::app_nap::prevent("relay spawn");
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
    spawn_fallback_portable(cwd, env, (cols, rows))
}

fn spawn_fallback_portable(
    cwd: PathBuf,
    env: Vec<(String, String)>,
    (cols, rows): (u16, u16),
) -> Option<(SharedBackend, TerminalSessionId)> {
    let mut backend = PortablePtyBackend::new();
    let cfg = SpawnConfig {
        cwd,
        env,
        cols,
        rows,
        scrollback: spawn_scrollback(),
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

/// A shell-integration command mark anchored to an absolute history line.
/// `exit` is `None` while the command runs and `Some(code)` once it finishes.
#[derive(Clone, Copy)]
struct CommandMark {
    line: u64,
    exit: Option<i32>,
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
}

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
        // owner and keystrokes are dropped until the first click. Restore
        // skips this (`grab_focus == false`) so building many split leaves
        // doesn't ping-pong focus before `focus_active` sets it once.
        if grab_focus {
            focus_handle.focus(window, cx);
        }

        let poll_task = Self::start_poll_task(cx);
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
            // Init to false; the on_focus callback fires for the focus() above
            // and flips this true for whichever pane actually wins focus.
            // Multiple panes constructed in the same effect run will each see
            // their focus() call land, last one wins, on_blur clears the rest.
            focused: false,
            visible: true,
            attention: false,
            search: SearchState::new(),
            title: None,
            dormant_cwd: None,
            _poll_task: Some(poll_task),
            _blink_task: blink_task,
            canvas_grid: Rc::new(Cell::new((DEFAULT_COLS, DEFAULT_ROWS))),
            canvas_bounds: Rc::new(Cell::new(Bounds::default())),
            selection: None,
            selecting: None,
            opener: None,
            hovered_link: None,
            command_marks: Vec::new(),
            progress: None,
            pending_attach: false,
            pending_relay_hint: None,
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
        self.snapshot = self
            .backend
            .lock()
            .expect("pty backend mutex poisoned")
            .snapshot(session_id)
            .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS));
        // The session was attached/spawned at its own size; (0,0) can never
        // equal a real canvas grid, so the next render always resizes it to
        // the painted bounds.
        self.last_resize = (0, 0);
        self._poll_task = Some(Self::start_poll_task(cx));
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
            ids,
            snapshot,
            theme,
            density,
            typography,
            focus_handle,
            target_grid: (DEFAULT_COLS, DEFAULT_ROWS),
            last_resize: (DEFAULT_COLS, DEFAULT_ROWS),
            cursor_visible: true,
            focused: false,
            visible: true,
            attention: false,
            search: SearchState::new(),
            title: None,
            dormant_cwd: Some(cwd),
            _poll_task: None,
            _blink_task: blink_task,
            canvas_grid: Rc::new(Cell::new((DEFAULT_COLS, DEFAULT_ROWS))),
            canvas_bounds: Rc::new(Cell::new(Bounds::default())),
            selection: None,
            selecting: None,
            opener: None,
            hovered_link: None,
            command_marks: Vec::new(),
            progress: None,
            pending_attach: false,
            pending_relay_hint: None,
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

    /// Push the on-screen state down from the owning `PaneGroup`. Becoming
    /// visible repaints immediately so the just-shown tab reflects any output
    /// that landed while it was throttled; the poll loop picks up the faster
    /// cadence on its next iteration.
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

    /// Relay id to PERSIST for this pane. Same as [`external_id`]
    /// (Self::external_id) for live views, but a still-pending view
    /// answers with its persisted hint — so a quit-save racing the
    /// post-paint reconcile re-persists the original row instead of
    /// dropping it (which would orphan the daemon PTY: alive, but no
    /// row points at it on the next boot).
    pub fn relay_id_for_capture(&self) -> Option<String> {
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

    pub(crate) fn on_search(&mut self, _: &Search, _window: &mut Window, cx: &mut Context<Self>) {
        self.search.open();
        self.rerun_search();
        cx.notify();
    }

    /// Cmd-Shift-I: extract the active selection's text and dispatch a
    /// `SendTextToActiveAgent` payload action up the tree. `WorkspaceRoot`
    /// resolves the destination agent and writes the bytes via the CLI
    /// runtime. No-op (with a debug trace) when the pane has no selection.
    fn on_send_selection_to_agent(
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
    fn on_send_last_command_output_to_agent(
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
    /// not wired) or when both marks lie above the visible viewport.
    ///
    /// The output band is `[prev_prompt.line + 1, last_prompt.line - 1]`
    /// in absolute history coords; both ends clamp into the snapshot's
    /// visible rows via `abs_line_to_screen_row`. Output that scrolled
    /// off the top is silently truncated to what's still on screen — a
    /// known limitation of the v1 viewport-only extractor.
    fn last_completed_command_output(&self) -> Option<String> {
        let n = self.command_marks.len();
        if n < 2 {
            return None;
        }
        let prev = &self.command_marks[n - 2];
        let last = &self.command_marks[n - 1];
        // Output band is exclusive of both prompt lines themselves.
        let band_start = prev.line.saturating_add(1);
        let band_end = last.line.saturating_sub(1);
        if band_end < band_start {
            return None;
        }
        // Clamp the band into the visible viewport. If the band is
        // entirely above or below the viewport, nothing to extract.
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
        Some(self.snapshot.rows_text(screen_start, screen_end))
    }

    fn rerun_search(&mut self) {
        let session_id = self.session_id;
        let grid = self.with_backend(|be| be.search_grid(session_id));
        let visible = self.snapshot.cells.len();
        self.search.rerun(&grid, visible);
    }

    /// Map a window-space pointer position to a `(row, col)` cell, clamped to
    /// the live grid, using the bounds captured by the last paint.
    fn cell_at(&self, pos: Point<Pixels>, window: &Window) -> (usize, usize) {
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
    fn on_select_down(&mut self, ev: &MouseDownEvent, window: &mut Window) {
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
    fn apply_drag(&mut self, current: (usize, usize)) {
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
    /// plain click does not paint a one-cell selection).
    fn finish_select(&mut self) -> bool {
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
        true
    }

    /// Forward a mouse event to the child when the app has enabled mouse
    /// reporting and Shift is NOT held (Shift is the escape hatch for local
    /// selection over a mouse-mode app). Returns `true` when it consumed the
    /// event, so the caller skips local selection.
    fn report_mouse(
        &mut self,
        button: MouseButton,
        pos: Point<Pixels>,
        modifiers: &Modifiers,
        action: MouseAction,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
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

    /// Resolve a possibly-relative link path against the session's OSC 7 cwd,
    /// falling back to the path as-is when no cwd is known.
    fn resolve_path(&mut self, path: &std::path::Path) -> PathBuf {
        if path.is_absolute() {
            return path.to_path_buf();
        }
        let id = self.session_id;
        match self.with_backend(|be| be.cwd_hint(id)) {
            Some(cwd) => cwd.join(path),
            None => path.to_path_buf(),
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
    fn try_open_link(
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
        self.open_link(hit.target, window, cx);
        true
    }

    /// Re-check the hovered link against the current snapshot after a refresh:
    /// keep it (updating the span) if a link still sits at its start cell,
    /// else drop it. Avoids underlining stale content after the grid changes
    /// while also not flickering off a still-valid link during streaming output.
    fn revalidate_hover(&mut self) {
        if let Some((row, c0, _)) = self.hovered_link {
            self.hovered_link = self
                .link_at(row, c0)
                .map(|hit| (row, hit.col_start, hit.col_end));
        }
    }

    /// Update the Cmd-hover link underline. Called on mouse-move; clears the
    /// highlight when Cmd isn't held or the pointer isn't over a link.
    fn update_hover(&mut self, ev: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let next = if ev.modifiers.platform {
            let (row, col) = self.cell_at(ev.position, window);
            self.link_at(row, col)
                .map(|hit| (row, hit.col_start, hit.col_end))
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
    fn on_wheel(&mut self, ev: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let metrics = CellMetrics::measure(&self.typography, window);
        let dy = f32::from(ev.delta.pixel_delta(px(metrics.line_height)).y);
        let mult = terminal_settings(cx).scroll_multiplier;
        let lines = (dy / metrics.line_height * mult).round() as i32;
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

        if let Err(err) = self.with_backend(|be| be.scroll(id, lines)) {
            tracing::warn!(?err, "pty scroll failed");
        }
        cx.notify();
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

        // Read DECCKM (app-cursor) live so cursor keys pick CSI vs SS3; apps
        // toggle it dynamically, so fetch per keystroke rather than caching.
        let session_id = self.session_id;
        let mode = self.with_backend(|be| be.input_mode(session_id));
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
            // Re-inject the SAME context ids on the inline wake path too.
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
        let mut got_bell = false;
        let mut latest_title: Option<String> = None;
        let mut clipboard_text: Option<String> = None;
        let mut pty_replies: Vec<Vec<u8>> = Vec::new();
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
                // A BEL while this pane is NOT focused raises attention (unless
                // the bell is disabled). A bell in the pane you're already
                // looking at is just noise.
                TerminalEvent::Bell { .. }
                    if !self.focused && settings.bell != BellStyle::Off =>
                {
                    got_bell = true
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
        let session_id = self.session_id;
        if needs_snapshot && let Ok(snapshot) = self.with_backend(|be| be.snapshot(session_id)) {
            self.snapshot = snapshot;
            self.revalidate_hover();
        }
        if had_output {
            self.cursor_visible = true;
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
        if self.visible || got_bell {
            cx.notify();
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
    fn visible_command_badges(&self) -> Vec<(usize, bool)> {
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
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
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
        // on the next keystroke. `display_offset` shifts the visible window
        // up into history while scrolled, so highlights track the rows the
        // user is actually looking at rather than the live tail.
        let visible_rows = self.snapshot.cells.len();
        let buckets = self
            .search
            .render_buckets(visible_rows, self.snapshot.display_offset);

        // Live render-time knobs from settings (alpha multipliers).
        let s = terminal_settings(cx);
        let alphas = Alphas {
            dim: s.dim_alpha,
            unfocused: s.unfocused_alpha,
            unfocused_cursor: s.unfocused_cursor_alpha,
        };

        // Build owned paint params (`FnOnce + 'static` requires no
        // borrows). Clone is cheap: snapshot is a Vec<Vec<Cell>> already
        // sized to the visible grid, buckets are tiny per-row vecs of
        // MatchHit (Copy), theme/typography/cursor are POD-sized.
        let paint_params = PaintParams {
            snapshot: self.snapshot.clone(),
            theme,
            typography: self.typography.clone(),
            cursor,
            cursor_shape: self.snapshot.cursor_shape,
            buckets,
            pane_focused,
            pad,
            hovered_link: self.hovered_link,
            selection: self.selection,
            command_badges: self.visible_command_badges(),
            alphas,
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
        let canvas_bounds = Rc::clone(&self.canvas_bounds);
        let grid_canvas = canvas(
            // Prepaint: no per-paint state to capture; return unit.
            |_bounds, _window, _cx| (),
            move |bounds, _: (), window, cx| {
                // Record the painted bounds so mouse handlers can map a
                // pixel position back to a cell on the next event.
                canvas_bounds.set(bounds);
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
            .on_action(cx.listener(Self::on_send_selection_to_agent))
            .on_action(cx.listener(Self::on_send_last_command_output_to_agent))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    // Cmd-click on a link opens it instead of selecting/reporting.
                    if this.try_open_link(ev, window, cx) {
                        cx.notify();
                        return;
                    }
                    // A mouse-reporting app (no Shift) gets the click forwarded
                    // instead of starting a local selection.
                    if !this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Press,
                        window,
                        cx,
                    ) {
                        this.on_select_down(ev, window);
                    }
                    // Notify so `MainPane`'s observer can re-sync the focused
                    // PaneId and repaint the active-pane ring on the next
                    // frame. Without this, click-to-focus is invisible until
                    // the next Cmd-* action.
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                // Cmd-hover link underline updates regardless of button state.
                this.update_hover(ev, window, cx);
                let Some(button) = ev.pressed_button else {
                    return;
                };
                // Forward drags to a mouse-reporting app; otherwise extend the
                // local selection.
                if this.report_mouse(
                    button,
                    ev.position,
                    &ev.modifiers,
                    MouseAction::Drag,
                    window,
                    cx,
                ) {
                    return;
                }
                if this.selecting.is_some() && button == MouseButton::Left {
                    let cell = this.cell_at(ev.position, window);
                    this.apply_drag(cell);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                    if this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Release,
                        window,
                        cx,
                    ) {
                        return;
                    }
                    if this.finish_select() {
                        cx.notify();
                    }
                }),
            )
            // Middle / right buttons only matter for mouse-reporting apps
            // (right-click menus in vim/tmux, middle-click paste). There is no
            // local-selection fallback for them, so they just forward when the
            // app is reporting and are otherwise ignored.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Press,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                    this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Release,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Press,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, ev: &MouseUpEvent, window, cx| {
                    this.report_mouse(
                        ev.button,
                        ev.position,
                        &ev.modifiers,
                        MouseAction::Release,
                        window,
                        cx,
                    );
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, window, cx| {
                this.on_wheel(ev, window, cx);
            }))
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
        // Scrolled-up indicator: a faint chip while the viewport is off the
        // live tail, so the user knows new output is landing below the fold
        // and that any keystroke will snap back down.
        if self.snapshot.display_offset > 0 {
            root = root.child(build_scroll_indicator(&theme, self.snapshot.display_offset));
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

/// Faint top-right chip shown while the viewport is scrolled up off the live
/// tail (`display_offset > 0`). Informational only — any keystroke snaps the
/// view back to the bottom (`send_bytes` → `scroll_to_bottom`).
fn build_scroll_indicator(theme: &Theme, offset: usize) -> gpui::Div {
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
        // `↑` = U+2191 Upwards Arrow.
        .child(format!("↑ {offset} lines"))
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
}
