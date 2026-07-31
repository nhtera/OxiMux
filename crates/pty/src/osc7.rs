//! Out-of-band OSC scanner — runs alongside alacritty's parser to pick up
//! the OSC sequences alacritty does NOT surface as events: OSC 7 (cwd),
//! OSC 133/633 (shell-integration command marks), and OSC 9;4 (progress).
//!
//! Everything alacritty already decodes (OSC 0/2 title, OSC 52 clipboard,
//! OSC 4/10/11/12 color queries, device reports) is captured via the `Term`
//! event listener in `state.rs` instead — decoding those twice would
//! double-handle (two clipboard writes, two color replies).
//!
//! Shells with an OSC 7 hook (`precmd` / `PROMPT_COMMAND`) emit:
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
//! - Only the OSC numbers we handle emit a hit; every other payload (titles,
//!   hyperlinks, palette tweaks) is discarded after collection.
//! - Runaway protection: an OSC payload that grows past `MAX_OSC_BYTES`
//!   without a terminator is dropped and the scanner resets to Normal.
//!   The shell is broken; we don't want to leak memory chasing it.

use std::path::PathBuf;

use crate::events::CommandMarkKind;

/// Hard cap on a single OSC payload buffer. RFC has no real limit but
/// 4 KiB is several orders of magnitude over any realistic OSC 7 path.
const MAX_OSC_BYTES: usize = 4096;

/// A decoded OSC sequence the scanner cares about. Anything else is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscHit {
    /// OSC 7 — resolved absolute working directory.
    Cwd(PathBuf),
    /// OSC 133 / 633 — shell-integration command phase + optional exit code.
    CommandMark {
        kind: CommandMarkKind,
        exit: Option<i32>,
    },
    /// OSC 9;4 — progress. `state`: 0 clear, 1 set, 2 error, 3 indeterminate,
    /// 4 warning. `value` is a 0..=100 percentage.
    Progress { state: u8, value: u8 },
}

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

/// Stateful scanner: feed bytes via [`OscScanner::feed`]; whenever a handled
/// OSC payload completes, the callback receives an [`OscHit`]. Bytes that
/// aren't a handled OSC (or are corrupt) are silently discarded — the scanner
/// never panics on malformed input.
#[derive(Default)]
pub struct OscScanner {
    state: OscState,
}

impl OscScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; for every handled OSC payload that completes inside or
    /// across this chunk, `on_hit` is called. Non-handled traffic costs ~1 ns
    /// per byte (single match + byte compare) — cheap enough to run on every
    /// PTY read.
    pub fn feed<F: FnMut(OscHit)>(&mut self, bytes: &[u8], mut on_hit: F) {
        for &b in bytes {
            self.step(b, &mut on_hit);
        }
    }

    fn step<F: FnMut(OscHit)>(&mut self, b: u8, on_hit: &mut F) {
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
                    Self::dispatch(&buf, on_hit);
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
                    Self::dispatch(&buf, on_hit);
                    OscState::Normal
                }
                ESC => {
                    // Two ESCs back-to-back inside OSC — flush whatever we
                    // have, then re-arm for a potential new escape.
                    Self::dispatch(&buf, on_hit);
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

    fn dispatch<F: FnMut(OscHit)>(buf: &[u8], on_hit: &mut F) {
        // Split off the OSC number prefix (`<num>;<rest>`).
        let Some(semi) = buf.iter().position(|&b| b == b';') else {
            return;
        };
        let (num, rest) = (&buf[..semi], &buf[semi + 1..]);
        match num {
            // OSC 7 — `file://host/path` working directory.
            b"7" => {
                if let Ok(s) = std::str::from_utf8(rest)
                    && let Some(path) = parse_file_uri(s)
                {
                    on_hit(OscHit::Cwd(path));
                }
            }
            // OSC 133 / OSC 633 command marks share the
            // A/B/C/D phase tags; 633's extra E/F/P fields parse to None.
            b"133" | b"633" => {
                if let Some(hit) = parse_command_mark(rest) {
                    on_hit(hit);
                }
            }
            // OSC 9 is overloaded; only `9;4;state;value` (progress) is ours.
            // OSC 9;<text> desktop notifications are left to a future surface.
            b"9" => {
                if let Some(hit) = parse_progress(rest) {
                    on_hit(hit);
                }
            }
            _ => {}
        }
    }
}

