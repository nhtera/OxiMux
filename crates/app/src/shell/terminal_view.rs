//! TerminalView — single-pane PTY render (Phase 1 step 4 + polish).
//!
//! Owns a `PortablePtyBackend` and one session. A background polling task
//! ticks every `POLL_INTERVAL_MS`, drains the event queue, copies the latest
//! `TerminalSnapshot` onto the view, and calls `cx.notify()`. Render walks
//! `snapshot.cells`, groups consecutive same-styled cells into runs, and
//! emits one styled `div` per run inside one row `div` per line.
//!
//! Polish slice adds three things on top of step 4's read-only baseline:
//! color (each run carries fg + bg resolved against the charcoal theme),
//! cursor (the cell under `snapshot.cursor` is forced to `inverse` so the
//! block cursor renders without any extra layout work), and resize (at
//! render time the view measures the available area from
//! `window.viewport_size()` minus the fixed chrome — sidebar + top +
//! status + paddings — computes a target `(cols, rows)` against hardcoded
//! cell metrics, and calls `backend.resize()` when the target changes).
//! Cell metrics are calibrated for Geist Mono 14 px; replace with
//! `text_system().line_height()` measurement during the step 9 perf pass.

use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, Styled, Task, Window, div,
    px,
};
use oximux_pty::{Cell, PortablePtyBackend, TerminalBackend, TerminalSessionId, TerminalSnapshot};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::key_input::keystroke_to_bytes;
use crate::shell::terminal_palette::{ColorRole, resolve};

/// How often the view drains events + re-snapshots. 16 ms ≈ 60 fps, matches the
/// `< 16 ms PTY → render latency` plan target without over-spending CPU on
/// idle.
const POLL_INTERVAL_MS: u64 = 16;

/// Default grid size on spawn. Window-driven resize takes over on the first
/// render.
pub const DEFAULT_COLS: u16 = 100;
pub const DEFAULT_ROWS: u16 = 32;

/// Hardcoded cell metrics for Geist Mono 14 px. Measured manually; replace
/// with `text_system().line_height()` + advance lookup during the step 9
/// perf/measurement pass.
const CELL_WIDTH_PX: f32 = 8.4;
const CELL_HEIGHT_PX: f32 = 18.0;

/// Chrome subtracted from viewport when computing the terminal area. Stays
/// in sync with `WorkspaceRoot` composition + density tokens.
const CHROME_W_PX: f32 = 240.0; // sidebar
const CHROME_H_PX: f32 = 36.0 + 22.0; // top bar + status bar

const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;

pub struct TerminalView {
    backend: PortablePtyBackend,
    session_id: TerminalSessionId,
    snapshot: TerminalSnapshot,
    theme: Theme,
    density: Density,
    typography: Typography,
    focus_handle: FocusHandle,
    last_resize: (u16, u16),
    _poll_task: Task<()>,
}

