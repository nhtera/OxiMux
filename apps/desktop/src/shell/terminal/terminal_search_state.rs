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
    /// The paste chord (Cmd+V, or Ctrl+Shift+V) landed while the overlay
    /// was open. The state machine has no clipboard access, so the host
    /// reads it and feeds the text through [`SearchState::paste`]. Without
    /// this the chord fell through to the terminal's own paste path and the
    /// clipboard went to the shell instead of the find box.
    PasteRequested,
    /// The query's selection changed without editing it: Cmd+A selected the
    /// whole query (or the empty box swallowed the chord), or a caret key
    /// collapsed the selection. Host only repaints. Without the Cmd+A claim
    /// the chord fell through to the terminal's own select-all and
    /// highlighted the grid instead of the find box.
    SelectionChanged,
    /// Cmd+C landed while the query was selected. The host copies
    /// [`SearchState::query`] to the clipboard. Without the claim the chord
    /// fell through to the terminal's copy, which sends SIGINT to the shell
    /// when the grid has no selection.
    CopyRequested,
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
    /// and on history-only match sets, so it must be captured here.
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
    /// Whether the whole query is selected (Cmd+A). The find box has no
    /// movable caret, so "everything selected" is the only selection it can
    /// hold: the next edit replaces the query, Backspace clears it, and Cmd+C
    /// copies it. Private so only the edit paths below can set or clear it.
    query_selected: bool,
    /// Whether the find box has the keyboard. The terminal keeps the GPUI
    /// focus either way; this decides whether [`Self::handle_key`] claims
    /// keystrokes or hands them to the terminal. Opening (Cmd+F) or clicking
    /// the bar sets it; a click on the grid clears it, so with the bar still
    /// open the user can type, Cmd+A, or Esc in the terminal itself.
    input_focused: bool,
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
            query_selected: false,
            input_focused: false,
        }
    }

    /// Whether the whole query is selected (Cmd+A), for the overlay paint.
    pub fn is_query_selected(&self) -> bool {
        self.query_selected
    }

    /// Whether the find box has the keyboard, for the overlay paint (caret
    /// and focus ring).
    pub fn is_input_focused(&self) -> bool {
        self.active && self.input_focused
    }

    /// Give the keyboard back to the find box (a click on the bar).
    pub fn focus_input(&mut self) {
        self.input_focused = true;
    }

    /// Hand the keyboard to the terminal while the bar stays open (a click
    /// on the grid). Drops any query selection with it, so Cmd+C copies the
    /// grid selection again.
    pub fn blur_input(&mut self) {
        self.input_focused = false;
        self.query_selected = false;
    }

    /// Drop the query if it is selected, so the edit that follows replaces
    /// it rather than appending to it. Returns whether anything was dropped.
    fn take_selected_query(&mut self) -> bool {
        if !std::mem::take(&mut self.query_selected) {
            return false;
        }
        self.query.clear();
        true
    }

    /// Flip into search mode and give the find box the keyboard. Re-opening
    /// preserves the query and selects it, the way Cmd+F does in a browser or
    /// editor: typing replaces the old needle, Enter keeps searching for it.
    /// (Host still triggers a re-scan against a fresh grid.)
    pub fn open(&mut self) {
        self.active = true;
        self.input_focused = true;
        self.query_selected = !self.query.is_empty();
    }

    /// Close the overlay and drop all search state. Host calls `cx.notify`
    /// after this so the highlight bg disappears on the next paint.
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.history_len = 0;
        self.current_index = None;
        self.query_selected = false;
        self.input_focused = false;
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

    /// Append clipboard text to the query — or replace it, when it is
    /// selected. Returns whether the query changed, so the host knows whether
    /// a re-scan is due.
    ///
    /// Only the first line is taken: the scan is row-major, so a needle
    /// containing a line break can never match, and a multi-line paste into
    /// a single-line find box conventionally keeps the first line. Any other
    /// control byte (tabs, stray C0 bytes) is dropped for the same reason
    /// `handle_key` rejects control characters — the grid never holds them.
    pub fn paste(&mut self, text: &str) -> bool {
        let first_line = text.split(['\n', '\r']).next().unwrap_or("");
        let pasted: String = first_line.chars().filter(|c| !c.is_control()).collect();
        // A paste with nothing printable leaves a selected query selected,
        // the way a text field ignores an empty clipboard.
        if pasted.is_empty() {
            return false;
        }
        self.take_selected_query();
        self.query.push_str(&pasted);
        true
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
    /// `SearchKeyOutcome::Pass` when search is inactive, when the terminal
    /// has the keyboard (the user clicked back into the grid), or when the
    /// keystroke carries a Cmd/Ctrl/Alt chord the box does not claim — those
    /// reach the regular terminal path.
    ///
    /// Bindings:
    /// - Escape       → dismiss
    /// - Enter        → next match (cycle forward)
    /// - Shift+Enter  → prev match (cycle back)
    /// - Up           → prev match
    /// - Down         → next match
    /// - Backspace    → pop char, or clear a selected query (re-runs scan)
    /// - Delete       → clear a selected query
    /// - Cmd+V / Ctrl+Shift+V → paste into the query (host reads clipboard)
    /// - Cmd+A        → select the whole query
    /// - Left/Right/Home/End → collapse a selected query
    /// - Cmd+C        → copy the query, when it is selected (host writes it)
    /// - Printable    → append, or replace a selected query (re-runs scan)
    /// - Other        → swallow (no repaint)
    pub fn handle_key(&mut self, event: &KeyDownEvent) -> SearchKeyOutcome {
        if !self.active || !self.input_focused {
            return SearchKeyOutcome::Pass;
        }
        let ks = &event.keystroke;
        if is_paste_chord(&ks.modifiers, &ks.key) {
            return SearchKeyOutcome::PasteRequested;
        }
        if is_cmd_chord(&ks.modifiers, &ks.key, "a") {
            self.query_selected = !self.query.is_empty();
            return SearchKeyOutcome::SelectionChanged;
        }
        if self.query_selected && is_cmd_chord(&ks.modifiers, &ks.key, "c") {
            return SearchKeyOutcome::CopyRequested;
        }
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
            // Caret keys collapse the selection, so the next key appends
            // again instead of replacing; with nothing selected they stay
            // swallowed (the caret is pinned to the end of the query).
            "left" | "right" | "home" | "end" if self.query_selected => {
                self.query_selected = false;
                return SearchKeyOutcome::SelectionChanged;
            }
            // Forward Delete clears a selected query like Backspace; with
            // the caret pinned to the end there is nothing after it otherwise.
            "delete" if self.query_selected => {
                self.take_selected_query();
                return SearchKeyOutcome::QueryChanged;
            }
            "backspace" => {
                if !self.take_selected_query() {
                    self.query.pop();
                }
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
            self.take_selected_query();
            self.query.push_str(s);
            return SearchKeyOutcome::QueryChanged;
        }
        // Function keys, etc. — swallowed (don't reach the shell) but no
        // state mutation, so no repaint needed.
        SearchKeyOutcome::Consumed
    }

    /// Lines to scroll (positive = back into history, matching the
    /// backend's `scroll` convention) so the cycled match is on screen,
    /// or `None` when no scroll is needed — match already visible, or no
    /// current match. Off-screen matches land mid-viewport so the user
    /// gets context above and below; the emulator clamps over-scroll, so
    /// a tail-area match simply snaps back to offset 0.
    pub fn follow_delta(&self, visible_rows: usize, display_offset: usize) -> Option<i32> {
        if visible_rows == 0 {
            return None;
        }
        let row = self.matches.get(self.current_index?)?.row;
        let window_top = self.history_len.saturating_sub(display_offset);
        if row >= window_top && row < window_top + visible_rows {
            return None;
        }
        let desired_top = row.saturating_sub(visible_rows / 2);
        let desired_offset = self.history_len.saturating_sub(desired_top);
        let delta = desired_offset as i64 - display_offset as i64;
        (delta != 0).then(|| delta.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
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

/// The paste chord the overlay claims for itself: Cmd+V (the terminal's own
/// paste binding, so the two never disagree about what ⌘V means) and the
/// Ctrl+Shift+V convention Linux/Windows terminals use, since plain Ctrl+V
/// is a control byte the shell may want. Every other modifier combination
/// still passes through to the regular key path.
fn is_paste_chord(mods: &gpui::Modifiers, key: &str) -> bool {
    if key != "v" {
        return false;
    }
    let cmd_v = mods.platform && !mods.control && !mods.alt && !mods.shift;
    let ctrl_shift_v = mods.control && mods.shift && !mods.platform && !mods.alt;
    cmd_v || ctrl_shift_v
}

/// Plain Cmd+`key` — no Shift/Ctrl/Alt, so `Cmd+Shift+A` and friends keep
/// reaching whatever else binds them.
fn is_cmd_chord(mods: &gpui::Modifiers, key: &str, want: &str) -> bool {
    key == want && mods.platform && !mods.control && !mods.alt && !mods.shift
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn key(chord: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(chord).expect("valid chord"),
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn paste_chord_is_claimed_only_while_open() {
        let mut s = SearchState::new();
        assert!(matches!(s.handle_key(&key("cmd-v")), SearchKeyOutcome::Pass));

        s.open();
        assert!(matches!(
            s.handle_key(&key("cmd-v")),
            SearchKeyOutcome::PasteRequested
        ));
        assert!(matches!(
            s.handle_key(&key("ctrl-shift-v")),
            SearchKeyOutcome::PasteRequested
        ));
        // Other Cmd chords (copy with nothing selected in the box) still
        // belong to the terminal.
        assert!(matches!(s.handle_key(&key("cmd-c")), SearchKeyOutcome::Pass));
        assert!(matches!(s.handle_key(&key("cmd-shift-v")), SearchKeyOutcome::Pass));
        assert!(matches!(s.handle_key(&key("ctrl-v")), SearchKeyOutcome::Pass));
        // The chord never leaks into the query.
        assert!(s.query.is_empty());
    }

    #[test]
    fn paste_appends_first_line_without_control_bytes() {
        let mut s = SearchState::new();
        s.open();
        s.query.push_str("nmk");
        assert!(s.paste("-copilot\tx\nsecond line"));
        assert_eq!(s.query, "nmk-copilotx");

        // CRLF clipboard text stops at the CR too.
        assert!(s.paste("!\r\nignored"));
        assert_eq!(s.query, "nmk-copilotx!");

        // Nothing printable → no change, so the host skips the re-scan.
        assert!(!s.paste(""));
        assert!(!s.paste("\x1b\x07"));
        assert!(!s.paste("\r\nonly a second line"));
        assert_eq!(s.query, "nmk-copilotx!");
    }

    #[test]
    fn select_all_selects_the_query_not_the_terminal() {
        let mut s = SearchState::new();
        // Closed overlay: Cmd+A is the terminal's select-all.
        assert!(matches!(s.handle_key(&key("cmd-a")), SearchKeyOutcome::Pass));

        s.open();
        // Empty box still claims the chord so the grid is never selected,
        // but there is nothing to select.
        assert!(matches!(
            s.handle_key(&key("cmd-a")),
            SearchKeyOutcome::SelectionChanged
        ));
        assert!(!s.is_query_selected());

        s.query.push_str("ssss");
        assert!(matches!(
            s.handle_key(&key("cmd-a")),
            SearchKeyOutcome::SelectionChanged
        ));
        assert!(s.is_query_selected());
        assert_eq!(s.query, "ssss", "selecting never edits the query");

        // Cmd+Shift+A is not select-all; it keeps reaching the terminal.
        assert!(matches!(
            s.handle_key(&key("cmd-shift-a")),
            SearchKeyOutcome::Pass
        ));
    }

    #[test]
    fn selected_query_is_replaced_cleared_or_copied() {
        let mut s = SearchState::new();
        s.open();

        // Typing over a selection replaces the query.
        s.query.push_str("ssss");
        s.handle_key(&key("cmd-a"));
        assert!(matches!(s.handle_key(&key("x")), SearchKeyOutcome::QueryChanged));
        assert_eq!(s.query, "x");
        assert!(!s.is_query_selected());
        // ...and a second key appends again.
        s.handle_key(&key("y"));
        assert_eq!(s.query, "xy");

        // Backspace over a selection clears the whole query.
        s.handle_key(&key("cmd-a"));
        assert!(matches!(
            s.handle_key(&key("backspace")),
            SearchKeyOutcome::QueryChanged
        ));
        assert!(s.query.is_empty());
        assert!(!s.is_query_selected());

        // Forward Delete clears a selection too; without one it is a no-op
        // (the caret is pinned to the end of the query).
        s.query.push_str("abc");
        assert!(matches!(s.handle_key(&key("delete")), SearchKeyOutcome::Consumed));
        assert_eq!(s.query, "abc");
        s.handle_key(&key("cmd-a"));
        assert!(matches!(
            s.handle_key(&key("delete")),
            SearchKeyOutcome::QueryChanged
        ));
        assert!(s.query.is_empty());
        assert!(!s.is_query_selected());

        // Paste over a selection replaces; an empty paste keeps it selected.
        s.query.push_str("old");
        s.handle_key(&key("cmd-a"));
        assert!(!s.paste("\n"));
        assert!(s.is_query_selected());
        assert!(s.paste("new"));
        assert_eq!(s.query, "new");
        assert!(!s.is_query_selected());

        // Cmd+C copies only while the query is selected; otherwise the
        // terminal keeps its copy / SIGINT behaviour.
        assert!(matches!(s.handle_key(&key("cmd-c")), SearchKeyOutcome::Pass));
        s.handle_key(&key("cmd-a"));
        assert!(matches!(
            s.handle_key(&key("cmd-c")),
            SearchKeyOutcome::CopyRequested
        ));
        assert!(s.is_query_selected(), "copy keeps the selection");

        // A caret key collapses the selection; the next key appends again.
        s.query.push_str("ab");
        s.handle_key(&key("cmd-a"));
        assert!(matches!(
            s.handle_key(&key("right")),
            SearchKeyOutcome::SelectionChanged
        ));
        assert!(!s.is_query_selected());
        s.handle_key(&key("c"));
        assert_eq!(s.query, "newabc");

        // A click on the grid deselects, so Cmd+C copies the grid again.
        s.handle_key(&key("cmd-a"));
        s.blur_input();
        assert!(matches!(s.handle_key(&key("cmd-c")), SearchKeyOutcome::Pass));

        // Match navigation leaves the selection alone; closing drops it.
        s.focus_input();
        s.handle_key(&key("cmd-a"));
        s.handle_key(&key("enter"));
        assert!(s.is_query_selected());
        s.handle_key(&key("escape"));
        assert!(!s.is_query_selected());
        assert!(s.query.is_empty());
    }

    #[test]
    fn grid_click_hands_every_key_to_the_terminal() {
        let mut s = SearchState::new();
        s.open();
        assert!(s.is_input_focused());
        s.query.push_str("needle");

        // Back in the grid: the bar stays open, but select-all, typing, Esc
        // and paste all belong to the terminal again.
        s.blur_input();
        assert!(s.active);
        assert!(!s.is_input_focused());
        for chord in ["cmd-a", "cmd-c", "cmd-v", "x", "backspace", "escape", "enter"] {
            assert!(
                matches!(s.handle_key(&key(chord)), SearchKeyOutcome::Pass),
                "{chord} must reach the terminal"
            );
        }
        assert_eq!(s.query, "needle", "the query is untouched");
        assert!(!s.is_query_selected());

        // Clicking the bar takes the keyboard back.
        s.focus_input();
        assert!(matches!(
            s.handle_key(&key("cmd-a")),
            SearchKeyOutcome::SelectionChanged
        ));
        assert!(s.is_query_selected());

        // Cmd+F from the grid refocuses the box with its query selected, so
        // typing replaces the old needle.
        s.blur_input();
        s.open();
        assert!(s.is_input_focused());
        assert!(s.is_query_selected());
        s.handle_key(&key("z"));
        assert_eq!(s.query, "z");

        // Closing drops focus with the rest of the state.
        s.close();
        assert!(!s.is_input_focused());
    }

    fn hit(row: usize) -> MatchRange {
        MatchRange {
            row,
            col_start: 0,
            col_end: 1,
        }
    }

    #[test]
    fn follow_delta_scrolls_to_offscreen_matches_only() {
        // 100 history rows above a 10-row viewport pinned to the tail.
        let mut s = SearchState::new();
        s.active = true;
        s.history_len = 100;
        s.current_index = Some(0);

        // Match on the live tail (row 105) is already visible → no scroll.
        s.matches = vec![hit(105)];
        assert_eq!(s.follow_delta(10, 0), None);

        // Match up in history (row 20): center it → window top 15 →
        // offset 85, so scroll back 85 lines.
        s.matches = vec![hit(20)];
        assert_eq!(s.follow_delta(10, 0), Some(85));

        // Already scrolled to offset 85: same match needs no further scroll.
        assert_eq!(s.follow_delta(10, 85), None);

        // From offset 85, a tail match (row 105) is below the window;
        // desired top (100) caps the offset at 0 → scroll forward 85.
        s.matches = vec![hit(105)];
        assert_eq!(s.follow_delta(10, 85), Some(-85));

        // No matches → never scrolls.
        s.matches.clear();
        s.current_index = None;
        assert_eq!(s.follow_delta(10, 50), None);
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
