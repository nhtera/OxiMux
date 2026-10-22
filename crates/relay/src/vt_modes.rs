//! Terminal modes for an exact reattach.
//!
//! The daemon keeps a PTY's output in a 1 MiB ring and replays it when a client
//! attaches. A ring cut at an arbitrary byte loses every mode set before the
//! cut: a TUI in the alternate screen, bracketed paste, mouse reporting, a
//! scroll region. The relay does not emulate a terminal (each client does), so
//! this module tracks just those modes from the live stream, cheaply, and turns
//! them into a short *postamble* appended to the replay.
//!
//! - [`ModeTracker`] — fed every live byte; knows each tracked mode's current
//!   value and where the latest alternate-screen entry sits in the stream.
//! - [`align_start`] — where a replay may start: after a newline, outside any
//!   escape sequence, on a UTF-8 boundary.
//! - [`postamble`] — the bytes that bring a client that replayed `body` from
//!   power-on to the live modes. Only the difference is emitted: re-sending a
//!   mode the body already set would be harmful (`?1049h` clears the screen).
//!
//! Pure: no I/O, no locking. The registry keeps a tracker beside the ring,
//! under the same lock, so a snapshot and its modes always agree.

/// DEC private modes this tracker follows, with their power-on defaults.
const TRACKED_MODES: &[(u16, bool)] = &[
    (1, false),    // DECCKM application cursor keys
    (6, false),    // DECOM origin mode
    (7, true),     // DECAWM autowrap
    (25, true),    // DECTCEM cursor visible
    (1004, false), // focus in/out reports
    (2004, false), // bracketed paste
    (2026, false), // synchronized output
];

/// The alternate-screen mode. `?47` / `?1047` are older spellings the client
/// emulator ignores, so tracking them would blank a screen it never left.
const ALT_SCREEN_MODES: [u16; 1] = [1049];
/// Mouse tracking modes: at most one is in force, the last one set.
const MOUSE_TRACKING: [u16; 3] = [1000, 1002, 1003];
/// Mouse coordinate encodings: likewise one at a time.
const MOUSE_ENCODING: [u16; 3] = [1005, 1006, 1015];

/// Longest CSI parameter string kept; longer ones are some other sequence.
const MAX_PARAMS: usize = 64;
/// Progressive keyboard-enhancement (`CSI > flags u`) stack depth kept (the
/// protocol allows more; only the top matters for a reattach).
const MAX_KEYBOARD_STACK: usize = 16;
/// How far into a replay [`align_start`] looks for a clean line start.
pub const ALIGN_SEARCH_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Scan {
    #[default]
    Ground,
    Esc,
    /// An escape with intermediate bytes (`ESC ( B`, a charset designation),
    /// running to its final byte. Nothing in it is tracked.
    EscInter,
    /// CSI: parameters accumulate in `ModeTracker::params`.
    Csi,
    /// A string (OSC, DCS, APC, PM, SOS) runs to BEL or ST; nothing in it is a
    /// mode change.
    Str,
    StrEsc,
}

/// The tracked modes' values. `Eq` so a postamble can diff two of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modes {
    /// Which alternate-screen mode is set, if any.
    pub alt_screen: Option<u16>,
    /// One bool per [`TRACKED_MODES`] entry.
    dec: [bool; TRACKED_MODES.len()],
    /// The mouse tracking mode in force (one of [`MOUSE_TRACKING`]).
    pub mouse_tracking: Option<u16>,
    /// The mouse coordinate encoding in force (one of [`MOUSE_ENCODING`]).
    pub mouse_encoding: Option<u16>,
    /// DECSTBM `(top, bottom)`, 1-based, as last set; `None` is the full screen.
    pub scroll_region: Option<(u16, u16)>,
    /// DECKPAM (application keypad) vs DECKPNM.
    pub keypad_application: bool,
    /// Progressive keyboard-enhancement flags stack.
    keyboard: Vec<u32>,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            alt_screen: None,
            dec: std::array::from_fn(|i| TRACKED_MODES[i].1),
            mouse_tracking: None,
            mouse_encoding: None,
            scroll_region: None,
            keypad_application: false,
            keyboard: Vec::new(),
        }
    }
}

