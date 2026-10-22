//! Answers the OSC 7501 program-status feature query for the PTYs this daemon
//! owns.
//!
//! A program asks `ESC ] 7501 ; ? ST` once, and only reports its status (idle,
//! working, waiting on the user…) after the terminal answers. Every client
//! parses the stream for itself, so if each answered the program would get one
//! reply per attached window or phone; the daemon owns the PTY and answers
//! exactly once, whether or not anyone is attached.
//!
//! Only the query is matched here: reports are parsed by the clients. The relay
//! deliberately does not depend on the terminal crate, so this is a small
//! streaming matcher of its own.
//!
//! The reply must never stall the reader: it is handed to the PTY's writer
//! thread with a non-blocking enqueue, at most one at a time. And it must never
//! loop: our reply is itself a well-formed query, so a program that echoes its
//! input (a `cat` with no arguments) would bounce it back forever. A query that
//! arrives within [`ECHO_WINDOW`] of our last reply being written is treated as
//! that echo and not answered.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The reply: a bare `?`, meaning every default state and kind.
pub(crate) const PROGRAM_STATUS_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// A second query this soon after the last answer is not answered.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// A query this soon after our reply hit the PTY is taken for its echo.
const ECHO_WINDOW: Duration = Duration::from_millis(500);
/// At most this many replies per [`CAP_WINDOW`]: a program answered once has
/// no reason to keep asking, and each reply is another chance to queue behind a
/// program that has stopped reading its input.
const MAX_REPLIES_PER_WINDOW: usize = 8;
const CAP_WINDOW: Duration = Duration::from_secs(60);
/// OSC payloads past this are not a query; the scan gives up on them.
const MAX_OSC_BYTES: usize = 4096;
/// `7501;?` — all of the payload a query needs before its terminator.
const QUERY_PREFIX: &[u8] = b"7501;?";

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// State shared between the detector (which enqueues a reply) and the writer
/// thread (which reports when it actually reached the PTY).
#[derive(Default)]
pub(crate) struct ReplyTicket {
    outstanding: AtomicBool,
    written_at: Mutex<Option<Instant>>,
}

impl ReplyTicket {
    /// Called by the writer thread once the reply bytes are written (or failed).
    pub(crate) fn mark_written(&self, at: Instant) {
        *self.written_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(at);
        self.outstanding.store(false, Ordering::Release);
    }
}

#[derive(Default)]
enum Scan {
    #[default]
    Normal,
    AfterEsc,
    /// Inside an OSC; `len` payload bytes seen, `matches` while they still
    /// spell a prefix of [`QUERY_PREFIX`] (or the whole of it).
    InOsc { len: usize, matches: bool },
    InOscAfterEsc { len: usize, matches: bool },
}

/// Streaming matcher for the feature query plus the reply gate. One per PTY,
/// fed every chunk of live output; replayed history never passes through it.
pub(crate) struct ProgramStatusQueryDetector {
    scan: Scan,
    ticket: Arc<ReplyTicket>,
    last_enqueued: Option<Instant>,
    /// When each recent reply was enqueued, oldest first, for the rate cap.
    recent: std::collections::VecDeque<Instant>,
}

impl ProgramStatusQueryDetector {
    pub(crate) fn new() -> Self {
        Self {
            scan: Scan::default(),
            ticket: Arc::new(ReplyTicket::default()),
            last_enqueued: None,
            recent: std::collections::VecDeque::new(),
        }
    }

    /// Scan one live chunk. For each complete query the gate lets through,
    /// `enqueue` is offered the reply ticket and returns whether the writer
    /// accepted it (a full queue drops the reply).
    pub(crate) fn feed(&mut self, bytes: &[u8], now: Instant, mut enqueue: impl FnMut(&Arc<ReplyTicket>) -> bool) {
        for &b in bytes {
            if self.step(b) && self.gate_open(now) {
                self.ticket.outstanding.store(true, Ordering::Release);
                if enqueue(&self.ticket) {
                    self.last_enqueued = Some(now);
                    self.recent.push_back(now);
                } else {
                    self.ticket.outstanding.store(false, Ordering::Release);
                }
            }
        }
    }

    fn gate_open(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|t| now.saturating_duration_since(*t) >= CAP_WINDOW) {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_REPLIES_PER_WINDOW || self.ticket.outstanding.load(Ordering::Acquire) {
            return false;
        }
        if self.last_enqueued.is_some_and(|t| now.saturating_duration_since(t) < DEBOUNCE) {
            return false;
        }
        let written = *self.ticket.written_at.lock().unwrap_or_else(|p| p.into_inner());
        written.is_none_or(|t| now.saturating_duration_since(t) >= ECHO_WINDOW)
    }

    /// Advance one byte; `true` when it completed a query.
    fn step(&mut self, b: u8) -> bool {
        let (next, hit) = match std::mem::take(&mut self.scan) {
            Scan::Normal | Scan::AfterEsc if b == ESC => (Scan::AfterEsc, false),
            Scan::Normal => (Scan::Normal, false),
            Scan::AfterEsc if b == b']' => (Scan::InOsc { len: 0, matches: true }, false),
            Scan::AfterEsc => (Scan::Normal, false),
            Scan::InOsc { len, matches } => match b {
                BEL => (Scan::Normal, is_query(len, matches)),
                ESC => (Scan::InOscAfterEsc { len, matches }, false),
                _ if len >= MAX_OSC_BYTES => (Scan::Normal, false),
                _ => {
                    let matches = matches && QUERY_PREFIX.get(len).is_none_or(|&want| want == b);
                    (Scan::InOsc { len: len + 1, matches }, false)
                }
            },
            Scan::InOscAfterEsc { len, matches } => match b {
                b'\\' => (Scan::Normal, is_query(len, matches)),
                // A fresh escape: the OSC was cut short; treat this ESC as the
                // start of whatever comes next.
                ESC => (Scan::AfterEsc, false),
                b']' => (Scan::InOsc { len: 0, matches: true }, false),
                _ => (Scan::Normal, false),
            },
        };
        self.scan = next;
        hit
    }
}

