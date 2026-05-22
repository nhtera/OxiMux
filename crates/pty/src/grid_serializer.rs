//! Serialize an `alacritty_terminal::Term` grid (+ optional scrollback) into
//! ANSI bytes that can be replayed into a fresh `vte::ansi::Processor` to
//! reconstruct the visible state. Used by the per-pane scrollback persistence
//! (Phase 4 step 16) so plain terminals restore with their prior output
//! intact after an app restart.
//!
//! Output shape (top-down, one row per terminal row):
//! - SGR resets between cells whose attributes differ from the running state.
//! - Cells written as UTF-8 (`cell.c`). WIDE_CHAR_SPACER cells are skipped
//!   because the preceding WIDE_CHAR cell already advanced the cursor by 2.
//! - `\r\n` between rows (no trailing newline).
//! - Final `ESC [ row ; col H` (CUP, 1-based) placing the cursor where it
//!   was at capture time.
//!
//! Fidelity is "looks the same in normal usage" — bracketed-paste mode,
//! alt-screen state, mouse-tracking modes, hyperlinks, and dotted/dashed
//! underline variants are NOT round-tripped. Matches the xterm.js
//! `serializeAddon` limitations.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};

/// Options controlling how much state the serializer emits.
#[derive(Debug, Clone, Copy, Default)]
pub struct SerializeOptions {
    /// Rows of scrollback to include above the visible grid. `0` = visible
    /// only. Clamped at runtime to whatever the grid actually retains.
    pub scrollback: usize,
}

/// Serialize a term into ANSI bytes. Pure function; safe to call off the
/// UI thread.
pub fn serialize_term(term: &Term<VoidListener>, opts: SerializeOptions) -> Vec<u8> {
    let cols = term.columns();
    let rows = term.screen_lines();
    let history = term.grid().history_size();
    let scrollback = opts.scrollback.min(history);

    // Worst case: every cell is 1 byte + a short SGR run per row.
    let mut out: Vec<u8> = Vec::with_capacity((scrollback + rows) * (cols + 16));

    // SGR-zero baseline so the receiver doesn't inherit prior state.
    out.extend_from_slice(b"\x1b[0m");

    let mut current = SgrState::default();
    let start_line = -(scrollback as i32);
    let end_line = rows as i32;

    let mut first_row = true;
    for line_idx in start_line..end_line {
        if !first_row {
            out.extend_from_slice(b"\r\n");
        }
        first_row = false;
        emit_row(&mut out, &mut current, term, line_idx, cols);
    }

    // Final cursor position. alacritty cursor.point.line is relative to the
    // visible top, so it's already 0..rows. Always positive on the way out.
    let cursor = term.grid().cursor.point;
    let cursor_row_1based = (cursor.line.0.max(0) as usize + 1).min(rows.max(1));
    let cursor_col_1based = (cursor.column.0 + 1).min(cols.max(1));
    out.extend_from_slice(format!("\x1b[{cursor_row_1based};{cursor_col_1based}H").as_bytes());

    out
}