/// Parse the payload AFTER the `133;`/`633;` prefix into a command mark.
/// `A`→PromptStart, `B`→CommandStart, `C`→OutputStart, `D[;exit]`→CommandEnd.
/// Unknown tags (633's `E`/`F`/`P`) return `None`.
fn parse_command_mark(rest: &[u8]) -> Option<OscHit> {
    let mut parts = rest.split(|&b| b == b';');
    let kind = match parts.next()? {
        b"A" => CommandMarkKind::PromptStart,
        b"B" => CommandMarkKind::CommandStart,
        b"C" => CommandMarkKind::OutputStart,
        b"D" => CommandMarkKind::CommandEnd,
        _ => return None,
    };
    // Exit code only meaningful on `D`; some shells append it as `D;<code>`.
    let exit = if kind == CommandMarkKind::CommandEnd {
        parts
            .next()
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.trim().parse::<i32>().ok())
    } else {
        None
    };
    Some(OscHit::CommandMark { kind, exit })
}

/// Parse the payload AFTER the `9;` prefix. Only `4;state;value` (progress)
/// is handled; `state`/`value` default to 0 when absent or non-numeric.
fn parse_progress(rest: &[u8]) -> Option<OscHit> {
    let mut parts = rest.split(|&b| b == b';');
    if parts.next()? != b"4" {
        return None;
    }
    let parse_u8 = |p: Option<&[u8]>| -> u8 {
        p.and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(0)
    };
    let state = parse_u8(parts.next());
    let value = parse_u8(parts.next()).min(100);
    Some(OscHit::Progress { state, value })
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
    let path = PathBuf::from(uri_path_to_native(&decoded));
    if path.is_absolute() { Some(path) } else { None }
}

/// Turn the path component of a `file:` URI into one the platform calls
/// absolute. A no-op off Windows.
fn uri_path_to_native(path: &str) -> &str {
    if cfg!(windows) {
        strip_drive_slash(path)
    } else {
        path
    }
}

