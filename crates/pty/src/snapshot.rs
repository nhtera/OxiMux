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

/// One visible cell. SGR attributes the renderer acts on:
/// `inverse` (SGR 7 / DECSCNM — render swaps fg/bg), `dim` (SGR 2 — fg alpha
/// ×~0.7), `bold` (SGR 1 — heavier font weight), `italic` (SGR 3 — slanted),
/// `underline` (SGR 4 — drawn line under the glyph), `strikethrough` (SGR 9 —
/// line through the glyph), and `hidden` (SGR 8 — glyph painted in its own bg
/// so it is invisible but still selectable/copyable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub inverse: bool,
    pub dim: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub hidden: bool,
    /// True when this cell holds a double-width (CJK / wide-emoji) glyph.
    /// The next column carries a `wide_spacer` cell; renderers paint this
    /// glyph spanning both columns.
    pub wide: bool,
    /// True for the trailing (or leading) column occupied by an adjacent
    /// wide glyph. The renderer skips this cell — its column is already
    /// painted by the wide glyph — and selection text omits it so copy
    /// yields one character, not "char + space".
    pub wide_spacer: bool,
}

/// Cursor shape the app requested via DECSCUSR, decoupled from alacritty's
/// `CursorShape` so the app crate never depends on vte. `Hidden` covers both
/// `CursorShape::Hidden` and DECTCEM-off; the renderer draws nothing for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShapeKind {
    #[default]
    Block,
    Bar,
    Underline,
    Hidden,
}

/// An OSC 8 explicit hyperlink covering an inclusive `[col_start, col_end]`
/// run on a visible `row`. The `uri` may differ from the displayed text, so
/// these are resolved before plain-text link detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkSpan {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub uri: String,
}

/// A frame-aligned snapshot. Cheap to clone (Vec<Row> small under 24x80).
#[derive(Debug, Clone, Default)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: (u16, u16),
    pub cells: Vec<Vec<Cell>>,
    /// Lines scrolled back from the live tail. `0` = pinned to the bottom
    /// (live). `> 0` = the user has scrolled up into history; drives the
    /// "scrolled-up" indicator and tells callers the cursor may be hidden.
    pub display_offset: usize,
    /// Shape the app asked the cursor to take (DECSCUSR / DECTCEM).
    pub cursor_shape: CursorShapeKind,
    /// Current scrollback length (history rows) at snapshot time. With
    /// `display_offset` this maps an absolute command-mark line to a screen
    /// row: `screen_row = mark_line - history_len + display_offset`.
    pub history_len: usize,
    /// OSC 8 explicit hyperlink spans on the visible rows. Empty on the common
    /// path (no app-emitted links); resolved before plain-text detection.
    pub links: Vec<HyperlinkSpan>,
}

