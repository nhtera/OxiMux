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

use crate::shell::terminal_search::{
    MatchRange, SearchOptions, compile_search_regex, find_matches_precompiled,
    find_matches_with_options,
};

/// Outcome of a keystroke routed to the search overlay. The host matches
/// on this to decide whether to notify, fetch a fresh grid, or fall through
/// to the regular PTY path.
pub enum SearchKeyOutcome {
    /// Search wasn't active or the keystroke carried Cmd/Ctrl/Alt — let
    /// the regular `on_key_down` path handle it.
    Pass,
    /// Key consumed; no state change, no repaint needed (e.g. function key
    /// swallowed while overlay is open).
    Consumed,
    /// Esc dismissed the overlay; host should repaint.
    Dismissed,
    /// Query mutated (backspace / printable input). Host must fetch a fresh
    /// grid and call `rerun`, then repaint.
    QueryChanged,
    /// Cycled to next/prev match (Enter / Shift+Enter / Up / Down). Host
    /// only needs to repaint — no grid refetch.
    CurrentChanged,
}

/// Which highlight style applies to a match cell run. `Current` is the
/// cycled "you are here" match (one at a time); `Other` is every other
/// match in the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    Current,
    Other,
}

/// Per-row match range with its highlight kind. The render path groups
/// consecutive cells with the same kind into one styled span.
#[derive(Clone, Copy, Debug)]
pub struct MatchHit {
    pub kind: MatchKind,
    pub col_start: usize,
    pub col_end: usize,
}

pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<MatchRange>,
    /// History row count at scan time. The render path subtracts this from
    /// `MatchRange::row` to get a visible-row index. Deriving it at render
    /// time from `max(MatchRange::row)` fails on partial-history scrollback
    /// and on history-only match sets — see code-review 260516-0807 H1.
    pub history_len: usize,
    /// Index into `matches` for the cycled "you are here" match. Set to
    /// `Some(0)` after each `rerun` when matches exist, advanced by
    /// `next_match` / `prev_match`. `None` when no matches.
    pub current_index: Option<usize>,
    /// Toggle state for the Aa / ab / .* overlay buttons. Defaults to all-
    /// off, which preserves the original "case-insensitive plain substring"
    /// scan behavior.
    pub options: SearchOptions,
    /// Compiled regex cached across reruns, keyed by the (query,
    /// case-sensitive) pair it was built from — typing reruns of the same
    /// needle (scroll-driven refreshes, toggles that don't affect the
    /// pattern) skip recompilation. `None` also covers invalid patterns.
    compiled: Option<(String, bool, regex::Regex)>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            active: false,
            query: String::new(),
            matches: Vec::new(),
            history_len: 0,
            current_index: None,
            options: SearchOptions::default(),
            compiled: None,
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
        self.current_index = None;
    }

    /// Re-scan the grid for the current query under the current `options`.
    /// Empty-query short-circuit keeps the host from over-fetching. Resets
    /// `current_index` to the first match so the user sees a highlighted
    /// "current" on every query mutation (find-as-you-type lands you on
    /// the first hit immediately).
    pub fn rerun(&mut self, grid: &[Vec<Cell>], visible_rows: usize) {
        if self.query.is_empty() {
            self.matches.clear();
            self.history_len = 0;
            self.current_index = None;
            return;
        }
        self.history_len = grid.len().saturating_sub(visible_rows);
        self.matches = if self.options.regex {
            // Compile at most once per (needle, case) pair. Invalid
            // patterns don't cache — they're cheap to re-reject and yield
            // zero matches either way.
            let stale = !matches!(
                &self.compiled,
                Some((q, cs, _)) if *q == self.query && *cs == self.options.case_sensitive
            );
            if stale {
                self.compiled = compile_search_regex(&self.query, self.options.case_sensitive)
                    .map(|re| (self.query.clone(), self.options.case_sensitive, re));
            }
            match &self.compiled {
                Some((q, cs, re)) if *q == self.query && *cs == self.options.case_sensitive => {
                    find_matches_precompiled(grid, re, self.options)
                }
                _ => Vec::new(), // invalid pattern → zero matches (unchanged)
            }
        } else {
            find_matches_with_options(grid, &self.query, self.options)
        };
        self.current_index = if self.matches.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    /// Flip the case-sensitive toggle. Caller must `rerun` afterward — the
    /// state machine is pure so it doesn't touch the grid.
    pub fn toggle_case_sensitive(&mut self) {
        self.options.case_sensitive = !self.options.case_sensitive;
    }

    /// Flip the whole-word toggle. Caller must `rerun` afterward.
    pub fn toggle_whole_word(&mut self) {
        self.options.whole_word = !self.options.whole_word;
    }

    /// Flip the regex toggle. Caller must `rerun` afterward.
    pub fn toggle_regex(&mut self) {
        self.options.regex = !self.options.regex;
    }

    /// Cycle to the next match, wrapping. No-op when there are no matches.
    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            self.current_index = None;
            return;
        }
        let next = match self.current_index {
            Some(i) => (i + 1) % self.matches.len(),
            None => 0,
        };
        self.current_index = Some(next);
    }

    /// Cycle to the previous match, wrapping. No-op when there are no
    /// matches.
    pub fn prev_match(&mut self) {
        if self.matches.is_empty() {
            self.current_index = None;
            return;
        }
        let len = self.matches.len();
        let prev = match self.current_index {
            Some(0) | None => len - 1,
            Some(i) => i - 1,
        };
        self.current_index = Some(prev);
    }

    /// Format the count badge for the overlay: empty when no query, else
    /// `i of N` (`- of 0` when no matches).
    pub fn count_badge(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let total = self.matches.len();
        match self.current_index {
            Some(i) => format!("{} of {}", i + 1, total),
            None => format!("- of {}", total),
        }
    }

    /// Dispatch a keystroke while the overlay is active. Returns
    /// `SearchKeyOutcome::Pass` when search is inactive (or the keystroke
    /// carries Cmd/Ctrl/Alt — those should reach the regular path).
    ///
    /// Bindings:
    /// - Escape       → dismiss
    /// - Enter        → next match (cycle forward)
    /// - Shift+Enter  → prev match (cycle back)
    /// - Up           → prev match
    /// - Down         → next match
    /// - Backspace    → pop char (re-runs scan)
    /// - Printable    → append (re-runs scan)
    /// - Other        → swallow (no repaint)
    pub fn handle_key(&mut self, event: &KeyDownEvent) -> SearchKeyOutcome {
        if !self.active {
            return SearchKeyOutcome::Pass;
        }
        let ks = &event.keystroke;
        if ks.modifiers.platform || ks.modifiers.control || ks.modifiers.alt {
            return SearchKeyOutcome::Pass;
        }
        match ks.key.as_str() {
            "escape" => {
                self.close();
                return SearchKeyOutcome::Dismissed;
            }
            "enter" => {
                if ks.modifiers.shift {
                    self.prev_match();
                } else {
                    self.next_match();
                }
                return SearchKeyOutcome::CurrentChanged;
            }
            "up" => {
                self.prev_match();
                return SearchKeyOutcome::CurrentChanged;
            }
            "down" => {
                self.next_match();
                return SearchKeyOutcome::CurrentChanged;
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
        // Function keys, etc. — swallowed (don't reach the shell) but no
        // state mutation, so no repaint needed.
        SearchKeyOutcome::Consumed
    }

    /// Bucket match ranges by visible row, tagging each with its highlight
    /// kind (`Current` for the cycled match, `Other` for the rest).
    /// Returns an empty Vec when inactive or no matches — callers can pass
    /// the result to `build_row` per-row without an extra `if active`
    /// branch.
    pub fn render_buckets(
        &self,
        visible_rows: usize,
        display_offset: usize,
    ) -> Vec<Vec<MatchHit>> {
        if !self.active || self.matches.is_empty() {
            return Vec::new();
        }
        // Top of the visible window in search-grid coordinates. At the live
        // tail (`display_offset == 0`) this is `history_len`; scrolling up by
        // N moves the window up N rows into history.
        let window_top = self.history_len.saturating_sub(display_offset);
        let mut buckets: Vec<Vec<MatchHit>> = vec![Vec::new(); visible_rows];
        for (idx, m) in self.matches.iter().enumerate() {
            if m.row < window_top {
                continue;
            }
            let visible_idx = m.row - window_top;
            if visible_idx >= visible_rows {
                continue;
            }
            let kind = if Some(idx) == self.current_index {
                MatchKind::Current
            } else {
                MatchKind::Other
            };
            buckets[visible_idx].push(MatchHit {
                kind,
                col_start: m.col_start,
                col_end: m.col_end,
            });
        }
        buckets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(row: usize) -> MatchRange {
        MatchRange {
            row,
            col_start: 0,
            col_end: 1,
        }
    }

    #[test]
    fn render_buckets_shifts_with_display_offset() {
        // 10 history rows above a 5-row viewport. Match at search-grid row 12
        // is on the live tail; row 8 is up in history.
        let mut s = SearchState::new();
        s.active = true;
        s.history_len = 10;
        s.current_index = Some(0);
        s.matches = vec![hit(12), hit(8)];

        // At the tail (offset 0): row 12 → visible row 2; the history match is
        // above the window and dropped.
        let tail = s.render_buckets(5, 0);
        assert!(!tail[2].is_empty(), "tail match lands on visible row 2");
        assert!(tail[0].is_empty() && tail[4].is_empty());

        // Scrolled up 4 lines: window top = 6, so the history match at row 8
        // becomes visible row 2, and the former tail match (row 12) scrolls
        // off the bottom and is dropped.
        let scrolled = s.render_buckets(5, 4);
        assert!(
            !scrolled[2].is_empty(),
            "history match is visible at row 2 once scrolled up"
        );
    }
}
