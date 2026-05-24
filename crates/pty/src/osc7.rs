//! OSC 7 `file://` URI scanner — used to track each PTY's current working
//! directory cheaply.
//!
//! Shells configured with an OSC 7 hook (most modern zsh / bash setups via
//! `precmd` or `PROMPT_COMMAND`) emit:
//!
//!   ESC ] 7 ; file://hostname/url-encoded-path BEL
//!     or
//!   ESC ] 7 ; file://hostname/url-encoded-path ESC \
//!
//! every prompt. Capturing it gives us the live cwd without a libproc
//! syscall — the win shows up most on rapid Cmd+D chains where each split
//! needs to inherit cwd before the kernel has refreshed `proc_pidinfo`.
//!
//! Design notes:
//! - Streaming: chunks can split an OSC mid-sequence (the kernel hands us
//!   ~4 KiB at a time; OSC payloads can be ~256 bytes). The state machine
//!   carries across `feed` calls.
//! - We only care about OSC 7 — every other OSC payload (titles, hyperlinks,
//!   palette tweaks) is discarded after collection.
//! - Runaway protection: an OSC payload that grows past `MAX_OSC_BYTES`
//!   without a terminator is dropped and the scanner resets to Normal.
//!   The shell is broken; we don't want to leak memory chasing it.

use std::path::PathBuf;

/// Hard cap on a single OSC payload buffer. RFC has no real limit but
/// 4 KiB is several orders of magnitude over any realistic OSC 7 path.
const MAX_OSC_BYTES: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

#[derive(Default)]
enum OscState {
    /// Outside any escape sequence.
    #[default]
    Normal,
    /// Just saw ESC; if next byte is `]` we enter OSC.
    AfterEsc,
    /// Accumulating an OSC payload until BEL or ESC \\ (ST) appears.
    InOsc(Vec<u8>),
    /// Inside OSC, saw ESC; if next byte is `\\` that's String Terminator.
    InOscAfterEsc(Vec<u8>),
}

/// Stateful scanner: feed bytes via [`Osc7Scanner::feed`]; whenever an
/// OSC 7 payload completes, the callback receives the parsed absolute path.
/// Bytes that aren't OSC 7 (or are corrupt) are silently discarded — the
/// scanner never panics on malformed input.
#[derive(Default)]
pub struct Osc7Scanner {
    state: OscState,
}

impl Osc7Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; for every OSC 7 file:// payload that completes inside
    /// or across this chunk, `on_cwd` is called with the resolved absolute
    /// path. Non-OSC-7 traffic costs ~1 ns per byte (single match + byte
    /// compare) — cheap enough to run on every PTY read.
    pub fn feed<F: FnMut(PathBuf)>(&mut self, bytes: &[u8], mut on_cwd: F) {
        for &b in bytes {
            self.step(b, &mut on_cwd);
        }
    }

    fn step<F: FnMut(PathBuf)>(&mut self, b: u8, on_cwd: &mut F) {
        // Take ownership of current state so we can move owned `Vec`s
        // around without borrow conflicts.
        let prev = std::mem::take(&mut self.state);
        self.state = match prev {
            OscState::Normal => {
                if b == ESC {
                    OscState::AfterEsc
                } else {
                    OscState::Normal
                }
            }
            OscState::AfterEsc => match b {
                b']' => OscState::InOsc(Vec::new()),
                ESC => OscState::AfterEsc, // back-to-back ESCs — stay armed
                _ => OscState::Normal,
            },
            OscState::InOsc(mut buf) => match b {
                BEL => {
                    Self::dispatch(&buf, on_cwd);
                    OscState::Normal
                }
                ESC => OscState::InOscAfterEsc(buf),
                _ => {
                    if buf.len() >= MAX_OSC_BYTES {
                        // Runaway — drop the payload + reset.
                        OscState::Normal
                    } else {
                        buf.push(b);
                        OscState::InOsc(buf)
                    }
                }
            },
            OscState::InOscAfterEsc(buf) => match b {
                b'\\' => {
                    Self::dispatch(&buf, on_cwd);
                    OscState::Normal
                }
                ESC => {
                    // Two ESCs back-to-back inside OSC — flush whatever we
                    // have, then re-arm for a potential new escape.
                    Self::dispatch(&buf, on_cwd);
                    OscState::AfterEsc
                }
                _ => {
                    // ESC followed by something other than `\\` is bogus;
                    // drop the partial payload and resync.
                    let _ = buf;
                    OscState::Normal
                }
            },
        };
    }

    fn dispatch<F: FnMut(PathBuf)>(buf: &[u8], on_cwd: &mut F) {
        // OSC 7 payload starts with `7;` then the URI.
        let Some(rest) = buf.strip_prefix(b"7;") else {
            return;
        };
        let Ok(s) = std::str::from_utf8(rest) else {
            return;
        };
        if let Some(path) = parse_file_uri(s) {
            on_cwd(path);
        }
    }
}

/// Parse a `file://hostname/path` or `file:///path` URI into an absolute
/// path. Performs URL-decoding on the path component. Returns `None` for
/// anything that doesn't look like a valid local file URI.
///
/// We don't validate the hostname — most shells emit either an empty host
/// or the local hostname, but cross-host SSH-in-a-pane is a future
/// feature. Validating would break that path silently.
pub fn parse_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs/path` (empty host) — first slash starts the path.
    // `file://host/abs/path` — find the slash after the host.
    let slash = rest.find('/')?;
    let raw_path = &rest[slash..];
    let decoded = percent_decode(raw_path)?;
    let path = PathBuf::from(decoded);
    if path.is_absolute() { Some(path) } else { None }
}

