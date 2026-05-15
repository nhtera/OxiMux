//! Renderable view of a session's current state.
//!
//! Step 1-2 ships an empty / size-only snapshot. Step 3 will fill it from
//! the `alacritty_terminal` grid (rows, cursor, dirty mask) and add a
//! scrollback ring window. Step 4 maps rows to gpui-component rendering.

/// One visible cell. Step 3 populates color + style; step 1-2 only fills
/// `ch` from raw output for any consumer that wants a quick read.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub ch: char,
}

/// A frame-aligned snapshot. Cheap to clone (Vec<Row> small under 24x80).
#[derive(Debug, Clone, Default)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: (u16, u16),
    pub cells: Vec<Vec<Cell>>,
}

impl TerminalSnapshot {
    pub fn empty(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cursor: (0, 0),
            cells: Vec::new(),
        }
    }
}
