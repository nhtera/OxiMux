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
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.size.cols = cols as usize;
        self.size.rows = rows as usize;
        self.term.resize(self.size);
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
}
