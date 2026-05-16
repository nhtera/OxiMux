//! Terminal state machine — `alacritty_terminal` wrapper.
//!
//! Owns the `Term<VoidListener>` + a `vte::ansi::Processor`. Feeds bytes
//! from the PTY through the parser into the grid. Renders into our flat
//! `TerminalSnapshot` on demand. Bounded scrollback via the `Config`.
//!
//! VoidListener is fine for v1 — the events alacritty emits are mostly
//! relevant to its own GUI (PTY write, MouseCursorDirty, Bell). When we
//! need bell or title forwarding, swap in a custom EventListener that
//! pushes into our `TerminalEvent` channel.
//!
//! Color extraction maps alacritty's `Color` (Named/Spec/Indexed) onto our
//! own `CellColor` so the app crate never has to depend on vte/alacritty
//! directly. Indices 0..=15 collapse onto `NamedColor16`; everything else
//! flows through `Indexed` for the renderer's 256-palette lookup.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};

use crate::snapshot::{Cell, CellColor, NamedColor16, TerminalSnapshot};

#[derive(Debug, Clone, Copy)]
struct SizeInfo {
    cols: usize,
    rows: usize,
    history: usize,
}

impl Dimensions for SizeInfo {
    fn columns(&self) -> usize {
        self.cols
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn total_lines(&self) -> usize {
        self.rows + self.history
    }
}

pub struct TerminalState {
    term: Term<VoidListener>,
    parser: Processor,
    size: SizeInfo,
}

impl TerminalState {
    /// Build a fresh state. `scrollback` is the maximum number of off-screen
    /// rows retained — plan target is 5000.
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let size = SizeInfo {
            cols: cols as usize,
            rows: rows as usize,
            history: scrollback,
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let term = Term::new(config, &size, VoidListener);
        Self {
            term,
            parser: Processor::new(),
            size,
        }
    }

    /// Feed PTY bytes through the ANSI parser into the grid.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// True when the shell has requested DECSET 2004 (bracketed paste).
    /// The renderer wraps pasted text with `\e[200~` / `\e[201~` only when
    /// this is set — modern shells (zsh, bash with readline ≥ 8) opt in;
    /// raw `cat` does not, and double-wrapping there would leak escape
    /// sequences as literal text.
    pub fn is_bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Resize the grid. Called whenever the pane's render area changes.
    ///
    /// Bypasses `Term::resize` and calls `grid_mut().resize(false, ..)`
    /// directly so alacritty's reflow doesn't run on shrink. The default
    /// `Term::resize` path passes `reflow = !is_alt_screen`, and for
    /// non-WRAPLINE rows (e.g. plain `ls` output) the reflow shrinker
    /// splits overflow off the right and prepends it to the next row in
    /// reverse iteration order — which makes a 3-column ls layout look
    /// scrambled in a narrower pane after a split. Terminal.app / iTerm2
    /// instead truncate rows on the right with no rearrangement; this
    /// matches that semantics.
    ///
    /// Trade-offs we accept by skipping `Term::resize`:
    /// - `inactive_grid` (alt screen buffer) stays at its old dimensions
    ///   until the user enters alt-screen mode, at which point the next
    ///   resize tick fixes it.
    /// - `vi_mode_cursor` line offset isn't re-anchored. We don't expose
    ///   vi mode in v1.
    /// - Tab stops aren't reset to the new width. We don't expose tabs
    ///   either; \t lands on default column boundaries inside the grid.
    /// - Selection isn't invalidated. We don't have selection in v1.
    ///
    /// Once any of those features ship, port the relevant lines from
    /// `Term::resize` back here and gate them on actual feature use.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.size.cols = cols as usize;
        self.size.rows = rows as usize;
        self.term
            .grid_mut()
            .resize(false, self.size.rows, self.size.cols);
        // Clamp cursor so a subsequent write doesn't index past the new
        // right edge. `grid.resize(false, ..)` truncates rows but doesn't
        // touch the cursor; without this clamp a wide-then-narrow resize
        // can leave `cursor.point.column` >= cols and the next character
        // write panics inside alacritty's grid indexing.
        let cursor = &mut self.term.grid_mut().cursor;
        if cursor.point.column.0 >= self.size.cols {
            cursor.point.column = Column(self.size.cols.saturating_sub(1));
        }
        if (cursor.point.line.0 as usize) >= self.size.rows {
            cursor.point.line = Line(self.size.rows as i32 - 1);
        }
    }