impl TerminalSnapshot {
    pub fn empty(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cursor: (0, 0),
            cells: Vec::new(),
            display_offset: 0,
            cursor_shape: CursorShapeKind::Block,
            history_len: 0,
            links: Vec::new(),
        }
    }

    /// Plain text for visible rows in `[start_row, end_row]` inclusive.
    /// Right-trims each row, joins with `\n`, drops `wide_spacer` cells
    /// (their column is owned by the adjacent wide glyph) and replaces
    /// `\0` with space. Out-of-range coords clamp silently to the grid.
    /// Used by the agent-context extractors and any future "send rows to
    /// X" path that needs the visible viewport as a string.
    pub fn rows_text(&self, start_row: usize, end_row: usize) -> String {
        if self.cells.is_empty() {
            return String::new();
        }
        let last = self.cells.len() - 1;
        let r0 = start_row.min(last);
        let r1 = end_row.min(last);
        if r1 < r0 {
            return String::new();
        }
        let mut out = String::with_capacity((r1 - r0 + 1) * 80);
        for (idx, row) in self.cells.iter().enumerate().skip(r0).take(r1 - r0 + 1) {
            let mut line = String::with_capacity(row.len());
            for cell in row {
                if cell.wide_spacer {
                    continue;
                }
                let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
                line.push(ch);
            }
            out.push_str(line.trim_end());
            if idx < r1 {
                out.push('\n');
            }
        }
        out
    }

    /// All visible rows as text, trailing blank rows stripped. The cheap
    /// "what's on screen right now" extractor — feeds the agent context
    /// when the user wants to share the whole viewport.
    pub fn get_content(&self) -> String {
        if self.cells.is_empty() {
            return String::new();
        }
        let full = self.rows_text(0, self.cells.len() - 1);
        full.trim_end_matches('\n').to_string()
    }

    /// Up to `n` most-recent NON-empty rows as text. Walks rows from the
    /// bottom and stops once it has collected `n` non-empty lines or
    /// reached the top. Skips empty rows entirely so a freshly-opened
    /// shell with one prompt line returns just that line, not 23 blanks
    /// followed by the prompt.
    pub fn last_n_non_empty_lines(&self, n: usize) -> String {
        if n == 0 || self.cells.is_empty() {
            return String::new();
        }
        let mut collected: Vec<String> = Vec::with_capacity(n);
        for row in self.cells.iter().rev() {
            if collected.len() == n {
                break;
            }
            let mut line = String::with_capacity(row.len());
            for cell in row {
                if cell.wide_spacer {
                    continue;
                }
                let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
                line.push(ch);
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            collected.push(trimmed.to_string());
        }
        collected.reverse();
        collected.join("\n")
    }

    /// Map an absolute history line (the unit P8 command marks are stored
    /// in: `history_len + cursor_line` at the time the mark fired) to a
    /// screen row index. Returns `None` when the line has scrolled off the
    /// visible viewport (above) or sits below the live tail (impossible
    /// under normal use).
    ///
    /// Formula: `screen_row = abs_line - history_len + display_offset`.
    /// `history_len` is the scrollback length at snapshot time, so this
    /// answers "where in the current viewport is line L?".
    pub fn abs_line_to_screen_row(&self, abs_line: u64) -> Option<usize> {
        let history = self.history_len as i64;
        let offset = self.display_offset as i64;
        let row = abs_line as i64 - history + offset;
        if row < 0 {
            return None;
        }
        let rows = self.cells.len();
        if (row as usize) >= rows {
            return None;
        }
        Some(row as usize)
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

    fn row_of(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|ch| Cell {
                ch,
                ..Cell::default()
            })
            .collect()
    }

    fn snap_with_rows(rows: Vec<Vec<Cell>>) -> TerminalSnapshot {
        let cols = rows.first().map(|r| r.len()).unwrap_or(0) as u16;
        let rows_len = rows.len() as u16;
        TerminalSnapshot {
            cols,
            rows: rows_len,
            cells: rows,
            ..TerminalSnapshot::empty(cols, rows_len)
        }
    }

    #[test]
    fn rows_text_right_trims_and_joins() {
        let snap = snap_with_rows(vec![
            row_of("hello   "),
            row_of("world!  "),
            row_of("        "),
        ]);
        assert_eq!(snap.rows_text(0, 2), "hello\nworld!\n");
        assert_eq!(snap.rows_text(0, 1), "hello\nworld!");
    }

    #[test]
    fn rows_text_clamps_out_of_range() {
        let snap = snap_with_rows(vec![row_of("a"), row_of("b")]);
        // end_row > last clamps to last; reversed range yields empty.
        assert_eq!(snap.rows_text(0, 99), "a\nb");
        assert_eq!(snap.rows_text(5, 99), "b");
    }

    #[test]
    fn rows_text_drops_wide_spacer() {
        let wide = Cell {
            ch: '你',
            wide: true,
            ..Cell::default()
        };
        let spacer = Cell {
            wide_spacer: true,
            ..Cell::default()
        };
        let row = vec![
            wide,
            spacer,
            Cell {
                ch: 'a',
                ..Cell::default()
            },
        ];
        let snap = snap_with_rows(vec![row]);
        assert_eq!(snap.rows_text(0, 0), "你a", "spacer must not produce a space");
    }

    #[test]
    fn get_content_strips_trailing_blank_rows() {
        let snap = snap_with_rows(vec![
            row_of("$ ls   "),
            row_of("a b c  "),
            row_of("       "),
            row_of("       "),
        ]);
        assert_eq!(snap.get_content(), "$ ls\na b c");
    }

    #[test]
    fn last_n_non_empty_lines_skips_blanks_and_caps_at_n() {
        let snap = snap_with_rows(vec![
            row_of("first  "),
            row_of("       "),
            row_of("second "),
            row_of("       "),
            row_of("third  "),
        ]);
        assert_eq!(snap.last_n_non_empty_lines(2), "second\nthird");
        assert_eq!(snap.last_n_non_empty_lines(5), "first\nsecond\nthird");
        assert_eq!(snap.last_n_non_empty_lines(0), "");
    }

    #[test]
    fn abs_line_to_screen_row_maps_via_history_and_offset() {
        let mut snap = snap_with_rows(vec![row_of("a"), row_of("b"), row_of("c")]);
        snap.history_len = 100;
        snap.display_offset = 0;
        // Live tail: abs_line 100..=102 → screen rows 0..=2.
        assert_eq!(snap.abs_line_to_screen_row(100), Some(0));
        assert_eq!(snap.abs_line_to_screen_row(102), Some(2));
        // Above viewport top → None.
        assert_eq!(snap.abs_line_to_screen_row(99), None);
        // Below viewport bottom → None.
        assert_eq!(snap.abs_line_to_screen_row(103), None);

        // Scrolled up by 2: mark at abs_line 100 is now at screen row 2.
        snap.display_offset = 2;
        assert_eq!(snap.abs_line_to_screen_row(100), Some(2));
        assert_eq!(snap.abs_line_to_screen_row(98), Some(0));
    }

    #[test]
    fn cell_equality_compares_all_fields() {
        let base = Cell {
            ch: 'a',
            fg: CellColor::Named(NamedColor16::Red),
            bg: CellColor::Default,
            inverse: false,
            dim: false,
            ..Cell::default()
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
