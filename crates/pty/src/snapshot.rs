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
/// fg/bg when true. `dim` mirrors SGR 2 — render multiplies fg alpha by ~0.7
/// (the reference terminal 0.69 / Alacritty 0.66 / Kitty 0.75; we sit in the middle). Other
/// SGR attributes (bold/italic/underline) live on `Cell` only when the
/// renderer needs them; right now we only carry the bits the renderer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub inverse: bool,
    pub dim: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_color_default_is_default_variant() {
        assert_eq!(CellColor::default(), CellColor::Default);
    }

    #[test]
    fn cell_default_is_blank_default_colored() {
        let c = Cell::default();
        assert_eq!(c.ch, '\0');
        assert_eq!(c.fg, CellColor::Default);
        assert_eq!(c.bg, CellColor::Default);
        assert!(!c.inverse);
        assert!(!c.dim);
    }

    #[test]
    fn empty_snapshot_preserves_dims_and_zero_cursor() {
        let snap = TerminalSnapshot::empty(80, 24);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.cursor, (0, 0));
        assert!(snap.cells.is_empty());
    }

    #[test]
    fn snapshot_default_is_zero_sized() {
        let snap = TerminalSnapshot::default();
        assert_eq!(snap.cols, 0);
        assert_eq!(snap.rows, 0);
        assert_eq!(snap.cursor, (0, 0));
        assert!(snap.cells.is_empty());
    }

    #[test]
    fn cell_color_variants_are_distinct() {
        // Smoke check that variant equality discriminates correctly.
        assert_ne!(CellColor::Indexed(7), CellColor::Indexed(8));
        assert_ne!(
            CellColor::Named(NamedColor16::Red),
            CellColor::Named(NamedColor16::Green)
        );
        assert_ne!(CellColor::Rgb(0, 0, 0), CellColor::Rgb(1, 0, 0));
        assert_ne!(CellColor::Default, CellColor::Named(NamedColor16::Black));
    }

    #[test]
    fn cell_equality_compares_all_fields() {
        let base = Cell {
            ch: 'a',
            fg: CellColor::Named(NamedColor16::Red),
            bg: CellColor::Default,
            inverse: false,
            dim: false,
        };
        let same = base;
        let diff_inverse = Cell {
            inverse: true,
            ..base
        };
        let diff_dim = Cell { dim: true, ..base };
        assert_eq!(base, same);
        assert_ne!(base, diff_inverse);
        assert_ne!(base, diff_dim);
    }
}