impl TerminalView {
    /// Build a view around an already-spawned backend + session. The spawn
    /// is done outside `cx.new` because the entity builder closure is
    /// infallible; this keeps spawn errors at the caller where they can be
    /// logged + fall back to a placeholder.
    pub fn mount(
        backend: PortablePtyBackend,
        session_id: TerminalSessionId,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = backend
            .snapshot(session_id)
            .unwrap_or_else(|_| TerminalSnapshot::empty(DEFAULT_COLS, DEFAULT_ROWS));

        let focus_handle = cx.focus_handle();
        // Grab focus on mount so the user can type into the shell immediately
        // without first clicking. Without this the window opens with no focus
        // owner and keystrokes are dropped until the first click.
        focus_handle.focus(window, cx);

        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                let executor = match this.read_with(cx, |_, cx| cx.background_executor().clone()) {
                    Ok(executor) => executor,
                    Err(_) => return,
                };
                executor
                    .timer(Duration::from_millis(POLL_INTERVAL_MS))
                    .await;
                if this.update(cx, |view, cx| view.tick(cx)).is_err() {
                    return;
                }
            }
        });

        Self {
            backend,
            session_id,
            snapshot,
            theme,
            density,
            typography,
            focus_handle,
            last_resize: (DEFAULT_COLS, DEFAULT_ROWS),
            _poll_task: poll_task,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let bytes = keystroke_to_bytes(&event.keystroke);
        if bytes.is_empty() {
            return;
        }
        if let Err(err) = self.backend.write(self.session_id, &bytes) {
            tracing::warn!(?err, "pty write failed");
            return;
        }
        cx.notify();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let events = self.backend.drain_events();
        if events.is_empty() {
            return;
        }
        if let Ok(snapshot) = self.backend.snapshot(self.session_id) {
            self.snapshot = snapshot;
        }
        cx.notify();
    }

    /// Compute (cols, rows) from the window viewport. Subtracts known chrome
    /// and panel padding, divides by hardcoded cell metrics, then clamps to
    /// a minimum so the PTY never sees `cols=0` when the window is dragged
    /// to a degenerate size.
    fn target_grid(&self, window: &Window) -> (u16, u16) {
        let viewport = window.viewport_size();
        let pad = self.density.pad_panel * 2.0;
        let available_w = (f32::from(viewport.width) - CHROME_W_PX - pad).max(CELL_WIDTH_PX);
        let available_h = (f32::from(viewport.height) - CHROME_H_PX - pad).max(CELL_HEIGHT_PX);
        let cols = (available_w / CELL_WIDTH_PX).floor() as u16;
        let rows = (available_h / CELL_HEIGHT_PX).floor() as u16;
        (cols.max(MIN_COLS), rows.max(MIN_ROWS))
    }

    fn maybe_resize(&mut self, window: &Window) {
        let target = self.target_grid(window);
        if target == self.last_resize {
            return;
        }
        if let Err(err) = self.backend.resize(self.session_id, target.0, target.1) {
            tracing::warn!(?err, "pty resize failed");
            return;
        }
        self.last_resize = target;
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_resize(window);

        let cursor = (
            self.snapshot.cursor.0 as usize,
            self.snapshot.cursor.1 as usize,
        );
        let theme = self.theme;
        let line_height = px(self.typography.t_body_lg + 4.0);
        let pad = px(self.density.pad_panel);
        let focus_handle = self.focus_handle.clone();

        let rows: Vec<gpui::Div> = self
            .snapshot
            .cells
            .iter()
            .enumerate()
            .map(|(row_idx, row)| build_row(row, row_idx, cursor, &theme, line_height))
            .collect();

        div()
            .id("oximux-terminal-view")
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_base)
            .text_color(theme.fg_base)
            .font_family(self.typography.family_mono.clone())
            .text_size(px(self.typography.t_body_lg))
            .px(pad)
            .py(pad)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx);
            }))
            .children(rows)
    }
}

fn build_row(
    row: &[Cell],
    row_idx: usize,
    cursor: (usize, usize),
    theme: &Theme,
    line_height: gpui::Pixels,
) -> gpui::Div {
    let mut row_div = div().flex().flex_row().h(line_height).whitespace_nowrap();
    let cursor_col = if cursor.0 == row_idx {
        Some(cursor.1)
    } else {
        None
    };
    for run in group_runs(row, cursor_col) {
        let (fg, bg) = effective_colors(&run, theme);
        let mut span = div().child(SharedString::from(run.text)).text_color(fg);
        if !run.inverse && run.bg == oximux_pty::CellColor::Default {
            // Skip painting the canvas — saves one rect per default-bg run.
        } else {
            span = span.bg(bg);
        }
        row_div = row_div.child(span);
    }
    row_div
}

struct Run {
    text: String,
    fg: oximux_pty::CellColor,
    bg: oximux_pty::CellColor,
    inverse: bool,
}

fn group_runs(row: &[Cell], cursor_col: Option<usize>) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (col_idx, cell) in row.iter().enumerate() {
        let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
        let inverse = cell.inverse || cursor_col == Some(col_idx);
        match runs.last_mut() {
            Some(last) if last.fg == cell.fg && last.bg == cell.bg && last.inverse == inverse => {
                last.text.push(ch);
            }
            _ => runs.push(Run {
                text: ch.to_string(),
                fg: cell.fg,
                bg: cell.bg,
                inverse,
            }),
        }
    }
    runs
}

/// Resolve fg + bg as concrete Hsla, then swap if `inverse`. Swapping at the
/// Hsla layer (not the `CellColor` layer) is what makes inverse on a
/// Default/Default cell actually flip the colors — if we swapped the
/// `CellColor`s first, `resolve(Default, Fg)` and `resolve(Default, Bg)`
/// would still split into fg_base / bg_base, but a Default/Default swap
/// would land back on fg_base / bg_base again and the cursor would be
/// invisible. With this version, the resolved fg_base (white) and bg_base
/// (charcoal) swap cleanly: cursor renders as a charcoal glyph on a white
/// block.
fn effective_colors(run: &Run, theme: &Theme) -> (Hsla, Hsla) {
    let fg = resolve(run.fg, ColorRole::Fg, theme);
    let bg = resolve(run.bg, ColorRole::Bg, theme);
    if run.inverse { (bg, fg) } else { (fg, bg) }
}
