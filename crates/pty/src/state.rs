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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb,
};

use crate::backend::TerminalSessionId;
use crate::events::TerminalEvent;
use crate::osc7::{OscHit, OscScanner};
use crate::snapshot::{Cell, CellColor, HyperlinkSpan, NamedColor16, TerminalSnapshot};

/// Cap on the event sink so a misbehaving stream (or a huge replay) can't grow
/// it without bound between drains. Realistic streams emit a handful per frame.
const MAX_SINK_EVENTS: usize = 512;

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

/// One queued non-bell event from alacritty's parser. alacritty calls
/// `send_event` synchronously from inside `parser.advance`, so the listener
/// can't reach the PTY writer or the backend channel — it parks events here
/// for `advance_collecting` to drain right after `advance`.
enum SinkEvent {
    /// OSC 0/2 window title.
    Title(String),
    /// OSC 52 clipboard-set (alacritty already base64-decoded the payload).
    Clipboard(String),
    /// OSC 4/10/11/12 color query — `reply` formats the resolved color into
    /// the escape sequence the child expects written back.
    ColorQuery {
        index: usize,
        reply: Arc<dyn Fn(Rgb) -> String + Send + Sync>,
    },
    /// Device report / cursor-position reply (DSR, DA, …) to write back.
    PtyWrite(Vec<u8>),
}

/// Captures the alacritty events we forward. `Bell` flips a cheap atomic
/// (read via `take_bell()`); the rest park in a bounded queue drained by
/// `advance_collecting()`.
#[derive(Clone)]
pub struct TermEventSink {
    bell: Arc<AtomicBool>,
    sink: Arc<Mutex<Vec<SinkEvent>>>,
}

impl TermEventSink {
    fn push(&self, ev: SinkEvent) {
        if let Ok(mut q) = self.sink.lock()
            && q.len() < MAX_SINK_EVENTS
        {
            q.push(ev);
        }
    }
}

impl EventListener for TermEventSink {
    fn send_event(&self, event: Event) {
        match event {
            Event::Bell => self.bell.store(true, Ordering::Release),
            Event::Title(title) => self.push(SinkEvent::Title(title)),
            // Only the system clipboard (`c`); selection (`p`/`s`) has no
            // distinct buffer on macOS, so writing it would clobber the copy.
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                self.push(SinkEvent::Clipboard(text))
            }
            Event::ColorRequest(index, reply) => self.push(SinkEvent::ColorQuery { index, reply }),
            Event::PtyWrite(text) => self.push(SinkEvent::PtyWrite(text.into_bytes())),
            _ => {}
        }
    }
}