/// A finished OSC is the query when its payload began with `7501;?`.
fn is_query(len: usize, matches: bool) -> bool {
    matches && len >= QUERY_PREFIX.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `chunks` at `now`, accepting every enqueue; returns replies sent.
    fn replies(det: &mut ProgramStatusQueryDetector, chunks: &[&[u8]], now: Instant) -> usize {
        let mut n = 0;
        for c in chunks {
            det.feed(c, now, |_| {
                n += 1;
                true
            });
        }
        n
    }

    #[test]
    fn a_query_with_either_terminator_is_answered() {
        let now = Instant::now();
        assert_eq!(replies(&mut ProgramStatusQueryDetector::new(), &[b"\x1b]7501;?\x07"], now), 1);
        assert_eq!(replies(&mut ProgramStatusQueryDetector::new(), &[b"\x1b]7501;?\x1b\\"], now), 1);
    }

    #[test]
    fn a_query_split_at_every_byte_is_answered_once() {
        let whole: &[u8] = b"text\x1b[31m\x1b]7501;?\x07\x1b[cmore";
        for split in 0..=whole.len() {
            let mut det = ProgramStatusQueryDetector::new();
            let n = replies(&mut det, &[&whole[..split], &whole[split..]], Instant::now());
            assert_eq!(n, 1, "split at {split}");
        }
    }

    #[test]
    fn reports_and_other_oscs_are_not_queries() {
        let now = Instant::now();
        for bytes in [
            &b"\x1b]7501;state=idle\x07"[..],
            b"\x1b]7501;\x07",
            b"\x1b]750;?\x07",
            b"\x1b]75011;?\x07",
            b"\x1b]0;7501;?\x07",
            b"\x1b]9999;?\x07",
        ] {
            assert_eq!(replies(&mut ProgramStatusQueryDetector::new(), &[bytes], now), 0, "{bytes:?}");
        }
    }

    #[test]
    fn only_one_reply_is_outstanding_and_repeats_are_debounced() {
        let mut det = ProgramStatusQueryDetector::new();
        let t0 = Instant::now();
        let mut tickets = Vec::new();
        det.feed(b"\x1b]7501;?\x07\x1b]7501;?\x07", t0, |t| {
            tickets.push(Arc::clone(t));
            true
        });
        assert_eq!(tickets.len(), 1, "a second query while one reply is queued");
        // Written at once; a query 100 ms later is still inside the window.
        tickets[0].mark_written(t0);
        assert_eq!(replies(&mut det, &[b"\x1b]7501;?\x07"], t0 + Duration::from_millis(100)), 0);
        assert_eq!(replies(&mut det, &[b"\x1b]7501;?\x07"], t0 + Duration::from_millis(600)), 1);
    }

    #[test]
    fn our_echoed_reply_is_not_answered_again() {
        let mut det = ProgramStatusQueryDetector::new();
        let t0 = Instant::now();
        let mut ticket = None;
        det.feed(b"\x1b]7501;?\x07", t0, |t| {
            ticket = Some(Arc::clone(t));
            true
        });
        // The writer was slow: the reply lands 2 s later and is echoed back.
        let written = t0 + Duration::from_secs(2);
        ticket.unwrap().mark_written(written);
        let echo = PROGRAM_STATUS_REPLY;
        assert_eq!(replies(&mut det, &[echo], written + Duration::from_millis(20)), 0);
    }

    #[test]
    fn a_dropped_reply_does_not_block_the_next_query() {
        let mut det = ProgramStatusQueryDetector::new();
        let t0 = Instant::now();
        det.feed(b"\x1b]7501;?\x07", t0, |_| false);
        assert_eq!(replies(&mut det, &[b"\x1b]7501;?\x07"], t0 + Duration::from_millis(1)), 1);
    }

    #[test]
    fn a_query_flood_is_capped_per_minute() {
        let mut det = ProgramStatusQueryDetector::new();
        let t0 = Instant::now();
        let mut answered = 0;
        for i in 0..200u64 {
            let now = t0 + Duration::from_secs(i);
            det.feed(b"\x1b]7501;?\x07", now, |t| {
                t.mark_written(now);
                answered += 1;
                true
            });
        }
        // 200 queries a second apart over 200 s: 8 per minute, at most 4 windows.
        assert!(answered <= 4 * MAX_REPLIES_PER_WINDOW, "{answered}");
        assert!(answered >= MAX_REPLIES_PER_WINDOW);
    }

    #[test]
    fn a_runaway_osc_is_abandoned() {
        let mut det = ProgramStatusQueryDetector::new();
        let mut bytes = b"\x1b]7501;?".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_OSC_BYTES + 10));
        bytes.extend_from_slice(b"\x07\x1b]7501;?\x07");
        // The overlong one is dropped; the well-formed one after it answers.
        assert_eq!(replies(&mut det, &[&bytes], Instant::now()), 1);
    }
}