    /// Return a row-major grid covering `history_rows` of scrollback prepended
    /// to the current visible rows. Caller iterates this for substring search
    /// (Phase 1 step 8). Bounded by 5000 history + ~32 rows × ~200 cols ≈
    /// 1M cells worst case; allocation cost is the price of decoupling search
    /// from the live grid lock.
    ///
    /// Clamps to `term.grid().history_size()` so a freshly-spawned session
    /// with empty history doesn't index `Line(-N)` with `N > 0` history rows
    /// — alacritty panics in that case.
    pub fn fill_search_grid(&self) -> Vec<Vec<Cell>> {
        let history = self.term.grid().history_size();
        let total = history + self.size.rows;
        let mut out = Vec::with_capacity(total);
        let start_line = -(history as i32);
        let end_line = self.size.rows as i32;
        for line_idx in start_line..end_line {
            let row = &self.term.grid()[Line(line_idx)];
            let mut row_cells = Vec::with_capacity(self.size.cols);
            for col_idx in 0..self.size.cols {
                let cell = &row[Column(col_idx)];
                row_cells.push(map_cell(cell));
            }
            out.push(row_cells);
        }
        out
    }

    /// Populate `snap` with the current visible grid + cursor.
    /// Allocates one `Vec<Cell>` per row; cheap at 24-200 rows.
    pub fn fill_snapshot(&self, snap: &mut TerminalSnapshot) {
        snap.cols = self.size.cols as u16;
        snap.rows = self.size.rows as u16;

        let cursor_point = self.term.grid().cursor.point;
        snap.cursor = (
            cursor_point.line.0.max(0) as u16,
            cursor_point.column.0 as u16,
        );

        snap.cells.clear();
        snap.cells.reserve(self.size.rows);
        for line_idx in 0..self.size.rows as i32 {
            let row = &self.term.grid()[Line(line_idx)];
            let mut row_cells = Vec::with_capacity(self.size.cols);
            for col_idx in 0..self.size.cols {
                let cell = &row[Column(col_idx)];
                row_cells.push(map_cell(cell));
            }
            snap.cells.push(row_cells);
        }
    }
}

fn map_cell(cell: &alacritty_terminal::term::cell::Cell) -> Cell {
    Cell {
        ch: cell.c,
        fg: map_color(cell.fg),
        bg: map_color(cell.bg),
        inverse: cell.flags.contains(Flags::INVERSE),
        dim: cell.flags.contains(Flags::DIM),
    }
}

