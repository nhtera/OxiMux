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

use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Task, Window, div, px,
};
use oximux_pty::{
    PortablePtyBackend, TerminalBackend, TerminalEvent, TerminalSessionId, TerminalSnapshot,
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::Search;
use crate::shell::key_input::keystroke_to_bytes;
use crate::shell::terminal_row::build_row;
use crate::shell::terminal_search::{
    MatchRange, find_matches, render_search_overlay, visible_match_ranges,
};

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

pub struct TerminalView {
    backend: PortablePtyBackend,
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
    /// Search overlay state. When `search_active` is true the on_key_down
    /// branch above the PTY path consumes printable input, Backspace, and
    /// Esc/Enter; all other keys are swallowed (no PTY write). Per-pane;
    /// resetting these on dismiss drops the highlight on the next paint.
    /// `search_history_len` records how much of the search grid was history
    /// at scan time, so the render path can offset `MatchRange::row` back
    /// to a visible-row index. Deriving it at render time from the max
    /// match row only works when scrollback is full — see code-review
    /// 260516-0807 for the failure cases.
    search_active: bool,
    search_query: String,
    search_matches: Vec<MatchRange>,
    search_history_len: usize,
    _poll_task: Task<()>,
    _blink_task: Task<()>,
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

        // Mirror focus state into `view.focused` for the blink task. Detach so
        // the listener survives until the entity drops; the listener's
        // WeakEntity will self-clean via update-returning-Err at that point.
        // `on_focus` also resets `cursor_visible` so a pane gaining focus
        // never waits a full 530 ms for the cursor to reappear.
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
            focused: true,
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_history_len: 0,
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

    fn on_search(&mut self, _: &Search, _window: &mut Window, cx: &mut Context<Self>) {
        // Esc/Enter clear the query, so the only way to reach this with
        // `search_query` non-empty is a stray Cmd+F while already open —
        // benign (re-runs the same match set against a fresh grid).
        self.search_active = true;
        self.rerun_search(cx);
        cx.notify();
    }

    fn rerun_search(&mut self, _cx: &mut Context<Self>) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_history_len = 0;
            return;
        }
        let grid = self.backend.search_grid(self.session_id);
        // Record history_len at scan time. Recovering it at render time
        // from `max(MatchRange::row)` fails on partial-history scrollback
        // and on history-only match sets — see code-review 260516-0807 H1.
        let visible = self.snapshot.cells.len();
        self.search_history_len = grid.len().saturating_sub(visible);
        self.search_matches = find_matches(&grid, &self.search_query);
    }

    fn dismiss_search(&mut self, cx: &mut Context<Self>) {
        self.search_active = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_history_len = 0;
        cx.notify();
    }

    /// Search-mode key handling. Returns true when the keystroke was consumed
    /// (do not forward to the PTY). Modifier-bearing input (Cmd / Ctrl / Alt)
    /// is left for the regular path so Cmd+F-then-Cmd+W still closes the tab.
    fn handle_search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let ks = &event.keystroke;
        if ks.modifiers.platform || ks.modifiers.control || ks.modifiers.alt {
            // Cmd/Ctrl/Alt combos take precedence — let them bubble.
            // (Cmd+F again is benign: re-fires `on_search`, no-op state.)
            return false;
        }
        match ks.key.as_str() {
            "escape" | "enter" => {
                self.dismiss_search(cx);
                return true;
            }
            "backspace" => {
                self.search_query.pop();
                self.rerun_search(cx);
                cx.notify();
                return true;
            }
            _ => {}
        }
        // Append printable text. Prefer `key_char` (shift-aware, IME-aware)
        // and reject anything that's a control byte — accidental ctrl-X
        // shouldn't pollute the needle even though we already filter
        // modifier-bearing keys above.
        let candidate =
            ks.key_char
                .as_deref()
                .filter(|s| !s.is_empty())
                .or(if ks.key.chars().count() == 1 {
                    Some(ks.key.as_str())
                } else {
                    None
                });
        if let Some(s) = candidate
            && s.chars().all(|c| !c.is_control())
        {
            self.search_query.push_str(s);
            self.rerun_search(cx);
            cx.notify();
            return true;
        }
        // Anything else (function keys, arrows, etc.) is swallowed while the
        // overlay is open — they neither edit the query nor reach the shell.
        true
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.search_active && self.handle_search_key(event, cx) {
            return;
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
        if let Err(err) = self.backend.write(self.session_id, bytes) {
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
        let wrap = self
            .backend
            .bracketed_paste(self.session_id)
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
        let events = self.backend.drain_events();
        if events.is_empty() {
            return;
        }
        // Resnapshot + reset blink only on real bytes. `Resize` and `Exit`
        // also flow through here — skipping the snapshot avoids one mutex
        // lock + full grid allocation per resize event under flood, and
        // skipping the blink reset stops the cursor pinning visible on a
        // dead session after the shell quits.
        let has_output = events
            .iter()
            .any(|e| matches!(e, TerminalEvent::Output { .. }));
        if has_output {
            if let Ok(snapshot) = self.backend.snapshot(self.session_id) {
                self.snapshot = snapshot;
            }
            self.cursor_visible = true;
        }
        cx.notify();
    }

    fn maybe_resize(&mut self) {
        if self.target_grid == self.last_resize {
            return;
        }
        if let Err(err) =
            self.backend
                .resize(self.session_id, self.target_grid.0, self.target_grid.1)
        {
            tracing::warn!(?err, "pty resize failed");
            return;
        }
        self.last_resize = self.target_grid;
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

        // Cursor is only drawn when this pane is focused *and* we're in the
        // visible half of the blink cycle. Out-of-grid sentinels make
        // `build_row`'s cursor-col check fall through silently — no `if`
        // gating in the hot path.
        let show_cursor = self.focus_handle.is_focused(window) && self.cursor_visible;
        let cursor = if show_cursor {
            (
                self.snapshot.cursor.0 as usize,
                self.snapshot.cursor.1 as usize,
            )
        } else {
            (usize::MAX, usize::MAX)
        };
        let theme = self.theme;
        let line_height = px(self.typography.t_body_lg + 4.0);
        let pad = px(self.density.pad_panel);
        let focus_handle = self.focus_handle.clone();

        // Match buckets per visible row. `search_history_len` was captured
        // at scan time (rerun_search) — using it here avoids deriving the
        // offset from the match set, which fails on partial-history grids.
        // If PTY output has scrolled history since the scan, the highlights
        // may drift one paint but self-correct on the next keystroke.
        let visible_rows = self.snapshot.cells.len();
        let buckets = if self.search_active && !self.search_matches.is_empty() {
            visible_match_ranges(&self.search_matches, self.search_history_len, visible_rows)
        } else {
            Vec::new()
        };

        let rows: Vec<gpui::Div> = self
            .snapshot
            .cells
            .iter()
            .enumerate()
            .map(|(row_idx, row)| {
                let match_cols = buckets.get(row_idx).map(|v| v.as_slice());
                build_row(row, row_idx, cursor, match_cols, &theme, line_height)
            })
            .collect();

        let overlay = if self.search_active {
            Some(render_search_overlay(
                &self.search_query,
                &theme,
                &self.typography,
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
            .font_family(self.typography.family_mono.clone())
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