/// Drop the slash a `file:` URI puts in front of a Windows drive letter.
///
/// URIs always spell the path with a leading slash, so `C:\Users\me` travels
/// as `file:///C:/Users/me` and arrives here as `/C:/Users/me`. Rust reads that
/// as rooted but prefixless, which on Windows is NOT absolute — so without this
/// the caller's absolute check discards every cwd a shell reports, and OSC 7 is
/// the only cwd source there (no `/proc`, no libproc).
///
/// Compiled on every platform, not `#[cfg]`-ed, so the rule stays testable
/// where the tests actually run.
fn strip_drive_slash(path: &str) -> &str {
    let b = path.as_bytes();
    let is_drive = b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':';
    if is_drive { &path[1..] } else { path }
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

    /// An absolute path and the URI body a shell would emit for it, spelled
    /// for whichever platform the test runs on. `/tmp/foo` is not absolute on
    /// Windows, so hard-coding it would have meant either a POSIX-only test or
    /// no Windows coverage at all — and OSC 7 is the ONLY cwd source there.
    fn abs(rel: &str) -> (String, PathBuf) {
        if cfg!(windows) {
            (format!("/C:/{rel}"), PathBuf::from(format!("C:/{rel}")))
        } else {
            (format!("/{rel}"), PathBuf::from(format!("/{rel}")))
        }
    }

    fn osc7(uri_body: &str) -> Vec<u8> {
        format!("\x1b]7;file://{uri_body}\x07").into_bytes()
    }

    fn collect(bytes: &[u8]) -> Vec<PathBuf> {
        let mut scanner = OscScanner::new();
        let mut paths = Vec::new();
        scanner.feed(bytes, |h| {
            if let OscHit::Cwd(p) = h {
                paths.push(p);
            }
        });
        paths
    }

    fn collect_hits(bytes: &[u8]) -> Vec<OscHit> {
        let mut scanner = OscScanner::new();
        let mut hits = Vec::new();
        scanner.feed(bytes, |h| hits.push(h));
        hits
    }

    #[test]
    fn bel_terminated_osc7_yields_path() {
        // `ESC ] 7 ; file://<abs> BEL`
        let (uri, want) = abs("tmp/foo");
        assert_eq!(collect(&osc7(&uri)), vec![want]);
    }

    #[test]
    fn st_terminated_osc7_yields_path() {
        // Same payload, terminated by ST (`ESC \`) rather than BEL.
        let (uri, want) = abs("tmp/bar");
        let bytes = format!("\x1b]7;file://{uri}\x1b\\").into_bytes();
        assert_eq!(collect(&bytes), vec![want]);
    }

    #[test]
    fn osc7_with_hostname_strips_host_correctly() {
        let (uri, want) = abs("Users/test/project");
        assert_eq!(collect(&osc7(&format!("localhost{uri}"))), vec![want]);
    }

    #[test]
    fn percent_encoded_space_decodes_correctly() {
        let (uri, _) = abs("tmp/with%20space");
        let (_, want) = abs("tmp/with space");
        assert_eq!(collect(&osc7(&uri)), vec![want]);
    }

    #[test]
    fn percent_encoded_utf8_decodes_correctly() {
        // `é` is U+00E9 = UTF-8 `0xC3 0xA9`.
        let (uri, _) = abs("tmp/%C3%A9");
        let (_, want) = abs("tmp/é");
        assert_eq!(collect(&osc7(&uri)), vec![want]);
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
        let (uri_a, want_a) = abs("a");
        let (uri_b, want_b) = abs("b");
        let bytes =
            format!("\x1b]7;file://{uri_a}\x07prompt$ \x1b]7;file://{uri_b}\x07").into_bytes();
        assert_eq!(collect(&bytes), vec![want_a, want_b]);
    }

    #[test]
    fn osc7_split_across_chunks_still_emits() {
        let mut scanner = OscScanner::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        let push_cwd = |s: &mut OscScanner, b: &[u8], v: &mut Vec<PathBuf>| {
            s.feed(b, |h| {
                if let OscHit::Cwd(p) = h {
                    v.push(p)
                }
            })
        };
        // Chunk 1: ESC ] 7 ; file://<abs>  — no terminator yet.
        let (uri, _) = abs("abs");
        let (_, want) = abs("abs/path");
        push_cwd(&mut scanner, format!("\x1b]7;file://{uri}").as_bytes(), &mut paths);
        assert!(paths.is_empty(), "no terminator yet");
        // Chunk 2: /path BEL
        push_cwd(&mut scanner, b"/path\x07", &mut paths);
        assert_eq!(paths, vec![want]);
    }

    #[test]
    fn st_split_across_chunks_still_emits() {
        // ESC \\ as ST split exactly at the ESC byte boundary.
        let mut scanner = OscScanner::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        let push_cwd = |s: &mut OscScanner, b: &[u8], v: &mut Vec<PathBuf>| {
            s.feed(b, |h| {
                if let OscHit::Cwd(p) = h {
                    v.push(p)
                }
            })
        };
        let (uri, want) = abs("x");
        push_cwd(&mut scanner, format!("\x1b]7;file://{uri}\x1b").as_bytes(), &mut paths);
        assert!(paths.is_empty(), "ST not completed");
        push_cwd(&mut scanner, b"\\rest", &mut paths);
        assert_eq!(paths, vec![want]);
    }

    #[test]
    fn osc133_prompt_and_command_end_decode() {
        // FinalTerm prompt mark, then a non-zero command exit.
        let hits = collect_hits(b"\x1b]133;A\x07\x1b]133;D;1\x07");
        assert_eq!(
            hits,
            vec![
                OscHit::CommandMark {
                    kind: CommandMarkKind::PromptStart,
                    exit: None,
                },
                OscHit::CommandMark {
                    kind: CommandMarkKind::CommandEnd,
                    exit: Some(1),
                },
            ]
        );
    }

    #[test]
    fn osc133_command_end_without_exit_is_none() {
        let hits = collect_hits(b"\x1b]133;D\x1b\\");
        assert_eq!(
            hits,
            vec![OscHit::CommandMark {
                kind: CommandMarkKind::CommandEnd,
                exit: None,
            }]
        );
    }

    #[test]
    fn osc633_shares_command_mark_semantics() {
        // 633 adds E/F/P; A/B/C/D map the same, E (command line) is ignored.
        let hits = collect_hits(b"\x1b]633;A\x07\x1b]633;E;ls -la\x07\x1b]633;D;0\x07");
        assert_eq!(
            hits,
            vec![
                OscHit::CommandMark {
                    kind: CommandMarkKind::PromptStart,
                    exit: None,
                },
                OscHit::CommandMark {
                    kind: CommandMarkKind::CommandEnd,
                    exit: Some(0),
                },
            ]
        );
    }

    #[test]
    fn osc9_4_progress_decodes_state_and_value() {
        let hits = collect_hits(b"\x1b]9;4;1;42\x07");
        assert_eq!(hits, vec![OscHit::Progress { state: 1, value: 42 }]);
    }

    #[test]
    fn osc9_4_progress_clamps_value_and_defaults_missing() {
        // Over-100 clamps; a bare `9;4` (clear) defaults both to 0.
        assert_eq!(
            collect_hits(b"\x1b]9;4;1;250\x07"),
            vec![OscHit::Progress {
                state: 1,
                value: 100
            }]
        );
        assert_eq!(
            collect_hits(b"\x1b]9;4\x07"),
            vec![OscHit::Progress { state: 0, value: 0 }]
        );
    }

    #[test]
    fn osc9_non_progress_is_ignored() {
        // OSC 9;<text> desktop notification — not progress, no hit.
        assert!(collect_hits(b"\x1b]9;build done\x07").is_empty());
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
        let (uri, want) = abs("ok");
        let mut bytes = Vec::with_capacity(5200);
        bytes.extend_from_slice(b"\x1b]7;");
        bytes.extend(std::iter::repeat_n(b'x', 5000));
        bytes.extend_from_slice(&osc7(&uri));
        let paths = collect(&bytes);
        // Runaway dropped; second OSC 7 still parses.
        assert_eq!(paths, vec![want]);
    }

    // POSIX-only spelling: `/relative/...` has no drive, so on Windows it is
    // correctly rejected as not-absolute rather than passed through.
    #[cfg(unix)]
    #[test]
    fn relative_path_is_rejected() {
        // No leading slash on the path component.
        let bytes = b"\x1b]7;file://host/relative/path/../escape\x07";
        let paths = collect(bytes);
        // `/relative/path/../escape` is absolute (starts with `/`) — accept
        // it; the OS resolves `..` later. The point of this test is just
        // to document that we DON'T normalize and DON'T reject `..`.
        assert_eq!(paths, vec![PathBuf::from("/relative/path/../escape")]);
    }

    #[test]
    fn a_windows_drive_loses_the_uri_slash() {
        // What a shell on Windows emits for C:\Users\me\proj. The rule runs
        // on every platform so it can be checked here; only the caller is
        // cfg-ed, because "/C:/x" is a legitimate (if odd) path on unix.
        assert_eq!(strip_drive_slash("/C:/Users/me/proj"), "C:/Users/me/proj");
        assert_eq!(strip_drive_slash("/d:/lower/drive"), "d:/lower/drive");
        // A POSIX path must survive untouched, including one that merely
        // starts with a letter or contains a colon further along.
        assert_eq!(strip_drive_slash("/Users/me/proj"), "/Users/me/proj");
        assert_eq!(strip_drive_slash("/c/not-a-drive"), "/c/not-a-drive");
        assert_eq!(strip_drive_slash("/tmp/a:b"), "/tmp/a:b");
        // Too short to be a drive spec.
        assert_eq!(strip_drive_slash("/C"), "/C");
        assert_eq!(strip_drive_slash(""), "");
    }

    #[cfg(windows)]
    #[test]
    fn windows_osc7_yields_an_absolute_path() {
        // The end-to-end shape: without the slash fix `is_absolute()` is false
        // and the hit is dropped, so cwd tracking never reports anything.
        let paths = collect(b"\x1b]7;file:///C:/Users/me/proj\x07");
        assert_eq!(paths, vec![PathBuf::from("C:/Users/me/proj")]);
        assert!(paths[0].is_absolute());
    }

    #[test]
    fn non_file_scheme_is_ignored() {
        let bytes = b"\x1b]7;https://example.com/path\x07";
        let paths = collect(bytes);
        assert!(paths.is_empty());
    }
}