impl Modes {
    /// The value of tracked DEC private mode `mode` (`None` if untracked).
    pub fn dec(&self, mode: u16) -> Option<bool> {
        TRACKED_MODES.iter().position(|(m, _)| *m == mode).map(|i| self.dec[i])
    }

    /// The keyboard-enhancement flags in force (0 = protocol off).
    pub fn keyboard_flags(&self) -> u32 {
        self.keyboard.last().copied().unwrap_or(0)
    }

    fn set_dec(&mut self, mode: u16, on: bool) {
        let group = |slot: &mut Option<u16>| match (on, *slot) {
            (true, _) => *slot = Some(mode),
            (false, Some(m)) if m == mode => *slot = None,
            (false, _) => {}
        };
        if ALT_SCREEN_MODES.contains(&mode) {
            self.alt_screen = on.then_some(mode);
        } else if MOUSE_TRACKING.contains(&mode) {
            group(&mut self.mouse_tracking);
        } else if MOUSE_ENCODING.contains(&mode) {
            group(&mut self.mouse_encoding);
        } else if let Some(i) = TRACKED_MODES.iter().position(|(m, _)| *m == mode) {
            self.dec[i] = on;
        }
    }

    /// DECSTR soft reset: the subset of modes it returns to their defaults.
    fn soft_reset(&mut self) {
        for mode in [1u16, 6, 7, 25] {
            let default = TRACKED_MODES.iter().find(|(m, _)| *m == mode).map(|(_, d)| *d).unwrap_or(false);
            self.set_dec(mode, default);
        }
        self.scroll_region = None;
        self.keypad_application = false;
    }
}

/// Streaming tracker of the [`Modes`] in force. Feed it every byte, in order;
/// sequences split across chunks are handled.
#[derive(Debug, Clone, Default)]
pub struct ModeTracker {
    modes: Modes,
    scan: Scan,
    params: Vec<u8>,
    params_overflowed: bool,
    /// Stream offset just past the byte that last entered the alternate
    /// screen — where a replay must start to contain that entry.
    alt_entry_end: Option<u64>,
    /// Stream offset of the next byte fed.
    offset: u64,
}