/// Serialize with an automatic binary-search cap on `scrollback` so the
/// final byte length fits under `max_bytes`. The visible grid is always
/// emitted regardless of the cap.
pub fn serialize_term_capped(term: &Term<VoidListener>, max_bytes: usize) -> Vec<u8> {
    let history = term.grid().history_size();
    let full = serialize_term(
        term,
        SerializeOptions {
            scrollback: history,
        },
    );
    if full.len() <= max_bytes {
        return full;
    }
    // Binary-search the largest scrollback that fits. The output size is
    // monotonically non-decreasing in `scrollback` so binary search is sound.
    let mut lo: usize = 0;
    let mut hi: usize = history;
    let mut best = serialize_term(term, SerializeOptions { scrollback: 0 });
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let candidate = serialize_term(term, SerializeOptions { scrollback: mid });
        if candidate.len() <= max_bytes {
            best = candidate;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

fn emit_row(
    out: &mut Vec<u8>,
    current: &mut SgrState,
    term: &Term<VoidListener>,
    line_idx: i32,
    cols: usize,
) {
    let row = &term.grid()[Line(line_idx)];
    for col_idx in 0..cols {
        let cell = &row[Column(col_idx)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            // The preceding WIDE_CHAR cell emitted a 2-column char; skip
            // the spacer so we don't double-advance.
            continue;
        }
        let next = SgrState::from_cell(cell);
        if next != *current {
            emit_sgr_diff(out, current, &next);
            *current = next;
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
    }
}

/// Running SGR state. Equality drives the per-cell diff emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SgrState {
    fg: SgrColor,
    bg: SgrColor,
    bold: bool,
    italic: bool,
    underline: bool,
    dim: bool,
    inverse: bool,
    hidden: bool,
    strikeout: bool,
}

impl Default for SgrState {
    fn default() -> Self {
        Self {
            fg: SgrColor::Default,
            bg: SgrColor::Default,
            bold: false,
            italic: false,
            underline: false,
            dim: false,
            inverse: false,
            hidden: false,
            strikeout: false,
        }
    }
}

impl SgrState {
    fn from_cell(c: &Cell) -> Self {
        Self {
            fg: SgrColor::from_ansi_fg(c.fg),
            bg: SgrColor::from_ansi_bg(c.bg),
            bold: c.flags.contains(Flags::BOLD),
            italic: c.flags.contains(Flags::ITALIC),
            underline: c.flags.intersects(Flags::ALL_UNDERLINES),
            dim: c.flags.contains(Flags::DIM),
            inverse: c.flags.contains(Flags::INVERSE),
            hidden: c.flags.contains(Flags::HIDDEN),
            strikeout: c.flags.contains(Flags::STRIKEOUT),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SgrColor {
    Default,
    Named(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl SgrColor {
    fn from_ansi_fg(c: AnsiColor) -> Self {
        match c {
            AnsiColor::Named(n) => named_to_sgr_fg(n),
            AnsiColor::Indexed(i) => match i {
                0..=15 => SgrColor::Named(index_to_named_fg(i)),
                _ => SgrColor::Indexed(i),
            },
            AnsiColor::Spec(rgb) => SgrColor::Rgb(rgb.r, rgb.g, rgb.b),
        }
    }
    fn from_ansi_bg(c: AnsiColor) -> Self {
        match c {
            AnsiColor::Named(n) => named_to_sgr_bg(n),
            AnsiColor::Indexed(i) => match i {
                0..=15 => SgrColor::Named(index_to_named_bg(i)),
                _ => SgrColor::Indexed(i),
            },
            AnsiColor::Spec(rgb) => SgrColor::Rgb(rgb.r, rgb.g, rgb.b),
        }
    }
}

fn named_to_sgr_fg(n: NamedColor) -> SgrColor {
    use NamedColor::*;
    match n {
        Foreground | DimForeground | Cursor => SgrColor::Default,
        Background => SgrColor::Default,
        Black | DimBlack => SgrColor::Named(30),
        Red | DimRed => SgrColor::Named(31),
        Green | DimGreen => SgrColor::Named(32),
        Yellow | DimYellow => SgrColor::Named(33),
        Blue | DimBlue => SgrColor::Named(34),
        Magenta | DimMagenta => SgrColor::Named(35),
        Cyan | DimCyan => SgrColor::Named(36),
        White | DimWhite => SgrColor::Named(37),
        BrightBlack => SgrColor::Named(90),
        BrightRed => SgrColor::Named(91),
        BrightGreen => SgrColor::Named(92),
        BrightYellow => SgrColor::Named(93),
        BrightBlue => SgrColor::Named(94),
        BrightMagenta => SgrColor::Named(95),
        BrightCyan => SgrColor::Named(96),
        BrightWhite | BrightForeground => SgrColor::Named(97),
    }
}

fn named_to_sgr_bg(n: NamedColor) -> SgrColor {
    use NamedColor::*;
    match n {
        Foreground | DimForeground | Cursor | Background => SgrColor::Default,
        Black | DimBlack => SgrColor::Named(40),
        Red | DimRed => SgrColor::Named(41),
        Green | DimGreen => SgrColor::Named(42),
        Yellow | DimYellow => SgrColor::Named(43),
        Blue | DimBlue => SgrColor::Named(44),
        Magenta | DimMagenta => SgrColor::Named(45),
        Cyan | DimCyan => SgrColor::Named(46),
        White | DimWhite => SgrColor::Named(47),
        BrightBlack => SgrColor::Named(100),
        BrightRed => SgrColor::Named(101),
        BrightGreen => SgrColor::Named(102),
        BrightYellow => SgrColor::Named(103),
        BrightBlue => SgrColor::Named(104),
        BrightMagenta => SgrColor::Named(105),
        BrightCyan => SgrColor::Named(106),
        BrightWhite | BrightForeground => SgrColor::Named(107),
    }
}

fn index_to_named_fg(i: u8) -> u8 {
    if i < 8 { 30 + i } else { 90 + (i - 8) }
}

fn index_to_named_bg(i: u8) -> u8 {
    if i < 8 { 40 + i } else { 100 + (i - 8) }
}

fn emit_sgr_diff(out: &mut Vec<u8>, _old: &SgrState, new: &SgrState) {
    // Brute force: SGR 0 then re-apply every active attribute on `new`. The
    // incremental encoding xterm.js does is fiddly (turning off bold without
    // also turning off dim, etc.); cells with attribute changes are
    // uncommon in normal terminal output so the reset+reapply cost is small.
    let mut codes: Vec<String> = vec!["0".into()];
    if new.bold {
        codes.push("1".into());
    }
    if new.dim {
        codes.push("2".into());
    }
    if new.italic {
        codes.push("3".into());
    }
    if new.underline {
        codes.push("4".into());
    }
    if new.inverse {
        codes.push("7".into());
    }
    if new.hidden {
        codes.push("8".into());
    }
    if new.strikeout {
        codes.push("9".into());
    }
    push_color(&mut codes, new.fg, true);
    push_color(&mut codes, new.bg, false);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(codes.join(";").as_bytes());
    out.push(b'm');
}

fn push_color(codes: &mut Vec<String>, c: SgrColor, fg: bool) {
    match c {
        SgrColor::Default => {} // baseline (SGR 39/49 implied by the leading SGR 0)
        SgrColor::Named(n) => codes.push(n.to_string()),
        SgrColor::Indexed(i) => {
            codes.push(if fg { "38".into() } else { "48".into() });
            codes.push("5".into());
            codes.push(i.to_string());
        }
        SgrColor::Rgb(r, g, b) => {
            codes.push(if fg { "38".into() } else { "48".into() });
            codes.push("2".into());
            codes.push(r.to_string());
            codes.push(g.to_string());
            codes.push(b.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TerminalState;

    fn replay_into_new_term(bytes: &[u8], cols: u16, rows: u16) -> TerminalState {
        let mut state = TerminalState::new(cols, rows, 100);
        state.advance(bytes);
        state
    }

    fn visible_text(state: &TerminalState) -> String {
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        state.fill_snapshot(&mut snap);
        snap.cells
            .iter()
            .map(|row| {
                let s: String = row.iter().map(|c| c.ch).collect();
                s.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn roundtrip_simple_text_preserves_visible_content() {
        let original = replay_into_new_term(b"hello\r\nworld\r\n", 20, 5);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        let restored = replay_into_new_term(&bytes, 20, 5);
        let orig_text = visible_text(&original);
        let restored_text = visible_text(&restored);
        assert_eq!(orig_text, restored_text);
        assert!(orig_text.contains("hello"));
        assert!(orig_text.contains("world"));
    }

    #[test]
    fn roundtrip_preserves_named_color() {
        // Red on default bg, then a default cell.
        let original = replay_into_new_term(b"\x1b[31mR\x1b[0m.", 10, 3);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        let restored = replay_into_new_term(&bytes, 10, 3);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        // Cell (0, 0) should be 'R' with red fg.
        assert_eq!(snap.cells[0][0].ch, 'R');
        assert_eq!(
            snap.cells[0][0].fg,
            crate::snapshot::CellColor::Named(crate::snapshot::NamedColor16::Red),
        );
        // Cell (0, 1) should be '.' with default fg.
        assert_eq!(snap.cells[0][1].ch, '.');
        assert_eq!(snap.cells[0][1].fg, crate::snapshot::CellColor::Default);
    }

    #[test]
    fn roundtrip_preserves_rgb_truecolor() {
        let original = replay_into_new_term(b"\x1b[38;2;200;50;25mX", 10, 3);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        let restored = replay_into_new_term(&bytes, 10, 3);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        assert_eq!(snap.cells[0][0].ch, 'X');
        assert_eq!(
            snap.cells[0][0].fg,
            crate::snapshot::CellColor::Rgb(200, 50, 25),
        );
    }

    #[test]
    fn roundtrip_preserves_cursor_position() {
        // Move cursor to row 3 col 5 (1-based) → (2, 4) 0-based.
        let original = replay_into_new_term(b"\x1b[3;5H", 20, 10);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        let restored = replay_into_new_term(&bytes, 20, 10);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        assert_eq!(snap.cursor, (2, 4));
    }

    #[test]
    fn capped_serialize_returns_some_output_when_under_cap() {
        let original = replay_into_new_term(b"hello", 20, 5);
        let bytes = serialize_term_capped(original.term_for_test(), 4096);
        assert!(!bytes.is_empty());
        assert!(bytes.len() <= 4096);
    }

    #[test]
    fn capped_serialize_shrinks_when_over_cap() {
        // Fill enough scrollback that the unbounded output exceeds the cap.
        let mut input: Vec<u8> = Vec::new();
        for i in 0..500 {
            // 80 wide row + CR/LF.
            input.extend_from_slice(format!("row {i:04} {}\r\n", "x".repeat(60)).as_bytes());
        }
        let original = replay_into_new_term(&input, 80, 24);
        let small_cap = 2048;
        let bytes = serialize_term_capped(original.term_for_test(), small_cap);
        assert!(
            bytes.len() <= small_cap,
            "capped output {} should be <= {}",
            bytes.len(),
            small_cap,
        );
    }

    #[test]
    fn empty_grid_serializes_without_panicking() {
        let original = TerminalState::new(80, 24, 100);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        // Should be at least the SGR 0 prefix + one CUP.
        assert!(bytes.starts_with(b"\x1b[0m"));
        assert!(bytes.ends_with(b"H"));
    }
}