pub struct TerminalState {
    term: Term<TermEventSink>,
    parser: Processor,
    size: SizeInfo,
    /// Out-of-band scanner for the OSCs alacritty drops (cwd, command marks,
    /// progress). Persists across reads — sequences span chunk boundaries.
    scanner: OscScanner,
    /// Shared with the `Term`'s sink; set on BEL, cleared by `take_bell()`.
    bell: Arc<AtomicBool>,
    /// Shared with the `Term`'s sink; drained by `advance_collecting()`.
    sink: Arc<Mutex<Vec<SinkEvent>>>,
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
        let bell = Arc::new(AtomicBool::new(false));
        let sink: Arc<Mutex<Vec<SinkEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let term = Term::new(
            config,
            &size,
            TermEventSink {
                bell: Arc::clone(&bell),
                sink: Arc::clone(&sink),
            },
        );
        Self {
            term,
            parser: Processor::new(),
            size,
            scanner: OscScanner::new(),
            bell,
            sink,
        }
    }

    /// Feed PTY bytes through the ANSI parser into the grid.
    ///
    /// Pure: used by replay/prefill and tests. It still fires the event sink
    /// (the listener is wired to the `Term`), so callers that DON'T want those
    /// events — replay — must follow with [`clear_collected`].
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Feed live PTY bytes and return everything that warrants a backend
    /// `TerminalEvent`: bell, OSC 7 cwd, OSC 133/633 command marks, OSC 9;4
    /// progress, OSC 0/2 title, OSC 52 clipboard, and device/color replies the
    /// child expects written back to the PTY. Backends call this on the read
    /// path; `advance` stays pure for replay.
    pub fn advance_collecting(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Vec<TerminalEvent> {
        let mut out = Vec::new();
        // Out-of-band OSC scan FIRST; the command-mark anchor is read from the
        // grid AFTER advance so it reflects where the cursor lands.
        let mut hits: Vec<OscHit> = Vec::new();
        self.scanner.feed(bytes, |h| hits.push(h));

        self.parser.advance(&mut self.term, bytes);

        if self.take_bell() {
            out.push(TerminalEvent::Bell { id });
        }

        if !hits.is_empty() {
            // Absolute history line: stable while scrollback hasn't capped
            // (new lines push content up, history_size grows in step), so a
            // gutter badge tracks its command as the view scrolls.
            let history = self.term.grid().history_size() as u64;
            let cursor_line = self.term.grid().cursor.point.line.0.max(0) as u64;
            let line = history + cursor_line;
            for hit in hits {
                match hit {
                    OscHit::Cwd(path) => out.push(TerminalEvent::CwdChanged { id, path }),
                    OscHit::CommandMark { kind, exit } => {
                        out.push(TerminalEvent::CommandMark {
                            id,
                            kind,
                            exit,
                            line,
                        })
                    }
                    OscHit::Progress { state, value } => {
                        out.push(TerminalEvent::Progress { id, state, value })
                    }
                }
            }
        }

        let drained: Vec<SinkEvent> = self
            .sink
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default();
        for ev in drained {
            match ev {
                SinkEvent::Title(title) => out.push(TerminalEvent::TitleChange { id, title }),
                SinkEvent::Clipboard(text) => out.push(TerminalEvent::Clipboard { id, text }),
                SinkEvent::PtyWrite(bytes) => out.push(TerminalEvent::PtyReply { id, bytes }),
                SinkEvent::ColorQuery { index, reply } => {
                    // Honor any color the app SET (OSC 4/10/11); else answer
                    // from a dark default palette so probes detect dark mode.
                    let rgb = self.term.colors()[index].unwrap_or_else(|| default_palette_rgb(index));
                    out.push(TerminalEvent::PtyReply {
                        id,
                        bytes: reply(rgb).into_bytes(),
                    });
                }
            }
        }
        out
    }

    /// Discard events accumulated by `advance` during a replay — a restored
    /// buffer must not re-fire the original session's clipboard writes, title
    /// changes, bell, or color replies into the live UI.
    pub fn clear_collected(&mut self) {
        self.bell.store(false, Ordering::Release);
        if let Ok(mut q) = self.sink.lock() {
            q.clear();
        }
        self.scanner = OscScanner::new();
    }

    /// Consume the pending-bell flag: returns `true` (and resets to `false`)
    /// if the child rang the bell since the last call. Called by the backend
    /// right after `advance` so a BEL in the chunk becomes a `TerminalEvent::Bell`.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, Ordering::AcqRel)
    }

    /// Input-relevant terminal modes for the keystroke encoder. Reads
    /// `TermMode::APP_CURSOR` (DECCKM) live so cursor keys switch to SS3 form
    /// while a full-screen app owns the screen.
    pub fn input_mode(&self) -> crate::InputMode {
        crate::InputMode {
            app_cursor: self.term.mode().contains(TermMode::APP_CURSOR),
        }
    }

    /// Cursor shape the app requested (DECSCUSR), mapped off alacritty's
    /// renderable cursor. `Hidden` covers both an explicit hidden shape and
    /// DECTCEM-off so the renderer has a single "draw nothing" signal.
    fn cursor_shape(&self) -> crate::CursorShapeKind {
        use crate::CursorShapeKind;
        match self.term.renderable_content().cursor.shape {
            CursorShape::Block => CursorShapeKind::Block,
            CursorShape::Beam => CursorShapeKind::Bar,
            CursorShape::Underline => CursorShapeKind::Underline,
            CursorShape::Hidden => CursorShapeKind::Hidden,
            // HollowBlock (and any future variant) → solid block.
            _ => CursorShapeKind::Block,
        }
    }

    /// Mouse-reporting modes the app has enabled, read live so the renderer
    /// can decide per-event whether to forward the mouse to the child.
    pub fn mouse_mode(&self) -> crate::MouseMode {
        let m = self.term.mode();
        crate::MouseMode {
            report_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            report_drag: m.contains(TermMode::MOUSE_DRAG),
            report_motion: m.contains(TermMode::MOUSE_MOTION),
            sgr: m.contains(TermMode::SGR_MOUSE),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            alt_screen: m.contains(TermMode::ALT_SCREEN),
        }
    }

    /// True when the shell has requested DECSET 2004 (bracketed paste).
    /// The renderer wraps pasted text with `\e[200~` / `\e[201~` only when
    /// this is set — modern shells (zsh, bash with readline ≥ 8) opt in;
    /// raw `cat` does not, and double-wrapping there would leak escape
    /// sequences as literal text.
    pub fn is_bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Crate-internal accessor used by `grid_serializer` to walk the live
    /// grid for persistence. Exposed (rather than wrapping every
    /// `serialize_*` call as a method) so the serializer module stays a
    /// pure function over `Term` and is unit-testable on its own.
    pub fn term_for_test(&self) -> &Term<TermEventSink> {
        &self.term
    }

    /// Resize the grid. Called whenever the pane's render area changes.
    ///
    /// Delegates to alacritty's `Term::resize` so the active grid, inactive
    /// (alt-screen) grid, damage tracker, vi cursor, tabs, and selection all
    /// stay coherent in one shot. An earlier revision bypassed this and
    /// called `grid_mut().resize(false, ..)` directly to suppress reflow on
    /// shrink, but that left the private `damage` tracker at the old line
    /// count — the next CSI cursor move into a new row indexed past
    /// `damage.lines.len()` and panicked at
    /// `term/mod.rs:258` (alacritty_terminal 0.25.1).
    ///
    /// The reflow worry from that revision (3-column `ls` output appearing
    /// scrambled after a shrink) traced to wrong cell metrics, not reflow
    /// itself — once `cell_metrics` measured the live 'm' advance, columns
    /// stopped lying to the shell and `ls` reflows cleanly. The visual leaf
    /// also runs `overflow_hidden`, so any residual right-edge spill is
    /// clipped at the pane boundary regardless of what the grid contains.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.size.cols = cols as usize;
        self.size.rows = rows as usize;
        // FIXME(alacritty-bump): Term::resize atomically resizes active grid,
        // inactive (alt-screen) grid, damage tracker, vi cursor, tabs, and
        // scroll_region. If a future alacritty release splits any of those
        // out of this entry point, the damage tracker can drift out of sync
        // again and panic on the next CSI cursor move (see commit history
        // for the v0.9 bypass that this code reverts).
        self.term.resize(self.size);
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

    /// Scroll the displayed viewport by `delta` lines (positive = back into
    /// history, negative = toward the live tail). Render-side only: the grid
    /// and PTY are untouched, so this never resizes or writes to the child.
    /// alacritty clamps the offset to the available history, so an
    /// over-scroll past the top/bottom is a no-op.
    pub fn scroll_lines(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Jump the viewport back to the live tail (display offset 0). Called on
    /// keyboard input so typing always snaps to the prompt.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Wipe the visible grid AND the scrollback history, then snap the
    /// viewport to the tail — the standard terminal "Clear" affordance. This
    /// feeds the same escape sequence the `clear(1)` utility emits (home,
    /// erase-display, erase-saved-lines) through the parser, so alacritty's
    /// grid + history + cursor all stay coherent: `ESC[3J` drops the saved
    /// scrollback, `ESC[2J` erases the screen, `ESC[H` homes the cursor.
    /// Goes through `advance` (not `advance_collecting`), so it raises no
    /// title/clipboard/bell side effects. The shell's own prompt redraws on
    /// the next keystroke / Enter, leaving a usable terminal immediately.
    pub fn clear(&mut self) {
        self.advance(b"\x1b[H\x1b[2J\x1b[3J");
        self.scroll_to_bottom();
    }

    /// Populate `snap` with the currently *displayed* grid + cursor, honoring
    /// the scrollback offset. Allocates one `Vec<Cell>` per row; cheap at
    /// 24-200 rows.
    ///
    /// When the user has scrolled up (`display_offset > 0`), screen row `r`
    /// shows grid line `r - offset` — negative lines index into history, the
    /// same range `fill_search_grid` walks. With `offset == 0` this is byte-
    /// identical to the pre-scroll behavior (every screen row maps to its own
    /// active-area line), so existing snapshot expectations are preserved.
    pub fn fill_snapshot(&self, snap: &mut TerminalSnapshot) {
        snap.cols = self.size.cols as u16;
        snap.rows = self.size.rows as u16;

        let offset = self.term.grid().display_offset() as i32;
        snap.display_offset = offset as usize;
        snap.history_len = self.term.grid().history_size();
        snap.cursor_shape = self.cursor_shape();

        // The cursor lives in active-area coords (line ≥ 0); in the displayed
        // viewport it shifts DOWN by the scroll offset. When it falls outside
        // the visible rows (scrolled into history), emit the off-grid sentinel
        // so the renderer suppresses it instead of clamping it to an edge.
        let cursor_point = self.term.grid().cursor.point;
        let cursor_row = cursor_point.line.0 + offset;
        snap.cursor = if cursor_row >= 0 && (cursor_row as usize) < self.size.rows {
            (cursor_row as u16, cursor_point.column.0 as u16)
        } else {
            (u16::MAX, u16::MAX)
        };

        snap.cells.clear();
        snap.cells.reserve(self.size.rows);
        snap.links.clear();
        for screen_row in 0..self.size.rows as i32 {
            let row = &self.term.grid()[Line(screen_row - offset)];
            let visible_row = screen_row as usize;
            let mut row_cells = Vec::with_capacity(self.size.cols);
            // OSC 8 run accumulator: (start_col, uri). Extends while the uri
            // matches; closes on change/None/row-end. Empty on the common
            // path (no app-emitted hyperlinks).
            let mut link_run: Option<(usize, String)> = None;
            for col_idx in 0..self.size.cols {
                let cell = &row[Column(col_idx)];
                match cell.hyperlink() {
                    Some(h) => {
                        let uri = h.uri();
                        let same = matches!(&link_run, Some((_, u)) if u == uri);
                        if !same {
                            if let Some((start, u)) = link_run.take() {
                                snap.links.push(HyperlinkSpan {
                                    row: visible_row,
                                    col_start: start,
                                    col_end: col_idx - 1,
                                    uri: u,
                                });
                            }
                            link_run = Some((col_idx, uri.to_string()));
                        }
                    }
                    None => {
                        if let Some((start, u)) = link_run.take() {
                            snap.links.push(HyperlinkSpan {
                                row: visible_row,
                                col_start: start,
                                col_end: col_idx - 1,
                                uri: u,
                            });
                        }
                    }
                }
                row_cells.push(map_cell(cell));
            }
            if let Some((start, u)) = link_run.take() {
                snap.links.push(HyperlinkSpan {
                    row: visible_row,
                    col_start: start,
                    col_end: self.size.cols - 1,
                    uri: u,
                });
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
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        // `ALL_UNDERLINES` covers single/double/curly/dotted/dashed — the
        // renderer collapses them to one underline style.
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
        strikethrough: cell.flags.contains(Flags::STRIKEOUT),
        hidden: cell.flags.contains(Flags::HIDDEN),
        wide: cell.flags.contains(Flags::WIDE_CHAR),
        // Both trailing (after a wide) and leading (when a wide would have
        // wrapped past the right edge) spacers collapse to the same render
        // signal: "skip me, the adjacent wide glyph owns this column."
        wide_spacer: cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
    }
}

/// Default color for an OSC color query when the app hasn't SET that slot.
/// Background is intentionally dark so tools (fzf, codex, vim) querying OSC 11
/// detect a dark theme; the 0..255 palette uses the standard xterm map.
fn default_palette_rgb(index: usize) -> Rgb {
    const FG: Rgb = Rgb {
        r: 0xcd,
        g: 0xd6,
        b: 0xe4,
    };
    const BG: Rgb = Rgb {
        r: 0x12,
        g: 0x14,
        b: 0x18,
    };
    match index {
        i if i == NamedColor::Foreground as usize => FG,
        i if i == NamedColor::Background as usize => BG,
        i if i == NamedColor::Cursor as usize => FG,
        0..=15 => ansi16_rgb(index as u8),
        16..=231 => {
            // 6×6×6 color cube. Each axis level 0 → 0, else 55 + 40·level.
            let i = index as u8 - 16;
            let level = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            Rgb {
                r: level(i / 36),
                g: level((i / 6) % 6),
                b: level(i % 6),
            }
        }
        232..=255 => {
            // 24-step grayscale ramp.
            let v = 8 + (index as u8 - 232) * 10;
            Rgb { r: v, g: v, b: v }
        }
        _ => FG,
    }
}

/// Standard xterm RGB for the 16 base ANSI colors (0..=15).
fn ansi16_rgb(idx: u8) -> Rgb {
    let (r, g, b) = match idx {
        0 => (0x00, 0x00, 0x00),
        1 => (0xcd, 0x00, 0x00),
        2 => (0x00, 0xcd, 0x00),
        3 => (0xcd, 0xcd, 0x00),
        4 => (0x00, 0x00, 0xee),
        5 => (0xcd, 0x00, 0xcd),
        6 => (0x00, 0xcd, 0xcd),
        7 => (0xe5, 0xe5, 0xe5),
        8 => (0x7f, 0x7f, 0x7f),
        9 => (0xff, 0x00, 0x00),
        10 => (0x00, 0xff, 0x00),
        11 => (0xff, 0xff, 0x00),
        12 => (0x5c, 0x5c, 0xff),
        13 => (0xff, 0x00, 0xff),
        14 => (0x00, 0xff, 0xff),
        _ => (0xff, 0xff, 0xff),
    };
    Rgb { r, g, b }
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
    fn bell_flag_set_on_bel_and_cleared_by_take() {
        let mut state = TerminalState::new(80, 24, 100);
        assert!(!state.take_bell(), "fresh state has no pending bell");
        // BEL (0x07) anywhere in the stream rings the bell.
        state.advance(b"a\x07b");
        assert!(state.take_bell(), "BEL should set the pending-bell flag");
        assert!(
            !state.take_bell(),
            "take_bell must clear the flag (one bell, one signal)"
        );
        // Plain output does not ring.
        state.advance(b"hello");
        assert!(!state.take_bell(), "no BEL → no bell");
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
    fn write_into_grown_row_does_not_panic() {
        // Regression: an earlier resize() bypassed Term::resize and called
        // grid_mut().resize() directly, which left the private damage tracker
        // at the old line count. The next CSI cursor-move into a new row
        // indexed damage.lines[N] where N >= old_rows and panicked at
        // alacritty_terminal/src/term/mod.rs:258. Resizing via Term::resize
        // keeps damage in sync; this test would hit the panic with the
        // bypassed code path.
        let mut state = TerminalState::new(80, 24, 100);
        state.resize(80, 40);
        // CUP row=35 col=1 (1-based) → line 34, which is valid in the grown
        // 40-row grid but past the old 24-row damage vec.
        state.advance(b"\x1b[35;1HX");
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.cursor, (34, 1));
    }

    #[test]
    fn shrink_then_write_at_bottom_does_not_panic() {
        // Companion to the grow case: shrink the grid then write to a row
        // that's in-bounds for the SHRUNK grid. Verifies Term::resize keeps
        // damage tightened on shrink too, and that the cursor lands inside
        // the new viewport without the manual clamp the bypass version had.
        let mut state = TerminalState::new(80, 40, 100);
        state.resize(80, 24);
        state.advance(b"\x1b[24;80HX");
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.cursor, (23, 79));
    }

    #[test]
    fn grow_shrink_grow_cycle_does_not_panic() {
        // Pins the grow-shrink-grow scenario that matches the original
        // "len 32, index 32" crash signature: damage must keep up with the
        // grid through every transition, not just monotonic growth.
        let mut state = TerminalState::new(80, 24, 100);
        state.resize(80, 40);
        state.advance(b"primary fill\r\n");
        state.resize(80, 20);
        state.resize(80, 35);
        state.advance(b"\x1b[30;1HY");
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.cursor, (29, 1));
    }

    #[test]
    fn clear_wipes_grid_and_scrollback() {
        // 3 visible rows; print 6 lines so several scroll into history.
        let mut state = TerminalState::new(20, 3, 100);
        state.advance(b"L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6");
        let before = fresh_snapshot(&state);
        assert!(before.history_len > 0, "precondition: lines scrolled to history");
        assert!(
            before.cells.iter().any(|r| r.iter().any(|c| c.ch != ' ')),
            "precondition: visible grid has content"
        );

        state.clear();

        let after = fresh_snapshot(&state);
        // Visible grid is blank.
        for row in &after.cells {
            for cell in row {
                assert_eq!(cell.ch, ' ', "every visible cell blank after clear");
            }
        }
        // Scrollback history wiped + viewport snapped to the tail.
        assert_eq!(after.history_len, 0, "scrollback cleared");
        assert_eq!(after.display_offset, 0, "viewport at the live tail");
        // The terminal is usable immediately: fresh output lands at the top.
        state.advance(b"hi");
        assert!(
            row_text(&fresh_snapshot(&state), 0).starts_with("hi"),
            "fresh output usable right after clear"
        );
    }

    #[test]
    fn osc8_hyperlink_span_captured() {
        // OSC 8: ESC ] 8 ; params ; URI ST  text  ESC ] 8 ; ; ST
        let mut state = TerminalState::new(40, 3, 100);
        state.advance(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");
        let snap = fresh_snapshot(&state);
        assert_eq!(snap.links.len(), 1, "one hyperlink span expected");
        let span = &snap.links[0];
        assert_eq!(span.row, 0);
        assert_eq!(span.col_start, 0);
        assert_eq!(span.col_end, 3, "\"link\" spans cols 0..=3");
        assert_eq!(span.uri, "https://example.com");
    }

    #[test]
    fn cursor_shape_tracks_decscusr_and_dectcem() {
        use crate::CursorShapeKind;
        let mut state = TerminalState::new(20, 3, 100);
        assert_eq!(fresh_snapshot(&state).cursor_shape, CursorShapeKind::Block);
        // DECSCUSR 5 = blinking bar (alacritty Beam).
        state.advance(b"\x1b[5 q");
        assert_eq!(fresh_snapshot(&state).cursor_shape, CursorShapeKind::Bar);
        // DECSCUSR 3 = blinking underline.
        state.advance(b"\x1b[3 q");
        assert_eq!(
            fresh_snapshot(&state).cursor_shape,
            CursorShapeKind::Underline
        );
        // DECTCEM hide → Hidden regardless of shape.
        state.advance(b"\x1b[?25l");
        assert_eq!(fresh_snapshot(&state).cursor_shape, CursorShapeKind::Hidden);
    }

    #[test]
    fn scroll_back_reports_offset_and_shifts_view() {
        // 3 visible rows; print 6 lines so several scroll into history.
        let mut state = TerminalState::new(20, 3, 100);
        state.advance(b"L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6");
        let bottom = fresh_snapshot(&state);
        assert_eq!(bottom.display_offset, 0, "fresh view is pinned to the tail");
        let bottom_top = row_text(&bottom, 0);

        // Scroll up two lines into history.
        state.scroll_lines(2);
        let scrolled = fresh_snapshot(&state);
        assert_eq!(scrolled.display_offset, 2);
        assert_ne!(
            row_text(&scrolled, 0),
            bottom_top,
            "scrolling up should reveal an earlier line at the top"
        );

        // Snapping to bottom resumes the live tail.
        state.scroll_to_bottom();
        assert_eq!(fresh_snapshot(&state).display_offset, 0);
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
    fn sgr_attributes_flow_through_snapshot() {
        // 1=bold 3=italic 4=underline 9=strikethrough 8=hidden. SGR sequences
        // don't advance the cursor, so each glyph lands on the next column:
        // col0=B col1=I col2=U col3=S col4=H. SGR 0 between resets so a flag
        // never bleeds into the following cell.
        let mut state = TerminalState::new(80, 24, 100);
        state.advance(b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[9mS\x1b[0m\x1b[8mH");
        let snap = fresh_snapshot(&state);
        assert!(snap.cells[0][0].bold, "B (SGR 1) should be bold");
        assert!(snap.cells[0][1].italic, "I (SGR 3) should be italic");
        assert!(snap.cells[0][2].underline, "U (SGR 4) should be underline");
        assert!(
            snap.cells[0][3].strikethrough,
            "S (SGR 9) should be strikethrough"
        );
        assert!(snap.cells[0][4].hidden, "H (SGR 8) should be hidden");
        // Resets actually cleared: the bold 'B' cell carries no other attr.
        assert!(!snap.cells[0][0].italic && !snap.cells[0][0].underline);
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

    const SID: TerminalSessionId = TerminalSessionId(1);

    #[test]
    fn advance_collecting_surfaces_osc52_clipboard() {
        let mut state = TerminalState::new(80, 24, 100);
        // OSC 52 ; c ; base64("hi") — alacritty decodes the base64 for us.
        let events = state.advance_collecting(SID, b"\x1b]52;c;aGk=\x07");
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::Clipboard { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi"], "OSC 52 c → clipboard set");
    }

    #[test]
    fn advance_collecting_replies_to_bg_color_query() {
        let mut state = TerminalState::new(80, 24, 100);
        // OSC 11 ; ? — query the default background; expect an rgb: reply.
        let events = state.advance_collecting(SID, b"\x1b]11;?\x07");
        let reply = events.iter().find_map(|e| match e {
            TerminalEvent::PtyReply { bytes, .. } => std::str::from_utf8(bytes).ok(),
            _ => None,
        });
        let reply = reply.expect("a PtyReply for the color query");
        // Format is `ESC ] 11 ; rgb:RRRR/GGGG/BBBB <terminator>`; the default
        // background is dark (each channel byte < 0x40) so probes read dark.
        assert!(reply.starts_with("\x1b]11;rgb:"), "got: {reply:?}");
        assert!(
            reply.contains("rgb:1212/1414/1818"),
            "dark default bg, got: {reply:?}"
        );
    }

    #[test]
    fn advance_collecting_anchors_command_mark_at_cursor_line() {
        let mut state = TerminalState::new(80, 24, 100);
        // Push the cursor down two rows, then emit a prompt-start mark. With
        // no scrollback yet the anchor is just the cursor's screen line.
        state.advance(b"a\r\nb\r\n");
        let events = state.advance_collecting(SID, b"\x1b]133;A\x07");
        let mark = events.iter().find_map(|e| match e {
            TerminalEvent::CommandMark { kind, line, .. } => Some((*kind, *line)),
            _ => None,
        });
        assert_eq!(mark, Some((crate::CommandMarkKind::PromptStart, 2)));
    }

    #[test]
    fn advance_collecting_surfaces_progress() {
        let mut state = TerminalState::new(80, 24, 100);
        let events = state.advance_collecting(SID, b"\x1b]9;4;1;75\x07");
        let p = events.iter().find_map(|e| match e {
            TerminalEvent::Progress { state, value, .. } => Some((*state, *value)),
            _ => None,
        });
        assert_eq!(p, Some((1, 75)));
    }

    #[test]
    fn wide_char_marks_primary_and_spacer_cells() {
        // `你` (U+4F60) is a double-width CJK glyph. alacritty stores the
        // char in the first cell with the WIDE_CHAR flag and a placeholder in
        // the next cell with WIDE_CHAR_SPACER. The map_cell mapper must
        // surface both flags so the renderer can pair them.
        let mut state = TerminalState::new(80, 24, 100);
        state.advance("你x".as_bytes());
        let snap = fresh_snapshot(&state);
        let row0 = &snap.cells[0];
        assert_eq!(row0[0].ch, '你', "wide glyph in column 0");
        assert!(row0[0].wide, "WIDE_CHAR flag must surface as wide=true");
        assert!(!row0[0].wide_spacer);
        assert!(
            row0[1].wide_spacer,
            "column 1 is the WIDE_CHAR_SPACER for the adjacent wide glyph"
        );
        assert!(!row0[1].wide, "spacer is not itself wide");
        assert_eq!(row0[2].ch, 'x', "narrow ascii after the wide glyph");
        assert!(!row0[2].wide);
        assert!(!row0[2].wide_spacer);
    }

    #[test]
    fn clear_collected_drops_replayed_clipboard() {
        let mut state = TerminalState::new(80, 24, 100);
        // Simulate a replay that contained an OSC 52: `advance` fires the sink
        // but `clear_collected` must discard it so it never reaches the UI.
        state.advance(b"\x1b]52;c;c2VjcmV0\x07");
        state.clear_collected();
        let events = state.advance_collecting(SID, b"x");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, TerminalEvent::Clipboard { .. })),
            "replayed clipboard must not leak after clear_collected"
        );
    }
}