impl ModeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn modes(&self) -> &Modes {
        &self.modes
    }

    /// Where the bytes behind the current alternate screen begin, when it is
    /// active: a replay that starts later has lost its entry.
    pub fn alt_entry_end(&self) -> Option<u64> {
        self.modes.alt_screen.and(self.alt_entry_end)
    }

    /// Stream offset of the next byte to be fed (= bytes fed so far, plus the
    /// start offset given to [`starting_at`](Self::starting_at)).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// A tracker whose first byte sits at stream offset `offset`.
    pub fn starting_at(offset: u64) -> Self {
        Self { offset, ..Self::default() }
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            // Plain text between escapes is most of any stream and changes
            // nothing: skip to the next ESC in one scan.
            if self.scan == Scan::Ground {
                match bytes.iter().position(|&b| b == 0x1b) {
                    Some(i) => {
                        self.offset += i as u64;
                        bytes = &bytes[i..];
                    }
                    None => {
                        self.offset += bytes.len() as u64;
                        return;
                    }
                }
            }
            self.offset += 1;
            self.step(bytes[0]);
            bytes = &bytes[1..];
        }
    }

    fn step(&mut self, b: u8) {
        const ESC: u8 = 0x1b;
        self.scan = match (self.scan, b) {
            // CAN / SUB abort any sequence.
            (_, 0x18 | 0x1a) => Scan::Ground,
            (Scan::Ground, ESC) => Scan::Esc,
            (Scan::Ground, _) => Scan::Ground,
            (Scan::Esc, b'[') => {
                self.params.clear();
                self.params_overflowed = false;
                Scan::Csi
            }
            (Scan::Esc, b']' | b'P' | b'_' | b'^' | b'X') => Scan::Str,
            (Scan::Esc, b'c') => {
                // RIS: a full reset.
                self.modes = Modes::default();
                self.alt_entry_end = None;
                Scan::Ground
            }
            (Scan::Esc, b'=') => {
                self.modes.keypad_application = true;
                Scan::Ground
            }
            (Scan::Esc, b'>') => {
                self.modes.keypad_application = false;
                Scan::Ground
            }
            (Scan::Esc, ESC) => Scan::Esc,
            // `ESC ( B` and the like: an intermediate, then a final byte that
            // means nothing on its own (`ESC ( =` is a charset, not DECKPAM).
            (Scan::Esc, 0x20..=0x2f) => Scan::EscInter,
            // `ESC 7`, `ESC 8`, …: nothing tracked.
            (Scan::Esc, _) => Scan::Ground,
            (Scan::EscInter, ESC) => Scan::Esc,
            (Scan::EscInter, 0x20..=0x2f) => Scan::EscInter,
            (Scan::EscInter, _) => Scan::Ground,
            (Scan::Csi, 0x20..=0x3f) => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(b);
                } else {
                    self.params_overflowed = true;
                }
                Scan::Csi
            }
            (Scan::Csi, 0x40..=0x7e) => {
                if !self.params_overflowed {
                    self.dispatch_csi(b);
                }
                Scan::Ground
            }
            (Scan::Csi, ESC) => Scan::Esc,
            // C0 controls inside a CSI are executed, not part of it.
            (Scan::Csi, 0x00..=0x1f) => Scan::Csi,
            (Scan::Csi, _) => Scan::Ground,
            (Scan::Str, 0x07) => Scan::Ground,
            (Scan::Str, ESC) => Scan::StrEsc,
            (Scan::Str, _) => Scan::Str,
            (Scan::StrEsc, b'\\') => Scan::Ground,
            // An ESC ends the string whatever follows, and starts an escape of
            // its own — as the client's parser has it.
            (Scan::StrEsc, _) => {
                self.scan = Scan::Esc;
                return self.step(b);
            }
        };
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        let params = std::mem::take(&mut self.params);
        let (marker, rest) = match params.first() {
            Some(&m @ (b'?' | b'>' | b'<' | b'=')) => (Some(m), &params[1..]),
            _ => (None, &params[..]),
        };
        let nums = || rest.split(|&c| c == b';').map(parse_num);
        match (marker, final_byte) {
            (Some(b'?'), b'h' | b'l') => {
                let on = final_byte == b'h';
                // A number past u16 is no mode (and must not wrap into one).
                for mode in nums().flatten().filter_map(|m| u16::try_from(m).ok()) {
                    let entering = on && ALT_SCREEN_MODES.contains(&mode) && self.modes.alt_screen.is_none();
                    self.modes.set_dec(mode, on);
                    if entering {
                        self.alt_entry_end = Some(self.offset);
                    }
                }
            }
            // DECSTBM. `CSI r` (or out-of-order bounds) is the full screen.
            (None, b'r') => {
                let mut it = nums();
                let top = it.next().flatten().unwrap_or(1).max(1);
                let bottom = it.next().flatten();
                let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
                self.modes.scroll_region = match bottom {
                    Some(bottom) if bottom > top => Some((clamp(top), clamp(bottom))),
                    _ => None,
                };
            }
            // DECSTR (`CSI ! p`): `!` is an intermediate, so it is in `rest`.
            (None, b'p') if rest == b"!" => self.modes.soft_reset(),
            // Keyboard enhancement: push, pop, set.
            (Some(b'>'), b'u') => {
                if self.modes.keyboard.len() >= MAX_KEYBOARD_STACK {
                    self.modes.keyboard.remove(0);
                }
                self.modes.keyboard.push(nums().next().flatten().unwrap_or(0));
            }
            (Some(b'<'), b'u') => {
                let n = nums().next().flatten().unwrap_or(1).max(1) as usize;
                let keep = self.modes.keyboard.len().saturating_sub(n);
                self.modes.keyboard.truncate(keep);
            }
            (Some(b'='), b'u') => {
                let mut it = nums();
                let flags = it.next().flatten().unwrap_or(0);
                let how = it.next().flatten().unwrap_or(1);
                let top = self.modes.keyboard.last().copied().unwrap_or(0);
                let next = match how {
                    2 => top | flags,
                    3 => top & !flags,
                    _ => flags,
                };
                match self.modes.keyboard.last_mut() {
                    Some(t) => *t = next,
                    None => self.modes.keyboard.push(next),
                }
            }
            _ => {}
        }
    }
}

