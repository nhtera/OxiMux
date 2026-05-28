//! Serialize an `alacritty_terminal::Term` grid (+ optional scrollback) into
//! ANSI bytes that can be replayed into a fresh `vte::ansi::Processor` to
//! reconstruct prior output after an app restart (per-pane scrollback
//! persistence).
//!
//! Output shape (top-down, one row per terminal row):
//! - SGR resets between cells whose attributes differ from the running state.
//! - Cells written as UTF-8 (`cell.c`). WIDE_CHAR_SPACER cells are skipped
//!   because the preceding WIDE_CHAR cell already advanced the cursor by 2.
//! - **Trailing blanks are trimmed** from each row, and **trailing empty
//!   rows are dropped** entirely. This is load-bearing for restore: the
//!   replay target is resized to fit the live pane, and `Term::resize`
//!   reflows the content. Emitting full-width-padded rows (the old
//!   behavior) made every line carry `cols` chars, so on a width change
//!   the padding wrapped into blank lines and pushed content around —
//!   the "scrambled prompt / scattered output" restore bug. Trimmed,
//!   short logical lines reflow cleanly at any width.
//! - `\r\n` between emitted rows (no trailing newline).
//! - **No final cursor-position (CUP) sequence.** Restored content is
//!   historical scrollback; the freshly-spawned shell prints its own
//!   prompt and owns the cursor from there. A captured absolute CUP would
//!   only fight the reflow and re-introduce misplacement.
//!
//! Fidelity is "looks the same in normal usage" — bracketed-paste mode,
//! alt-screen state, mouse-tracking modes, hyperlinks, dotted/dashed
//! underline variants, and exact cursor position are NOT round-tripped.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};

/// Capture-blob header identifying a payload that begins with the grid
/// dimensions used to produce it. Prevents the "replay 200-col bytes into
/// an 80-col Term" scramble seen on workspace restore when the new pane
/// happens to be sized differently from the captured one. Versioned by
/// the trailing `\x01` so future format tweaks can be detected without
/// disambiguation against random binary content.
pub const CAPTURE_HEADER_MAGIC: &[u8; 5] = b"OXBF\x01";

/// Total header length: 4-byte tag + 1-byte version + cols(u16 LE) +
/// rows(u16 LE) = 9 bytes.
pub const CAPTURE_HEADER_LEN: usize = 9;

/// Build the 9-byte header for a serialized capture. Public so backends
/// outside the in-process portable-pty path (e.g. the relay) can emit
/// the same envelope when they capture state from a remote grid.
pub fn build_capture_header(cols: u16, rows: u16) -> [u8; CAPTURE_HEADER_LEN] {
    let mut buf = [0u8; CAPTURE_HEADER_LEN];
    buf[..5].copy_from_slice(CAPTURE_HEADER_MAGIC);
    buf[5..7].copy_from_slice(&cols.to_le_bytes());
    buf[7..9].copy_from_slice(&rows.to_le_bytes());
    buf
}

/// Detect + parse the capture header. Returns the saved `(cols, rows)`
/// and the remaining payload slice on success; `None` for legacy blobs
/// (caller treats them as plain ANSI without resizing).
pub fn parse_capture_header(bytes: &[u8]) -> Option<(u16, u16, &[u8])> {
    if bytes.len() < CAPTURE_HEADER_LEN || &bytes[..5] != CAPTURE_HEADER_MAGIC {
        return None;
    }
    let cols = u16::from_le_bytes([bytes[5], bytes[6]]);
    let rows = u16::from_le_bytes([bytes[7], bytes[8]]);
    Some((cols, rows, &bytes[CAPTURE_HEADER_LEN..]))
}

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

    // Drop trailing empty rows: find the last line with any visible
    // content and stop there. The lower portion of a full-screen grid is
    // usually blank; emitting it as a tall stack of `\r\n` would reflow
    // into blank lines and shove restored content around on resize.
    let Some(last_content) = (start_line..end_line)
        .rev()
        .find(|&li| row_has_content(term, li, cols))
    else {
        // Nothing on screen — just the SGR baseline.
        return out;
    };

    let mut first_row = true;
    for line_idx in start_line..=last_content {
        if !first_row {
            out.extend_from_slice(b"\r\n");
        }
        first_row = false;
        emit_row(&mut out, &mut current, term, line_idx, cols);
    }

    out
}

/// True when any cell in the row carries a printable glyph (not blank /
/// not the unwritten-cell sentinel). Used to find the last content row so
/// trailing blank rows are dropped from the capture.
fn row_has_content(term: &Term<VoidListener>, line_idx: i32, cols: usize) -> bool {
    let row = &term.grid()[Line(line_idx)];
    (0..cols).any(|c| {
        let ch = row[Column(c)].c;
        ch != ' ' && ch != '\0'
    })
}

