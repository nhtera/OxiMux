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

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::snapshot::{Cell, TerminalSnapshot};

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
                row_cells.push(Cell { ch: cell.c });
            }
            snap.cells.push(row_cells);
        }
    }
}
