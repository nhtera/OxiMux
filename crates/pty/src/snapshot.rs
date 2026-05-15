//! Renderable view of a session's current state.
//!
//! `TerminalSnapshot` is a frame-aligned, render-side copy of the alacritty
//! grid. It is intentionally decoupled from `alacritty_terminal` types so the
//! app crate doesn't depend on vte/alacritty — that boundary stops bleeding
//! ansi-parser internals into the GPUI layer and keeps the snapshot replay-
//! friendly for fixture tests later.

/// 16 standard ANSI named colors. Bright variants are folded in so the
/// renderer needs one palette entry per index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedColor16 {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

/// Resolved cell color. `Default` defers to the theme — distinct from `White`
/// or `Black` so the renderer can paint the canvas itself instead of forcing
/// a literal color in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellColor {
    #[default]
    Default,
    Named(NamedColor16),
    /// xterm 256-color palette index (0..=255). Indices 0..=15 mirror the
    /// 16 named colors; 16..=231 is the 6×6×6 cube; 232..=255 is grayscale.
    Indexed(u8),
    /// 24-bit truecolor from `Color::Spec`.
    Rgb(u8, u8, u8),
}

/// One visible cell. `inverse` mirrors the SGR 7 / DECSCNM bit — render swaps
/// fg/bg when true. Bold/italic/underline live on `Cell` only when the
/// renderer needs them; right now we only carry `inverse` because that's what
/// the cursor relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub inverse: bool,
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