fn map_color(color: AnsiColor) -> CellColor {
    match color {
        AnsiColor::Named(named) => map_named(named),
        AnsiColor::Indexed(idx) => match idx {
            0..=15 => map_named_by_index(idx),
            other => CellColor::Indexed(other),
        },
        AnsiColor::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn map_named(named: NamedColor) -> CellColor {
    use NamedColor::*;
    match named {
        Foreground | DimForeground => CellColor::Default,
        Background => CellColor::Default,
        BrightForeground => CellColor::Named(NamedColor16::BrightWhite),
        Cursor => CellColor::Default,
        Black | DimBlack => CellColor::Named(NamedColor16::Black),
        Red | DimRed => CellColor::Named(NamedColor16::Red),
        Green | DimGreen => CellColor::Named(NamedColor16::Green),
        Yellow | DimYellow => CellColor::Named(NamedColor16::Yellow),
        Blue | DimBlue => CellColor::Named(NamedColor16::Blue),
        Magenta | DimMagenta => CellColor::Named(NamedColor16::Magenta),
        Cyan | DimCyan => CellColor::Named(NamedColor16::Cyan),
        White | DimWhite => CellColor::Named(NamedColor16::White),
        BrightBlack => CellColor::Named(NamedColor16::BrightBlack),
        BrightRed => CellColor::Named(NamedColor16::BrightRed),
        BrightGreen => CellColor::Named(NamedColor16::BrightGreen),
        BrightYellow => CellColor::Named(NamedColor16::BrightYellow),
        BrightBlue => CellColor::Named(NamedColor16::BrightBlue),
        BrightMagenta => CellColor::Named(NamedColor16::BrightMagenta),
        BrightCyan => CellColor::Named(NamedColor16::BrightCyan),
        BrightWhite => CellColor::Named(NamedColor16::BrightWhite),
    }
}

fn map_named_by_index(idx: u8) -> CellColor {
    let n = match idx {
        0 => NamedColor16::Black,
        1 => NamedColor16::Red,
        2 => NamedColor16::Green,
        3 => NamedColor16::Yellow,
        4 => NamedColor16::Blue,
        5 => NamedColor16::Magenta,
        6 => NamedColor16::Cyan,
        7 => NamedColor16::White,
        8 => NamedColor16::BrightBlack,
        9 => NamedColor16::BrightRed,
        10 => NamedColor16::BrightGreen,
        11 => NamedColor16::BrightYellow,
        12 => NamedColor16::BrightBlue,
        13 => NamedColor16::BrightMagenta,
        14 => NamedColor16::BrightCyan,
        _ => NamedColor16::BrightWhite,
    };
    CellColor::Named(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_snapshot(state: &TerminalState) -> TerminalSnapshot {
        let mut snap = TerminalSnapshot::default();
        state.fill_snapshot(&mut snap);
        snap
    }

    fn row_text(snap: &TerminalSnapshot, row: usize) -> String {
        snap.cells[row].iter().map(|c| c.ch).collect()
    }

    fn first_non_default_fg(snap: &TerminalSnapshot) -> Option<CellColor> {
        for row in &snap.cells {
            for cell in row {
                if cell.fg != CellColor::Default {
                    return Some(cell.fg);
                }
            }
        }
        None
    }

    #[test]
    fn bracketed_paste_toggles_on_decset_2004() {
        let mut state = TerminalState::new(80, 24, 100);
        assert!(!state.is_bracketed_paste(), "default should be off");

        state.advance(b"\x1b[?2004h");
        assert!(state.is_bracketed_paste(), "DECSET 2004 should enable");

        state.advance(b"\x1b[?2004l");
        assert!(
            !state.is_bracketed_paste(),
            "DECRST 2004 should disable again"
        );
    }

    #[test]
    fn new_dimensions_match() {
        let state = TerminalState::new(80, 24, 100);
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.cells.len(), 24);
        for row in &snap.cells {
            assert_eq!(row.len(), 80);
        }
    }

    #[test]
    fn advance_echo_visible_in_snapshot() {
        let mut state = TerminalState::new(80, 24, 100);
        state.advance(b"Hello");
        let snap = fresh_snapshot(&state);
        assert!(
            row_text(&snap, 0).starts_with("Hello"),
            "row 0 = {:?}",
            row_text(&snap, 0)
        );
    }

    #[test]
    fn cursor_moves_after_cup_sequence() {
        let mut state = TerminalState::new(80, 24, 100);
        // CUP row=5 col=10 (1-based) → (4, 9) 0-based.
        state.advance(b"\x1b[5;10H");
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.cursor, (4, 9));
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut state = TerminalState::new(80, 24, 100);
        state.resize(120, 40);
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.cols, 120);
        assert_eq!(snap.rows, 40);
        assert_eq!(snap.cells.len(), 40);
        assert_eq!(snap.cells[0].len(), 120);
    }

    #[test]
    fn fill_search_grid_covers_at_least_visible_rows() {
        // On a fresh state alacritty currently reports `history_size() == 0`,
        // so the grid is exactly `rows`. Asserting `>=` keeps the test robust
        // if a future alacritty version pre-allocates scrollback.
        let state = TerminalState::new(80, 24, 100);
        let grid = state.fill_search_grid();
        assert!(grid.len() >= 24, "got {} rows", grid.len());
        for row in &grid {
            assert_eq!(row.len(), 80);
        }
    }

    #[test]
    fn map_color_rgb_branch() {
        let mut state = TerminalState::new(80, 24, 100);
        // SGR truecolor + a character so the cell is non-empty.
        state.advance(b"\x1b[38;2;255;0;128mX");
        let snap = fresh_snapshot(&state);
        assert_eq!(
            first_non_default_fg(&snap),
            Some(CellColor::Rgb(255, 0, 128))
        );
    }

    #[test]
    fn map_color_indexed_above_15() {
        let mut state = TerminalState::new(80, 24, 100);
        state.advance(b"\x1b[38;5;200mX");
        let snap = fresh_snapshot(&state);
        assert_eq!(first_non_default_fg(&snap), Some(CellColor::Indexed(200)));
    }

    #[test]
    fn map_color_indexed_below_16_maps_to_named() {
        let mut state = TerminalState::new(80, 24, 100);
        // Index 1 → NamedColor16::Red via map_named_by_index.
        state.advance(b"\x1b[38;5;1mX");
        let snap = fresh_snapshot(&state);
        assert_eq!(
            first_non_default_fg(&snap),
            Some(CellColor::Named(NamedColor16::Red))
        );
    }

    #[test]
    fn dim_flag_flows_through_snapshot() {
        // SGR 2 = faint/dim, then 'X' lands the dim bit on the cell.
        // SGR 22 turns dim off again, so the next 'Y' must NOT be dim.
        let mut state = TerminalState::new(80, 24, 100);
        state.advance(b"\x1b[2mX\x1b[22mY");
        let snap = fresh_snapshot(&state);
        assert!(snap.cells[0][0].dim, "X (after SGR 2) should carry dim");
        assert!(!snap.cells[0][1].dim, "Y (after SGR 22) should clear dim");
    }
}