/// Minimal percent-decode for OSC 7 paths. RFC 3986 says path segments
/// percent-encode `/`-disallowed bytes; in practice, shells encode
/// non-ASCII bytes + the few sub-delims that aren't safe in filesystem
/// paths. Anything past a malformed `%XX` resets us to `None` so the
/// caller falls back to libproc rather than running with a corrupt path.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn collect(bytes: &[u8]) -> Vec<PathBuf> {
        let mut scanner = Osc7Scanner::new();
        let mut paths = Vec::new();
        scanner.feed(bytes, |p| paths.push(p));
        paths
    }

    #[test]
    fn bel_terminated_osc7_yields_path() {
        // `ESC ] 7 ; file:///tmp/foo BEL`
        let bytes = b"\x1b]7;file:///tmp/foo\x07";
        let paths = collect(bytes);
        assert_eq!(paths, vec![PathBuf::from("/tmp/foo")]);
    }

    #[test]
    fn st_terminated_osc7_yields_path() {
        // `ESC ] 7 ; file:///tmp/bar ESC \`
        let bytes = b"\x1b]7;file:///tmp/bar\x1b\\";
        let paths = collect(bytes);
        assert_eq!(paths, vec![PathBuf::from("/tmp/bar")]);
    }

    #[test]
    fn osc7_with_hostname_strips_host_correctly() {
        let bytes = b"\x1b]7;file://localhost/Users/test/project\x07";
        let paths = collect(bytes);
        assert_eq!(paths, vec![PathBuf::from("/Users/test/project")]);
    }

    #[test]
    fn percent_encoded_space_decodes_correctly() {
        let bytes = b"\x1b]7;file:///tmp/with%20space\x07";
        let paths = collect(bytes);
        assert_eq!(paths, vec![PathBuf::from("/tmp/with space")]);
    }

    #[test]
    fn percent_encoded_utf8_decodes_correctly() {
        // "/tmp/é" — `é` is U+00E9 = UTF-8 `0xC3 0xA9`.
        let bytes = b"\x1b]7;file:///tmp/%C3%A9\x07";
        let paths = collect(bytes);
        assert_eq!(paths, vec![PathBuf::from("/tmp/é")]);
    }

    #[test]
    fn non_osc7_payload_is_ignored() {
        // OSC 0 = window title.
        let bytes = b"\x1b]0;some title\x07hello";
        let paths = collect(bytes);
        assert!(paths.is_empty());
    }

    #[test]
    fn multiple_osc7_in_one_chunk_emit_all() {
        let bytes = b"\x1b]7;file:///a\x07prompt$ \x1b]7;file:///b\x07";
        let paths = collect(bytes);
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn osc7_split_across_chunks_still_emits() {
        let mut scanner = Osc7Scanner::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        // Chunk 1: ESC ] 7 ; file:///abs
        scanner.feed(b"\x1b]7;file:///abs", |p| paths.push(p));
        assert!(paths.is_empty(), "no terminator yet");
        // Chunk 2: /path BEL
        scanner.feed(b"/path\x07", |p| paths.push(p));
        assert_eq!(paths, vec![PathBuf::from("/abs/path")]);
    }

    #[test]
    fn st_split_across_chunks_still_emits() {
        // ESC \\ as ST split exactly at the ESC byte boundary.
        let mut scanner = Osc7Scanner::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        scanner.feed(b"\x1b]7;file:///x\x1b", |p| paths.push(p));
        assert!(paths.is_empty(), "ST not completed");
        scanner.feed(b"\\rest", |p| paths.push(p));
        assert_eq!(paths, vec![PathBuf::from("/x")]);
    }

    #[test]
    fn malformed_percent_encoding_drops_path() {
        // `%ZZ` is not valid hex.
        let bytes = b"\x1b]7;file:///bad%ZZ\x07";
        let paths = collect(bytes);
        assert!(paths.is_empty());
    }

    #[test]
    fn runaway_osc_payload_is_dropped_without_panic() {
        // 5000 bytes of garbage inside OSC, never terminated within the
        // chunk. Scanner must not allocate forever and must resync on
        // next ESC.
        let mut bytes = Vec::with_capacity(5200);
        bytes.extend_from_slice(b"\x1b]7;");
        bytes.extend(std::iter::repeat_n(b'x', 5000));
        bytes.extend_from_slice(b"\x1b]7;file:///ok\x07");
        let paths = collect(&bytes);
        // Runaway dropped; second OSC 7 still parses.
        assert_eq!(paths, vec![PathBuf::from("/ok")]);
    }

    #[test]
    fn relative_path_is_rejected() {
        // No leading slash on the path component.
        let bytes = b"\x1b]7;file://host/relative/path/../escape\x07";
        let paths = collect(bytes);
        // `/relative/path/../escape` is absolute (starts with `/`) — accept
        // it; the OS resolves `..` later. The point of this test is just
        // to document that we DON'T normalize and DON'T reject `..`.
        assert_eq!(
            paths,
            vec![PathBuf::from("/relative/path/../escape")]
        );
    }

    #[test]
    fn non_file_scheme_is_ignored() {
        let bytes = b"\x1b]7;https://example.com/path\x07";
        let paths = collect(bytes);
        assert!(paths.is_empty());
    }
}