/// Serialize a term with a dimension header prepended so the consumer
/// can resize a fresh Term to match BEFORE replaying the body. This is
/// the format used by the in-process persistence path; it's what fixes
/// the "scrambled prompt after workspace restore" symptom when the new
/// pane is sized differently from the captured one.
///
/// The `max_bytes` budget covers the total returned blob — the body is
/// capped to `max_bytes - CAPTURE_HEADER_LEN` so the header always fits.
pub fn serialize_term_capped_with_dims(term: &Term<VoidListener>, max_bytes: usize) -> Vec<u8> {
    let cols = term.columns() as u16;
    let rows = term.screen_lines() as u16;
    let body_budget = max_bytes.saturating_sub(CAPTURE_HEADER_LEN);
    let body = serialize_term_capped(term, body_budget);
    let header = build_capture_header(cols, rows);
    let mut out = Vec::with_capacity(CAPTURE_HEADER_LEN + body.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&body);
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
    // Trim trailing blanks: emit only through the last cell that carries a
    // printable glyph. Leading + interior spacing (e.g. the column gaps in
    // `ls` output) is preserved — only the run of blank padding at the
    // right edge is dropped, so the replayed line is its natural length
    // and reflows without spilling into wrapped blank rows.
    let end = match (0..cols).rev().find(|&c| {
        let ch = row[Column(c)].c;
        ch != ' ' && ch != '\0'
    }) {
        Some(last_sig) => last_sig + 1,
        None => 0,
    };
    for col_idx in 0..end {
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
    fn positioned_content_survives_roundtrip() {
        // Exact cursor position is intentionally NOT round-tripped — the
        // restored scrollback hands the cursor to the freshly-spawned
        // shell. But content WRITTEN at an absolute position must land on
        // the same row/col after replay. Move to row 3 col 5 (1-based)
        // and write 'Z'.
        let original = replay_into_new_term(b"\x1b[3;5HZ", 20, 10);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        let restored = replay_into_new_term(&bytes, 20, 10);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        // 'Z' was at row 2 col 4 (0-based); it must survive there.
        assert_eq!(snap.cells[2][4].ch, 'Z');
    }

    #[test]
    fn trailing_blank_rows_and_padding_are_dropped() {
        // The restore-scramble fix: content padded to full width + empty
        // lower rows must NOT be emitted, so the capture stays compact and
        // reflows cleanly. Two short lines in a wide, tall grid.
        let original = replay_into_new_term(b"hi\r\nbye\r\n", 80, 24);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        // No full-width padding: the body is just the two trimmed lines.
        // (SGR baseline + "hi" + CRLF + "bye" — well under 20 bytes.)
        assert!(
            bytes.len() < 20,
            "expected compact output, got {} bytes: {:?}",
            bytes.len(),
            String::from_utf8_lossy(&bytes),
        );
        // And it still round-trips the visible content.
        let restored = replay_into_new_term(&bytes, 80, 24);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        assert_eq!(snap.cells[0].iter().map(|c| c.ch).collect::<String>().trim_end(), "hi");
        assert_eq!(snap.cells[1].iter().map(|c| c.ch).collect::<String>().trim_end(), "bye");
    }

    #[test]
    fn interior_spacing_is_preserved() {
        // `ls`-style column gaps are interior whitespace, not trailing —
        // they must survive so multi-column output stays aligned.
        let original = replay_into_new_term(b"a    b    c", 80, 5);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        let restored = replay_into_new_term(&bytes, 80, 5);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        let row0: String = snap.cells[0].iter().map(|c| c.ch).collect();
        assert_eq!(row0.trim_end(), "a    b    c");
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
    fn capture_header_round_trips_dims() {
        let header = build_capture_header(120, 40);
        let (cols, rows, payload) = parse_capture_header(&header).expect("magic present");
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
        assert!(payload.is_empty());
    }

    #[test]
    fn capture_header_rejects_unmagic_blob() {
        // Legacy (pre-header) blobs start with `\x1b[0m` from
        // `serialize_term`; the parser must report None so the caller
        // falls back to direct advance.
        let legacy = b"\x1b[0mhello";
        assert!(parse_capture_header(legacy).is_none());
    }

    #[test]
    fn capture_header_rejects_short_input() {
        assert!(parse_capture_header(b"OX").is_none());
        assert!(parse_capture_header(b"OXBF").is_none());
    }

    #[test]
    fn serialize_with_dims_emits_parseable_header() {
        // Build a non-trivial term so the body is non-empty and we
        // verify the dims-prefix sits in front of the SGR baseline.
        let original = replay_into_new_term(b"abc", 80, 24);
        let bytes = serialize_term_capped_with_dims(original.term_for_test(), 1024);
        let (cols, rows, payload) = parse_capture_header(&bytes).expect("header present");
        assert_eq!((cols, rows), (80, 24));
        // Payload should start with the SGR-0 baseline that
        // `serialize_term` always emits first.
        assert!(payload.starts_with(b"\x1b[0m"));
    }

    #[test]
    fn dim_aware_capture_replays_into_matching_term_preserves_content() {
        // The fix's smoke test: capture a 120-wide term containing
        // content at column 100 (would land off-screen in an 80-col
        // replay) and verify the dim-aware path round-trips correctly
        // when the consumer resizes to match the header first.
        let mut original = replay_into_new_term(b"", 120, 10);
        // Park the cursor at row 1 col 100 (1-based CUP) and write "X".
        original.advance(b"\x1b[1;100HX");
        let bytes = serialize_term_capped_with_dims(original.term_for_test(), 4096);
        let (cols, rows, payload) = parse_capture_header(&bytes).expect("header");
        assert_eq!((cols, rows), (120, 10));
        // Consumer side: spawn a Term at the saved dims and replay.
        let restored = replay_into_new_term(payload, cols, rows);
        let mut snap = crate::snapshot::TerminalSnapshot::default();
        restored.fill_snapshot(&mut snap);
        // The 'X' must land at row 0 col 99 (0-based) — same as in the
        // original. Without the dim-aware capture, replaying into an
        // 80-col Term would clip the position and produce scramble.
        assert_eq!(snap.cells[0][99].ch, 'X');
    }

    #[test]
    fn empty_grid_serializes_without_panicking() {
        let original = TerminalState::new(80, 24, 100);
        let bytes = serialize_term(original.term_for_test(), SerializeOptions::default());
        // An all-blank grid emits just the SGR-0 baseline — no content
        // rows, no trailing cursor-move.
        assert_eq!(bytes, b"\x1b[0m");
    }
}
