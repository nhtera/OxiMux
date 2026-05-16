//! Search-overlay state for `TerminalView`.
//!
//! Owns the four state fields (active/query/matches/history_len) and the
//! search-mode key dispatcher. The host (`TerminalView`) owns I/O: it
//! fetches the search grid from the backend and calls `cx.notify()`. This
//! module is pure data + pure dispatch so the math stays testable in
//! isolation and `terminal_view.rs` stays under the 500-LOC file-size cap.
//!
//! Naming note: `terminal_search.rs` is the row-major scan + overlay paint
//! (pure functions). This module is the *stateful* glue between key events
//! and that scan. Two files because the responsibility levels differ —
//! pure math vs view-coupled state machine.

use gpui::KeyDownEvent;
use oximux_pty::Cell;

use crate::shell::terminal_search::{MatchRange, find_matches, visible_match_ranges};

/// Outcome of a keystroke routed to the search overlay. The host matches
/// on this to decide whether to notify, fetch a fresh grid, or fall through
/// to the regular PTY path.
pub enum SearchKeyOutcome {
    /// Search wasn't active or the keystroke carried a modifier — let the
    /// regular `on_key_down` path handle it. (Cmd+F-while-open is benign:
    /// the action re-dispatches `Search`.)
    Pass,
    /// Key consumed; no state change, no repaint needed (e.g. function key
    /// swallowed while overlay is open).
    Consumed,
    /// Esc / Enter dismissed the overlay; host should repaint.
    Dismissed,
    /// Query mutated (backspace / printable input). Host must fetch a fresh
    /// grid and call `rerun`, then repaint.
    QueryChanged,
}

pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<MatchRange>,
    /// History row count at scan time. The render path subtracts this from
    /// `MatchRange::row` to get a visible-row index. Deriving it at render
    /// time from `max(MatchRange::row)` fails on partial-history grids and
    /// on history-only match sets — see code-review 260516-0807 H1.
    pub history_len: usize,
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            matches: Vec::new(),
            history_len: 0,
        }
    }

    /// Flip into search mode. Idempotent — re-opening preserves the query
    /// so a stray Cmd+F while already open is a no-op state-wise (host
    /// still triggers a re-scan against a fresh grid, which is what the
    /// user expects).
    pub fn open(&mut self) {
        self.active = true;
    }

    /// Close the overlay and drop all search state. Host calls `cx.notify`
    /// after this so the highlight bg disappears on the next paint.
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.history_len = 0;
    }

    /// Re-scan the grid for the current query. Empty-query short-circuit
    /// keeps the host from over-fetching: when the query is empty, matches
    /// and history_len are cleared and no scan runs.
    pub fn rerun(&mut self, grid: &[Vec<Cell>], visible_rows: usize) {
        if self.query.is_empty() {
            self.matches.clear();
            self.history_len = 0;
            return;
        }
        self.history_len = grid.len().saturating_sub(visible_rows);
        self.matches = find_matches(grid, &self.query);
    }

    /// Dispatch a keystroke while the overlay is active. Returns
    /// `SearchKeyOutcome::Pass` when search is inactive (or the keystroke
    /// carries Cmd/Ctrl/Alt — those should reach the regular path).
    pub fn handle_key(&mut self, event: &KeyDownEvent) -> SearchKeyOutcome {
        if !self.active {
            return SearchKeyOutcome::Pass;
        }
        let ks = &event.keystroke;
        if ks.modifiers.platform || ks.modifiers.control || ks.modifiers.alt {
            return SearchKeyOutcome::Pass;
        }
        match ks.key.as_str() {
            "escape" | "enter" => {
                self.close();
                return SearchKeyOutcome::Dismissed;
            }
            "backspace" => {
                self.query.pop();
                return SearchKeyOutcome::QueryChanged;
            }
            _ => {}
        }
        // Prefer `key_char` (shift-aware, IME-aware) and reject any control
        // byte even though we already filtered modifier-bearing keys above.
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
            self.query.push_str(s);
            return SearchKeyOutcome::QueryChanged;
        }
        // Function keys, arrows, etc. — swallowed (don't reach the shell)
        // but no state mutation, so no repaint needed.
        SearchKeyOutcome::Consumed
    }

    /// Bucket match ranges by visible row for the render path. Returns an
    /// empty Vec when inactive or no matches — callers can pass the result
    /// to `build_row` per-row without an extra `if active` branch.
    pub fn render_buckets(&self, visible_rows: usize) -> Vec<Vec<(usize, usize)>> {
        if !self.active || self.matches.is_empty() {
            return Vec::new();
        }
        visible_match_ranges(&self.matches, self.history_len, visible_rows)
    }
}