fn parse_num(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Where a replay of `snap` should start: the first byte after a `\n`, within
/// [`ALIGN_SEARCH_BYTES`], that is outside any escape sequence (scanning from
/// the cut as if it were clean). Without one, the first UTF-8 boundary, so at
/// worst a partial line shows — never half a character.
pub fn align_start(snap: &[u8]) -> usize {
    let mut probe = ModeTracker::new();
    for (i, &b) in snap.iter().enumerate().take(ALIGN_SEARCH_BYTES) {
        probe.step(b);
        if b == b'\n' && probe.scan == Scan::Ground {
            return i + 1;
        }
    }
    snap.iter().position(|&b| (b & 0xC0) != 0x80).unwrap_or(snap.len())
}

/// The bytes that take a client which replayed `body` (from power-on) to the
/// live modes `live`. Empty when they already agree.
///
/// Order: screen, then scroll region and origin mode — both home the cursor,
/// so they go inside `ESC 7 … ESC 8` to keep the cursor the body left (the
/// bracket runs after any screen switch, so it is that screen's cursor; and it
/// is only used when needed, since it overwrites a cursor the program saved
/// itself) — then the input modes and cursor visibility, which move nothing.
pub fn postamble(body: &Modes, live: &Modes) -> Vec<u8> {
    if body == live {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut put = |s: String| out.extend_from_slice(s.as_bytes());
    if body.alt_screen != live.alt_screen {
        match (body.alt_screen, live.alt_screen) {
            (_, Some(m)) => put(format!("\x1b[?{m}h")),
            (Some(m), None) => put(format!("\x1b[?{m}l")),
            (None, None) => {}
        }
    }
    let mut homing = String::new();
    if body.scroll_region != live.scroll_region {
        match live.scroll_region {
            Some((top, bottom)) => homing.push_str(&format!("\x1b[{top};{bottom}r")),
            None => homing.push_str("\x1b[r"),
        }
    }
    if body.dec(6) != live.dec(6) {
        homing.push_str(if live.dec(6) == Some(true) { "\x1b[?6h" } else { "\x1b[?6l" });
    }
    if !homing.is_empty() {
        put(format!("\x1b7{homing}\x1b8"));
    }
    for (i, (mode, _)) in TRACKED_MODES.iter().enumerate() {
        // Synchronized output is a frame-length hold, never restored: a
        // program that died mid-frame would leave the pane frozen.
        if matches!(mode, 6 | 25 | 2026) || body.dec[i] == live.dec[i] {
            continue;
        }
        put(format!("\x1b[?{mode}{}", if live.dec[i] { 'h' } else { 'l' }));
    }
    for (b, l) in [(body.mouse_tracking, live.mouse_tracking), (body.mouse_encoding, live.mouse_encoding)] {
        match (b, l) {
            (b, Some(m)) if b != Some(m) => put(format!("\x1b[?{m}h")),
            (Some(m), None) => put(format!("\x1b[?{m}l")),
            _ => {}
        }
    }
    if body.keypad_application != live.keypad_application {
        put(if live.keypad_application { "\x1b=".into() } else { "\x1b>".into() });
    }
    if body.keyboard_flags() != live.keyboard_flags() {
        put(format!("\x1b[={};1u", live.keyboard_flags()));
    }
    if body.dec(25) != live.dec(25) {
        put(format!("\x1b[?25{}", if live.dec(25) == Some(true) { 'h' } else { 'l' }));
    }
    out
}

#[cfg(test)]
#[path = "vt_modes_tests.rs"]
mod tests;
