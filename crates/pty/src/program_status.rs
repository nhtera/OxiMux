//! Program status (OSC 7501): the open protocol a program uses to tell its
//! terminal what it is doing — idle, working, waiting on the user, done, or
//! failed — so the terminal can surface it without scraping the screen.
//!
//! This module is pure: the report grammar, the per-terminal record store and
//! its lifetime rules, and the text sanitizer every display surface shares. The
//! byte scanner (`osc7.rs`) hands it complete payloads; `TerminalState` owns one
//! store per terminal session and reports changes of the root record.
//!
//! Wire format (rev 0.4):
//!
//! ```text
//! OSC 7501 ; pairs ST      report
//! OSC 7501 ; ? ST          feature query — answered with OSC 7501 ; ? ST
//! pairs := pair (":" pair)*    pair := key "=" value
//! key   := [a-z]+              value := [A-Za-z0-9_.,+/=-]*
//! ```
//!
//! A malformed pair is skipped and the rest of the report still applies; a
//! report that breaks any size limit, carries bad base64, or decodes to a
//! control character is dropped whole.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

/// Bytes this terminal writes back to answer a feature query. A bare `?` means
/// "every default state and kind", which is exactly what we support.
pub const PROGRAM_STATUS_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// Records kept per terminal. The spec requires at least 64 and allows 256;
/// the least recently updated record is evicted past the cap.
pub const MAX_RECORDS: usize = 256;

/// Whole sequence, `ESC ] 7501 ;` through ST. The payload handed to the parser
/// is the part after `7501;`, so it may use this much less the 8 framing bytes.
const MAX_SEQUENCE_BYTES: usize = 4096;
const SEQUENCE_FRAMING_BYTES: usize = 8;
const MAX_KEY_BYTES: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG_DECODED: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE_DECODED: usize = 192;
const MAX_APP_BYTES: usize = 32;
const MAX_ID_BYTES: usize = 128;
const MAX_ID_SEGMENT_BYTES: usize = 32;
const MAX_ID_DEPTH: usize = 8;

/// Standard alphabet, padding optional on decode (the spec allows either).
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// What a program says it is doing. `clear` is not a state a record can hold;
/// it is a [`ProgramReport::Clear`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

/// Why a `blocked` program is waiting. An unrecognized kind reads as absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

/// One record, exactly as its last report described it: a report replaces the
/// whole record, so keys it omits are gone. `msg` and `title` are decoded and
/// already passed through [`sanitize_display_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramRecord {
    pub state: ProgramState,
    /// Only meaningful for `Blocked`.
    pub kind: Option<BlockedKind>,
    /// 0..=100, only for `Working`/`Blocked`; `None` is indeterminate.
    pub progress: Option<u8>,
    /// The record's own `app`. Inheritance from ancestors is resolved by
    /// [`ProgramStatusStore::app_of`].
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

/// A parsed report: set one record, or clear a record and its subtree (`id`
/// `None` clears every record on the terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramReport {
    Set {
        id: Vec<String>,
        record: ProgramRecord,
    },
    Clear {
        id: Option<Vec<String>>,
    },
}

/// `true` when an OSC 7501 payload (the bytes after `7501;`) is a feature
/// query rather than a report.
pub fn is_query(payload: &[u8]) -> bool {
    payload.first() == Some(&b'?')
}

/// Parse an OSC 7501 report payload (the bytes after `7501;`). `None` drops the
/// report: a limit breach, a missing or unknown `state`, a malformed `id`, bad
/// base64, or decoded text holding a control character.
pub fn parse_report(payload: &[u8]) -> Option<ProgramReport> {
    if payload.len() > MAX_SEQUENCE_BYTES - SEQUENCE_FRAMING_BYTES || is_query(payload) {
        return None;
    }
    let mut state: Option<&[u8]> = None;
    let mut id: Option<&[u8]> = None;
    let mut kind: Option<&[u8]> = None;
    let mut progress: Option<&[u8]> = None;
    let mut app: Option<&[u8]> = None;
    let mut title: Option<&[u8]> = None;
    let mut msg: Option<&[u8]> = None;
    for pair in payload.split(|&b| b == b':') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = pair[..eq].trim_ascii();
        let value = pair[eq + 1..].trim_ascii();
        if key.len() > MAX_KEY_BYTES {
            return None;
        }
        if key.is_empty() || !key.iter().all(u8::is_ascii_lowercase) || !value.iter().all(|&b| is_value_byte(b)) {
            continue;
        }
        // Last value wins for a repeated key; unknown keys are ignored.
        let slot = match key {
            b"state" => &mut state,
            b"id" => &mut id,
            b"kind" => &mut kind,
            b"progress" => &mut progress,
            b"app" => &mut app,
            b"title" => &mut title,
            b"msg" => &mut msg,
            _ => continue,
        };
        *slot = Some(value);
    }

    let id = match id {
        Some(raw) => Some(parse_id(raw)?),
        None => None,
    };
    let state = match state? {
        b"idle" => ProgramState::Idle,
        b"working" => ProgramState::Working,
        b"done" => ProgramState::Done,
        b"blocked" => ProgramState::Blocked,
        b"error" => ProgramState::Error,
        b"clear" => return Some(ProgramReport::Clear { id }),
        _ => return None,
    };
    let kind = match (state, kind) {
        (ProgramState::Blocked, Some(b"permission")) => Some(BlockedKind::Permission),
        (ProgramState::Blocked, Some(b"question")) => Some(BlockedKind::Question),
        (ProgramState::Blocked, Some(b"auth")) => Some(BlockedKind::Auth),
        _ => None,
    };
    let progress = match state {
        ProgramState::Working | ProgramState::Blocked => progress
            .and_then(|p| std::str::from_utf8(p).ok())
            .and_then(|p| p.parse::<u8>().ok())
            .filter(|p| *p <= 100),
        _ => None,
    };
    let app = match app {
        Some(raw) if raw.len() > MAX_APP_BYTES => return None,
        Some(raw) if !raw.is_empty() && raw.iter().all(|&b| is_segment_byte(b)) => {
            Some(String::from_utf8_lossy(raw).into_owned())
        }
        _ => None,
    };
    let title = decode_text(title, MAX_TITLE_ENCODED, MAX_TITLE_DECODED)?;
    let msg = decode_text(msg, MAX_MSG_ENCODED, MAX_MSG_DECODED)?;
    Some(ProgramReport::Set {
        id: id.unwrap_or_default(),
        record: ProgramRecord {
            state,
            kind,
            progress,
            app,
            title,
            msg,
        },
    })
}

fn is_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn is_segment_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
}

/// `segment ("/" segment)*`. A bad id drops the report — it never falls back
/// to the root record.
fn parse_id(raw: &[u8]) -> Option<Vec<String>> {
    if raw.is_empty() || raw.len() > MAX_ID_BYTES {
        return None;
    }
    let mut segments = Vec::new();
    for seg in raw.split(|&b| b == b'/') {
        if seg.is_empty() || seg.len() > MAX_ID_SEGMENT_BYTES || !seg.iter().all(|&b| is_segment_byte(b)) {
            return None;
        }
        segments.push(String::from_utf8_lossy(seg).into_owned());
    }
    (segments.len() <= MAX_ID_DEPTH).then_some(segments)
}

/// Decode one free-text value. Outer `None` drops the whole report; inner
/// `None` means the key was absent (or empty).
fn decode_text(raw: Option<&[u8]>, max_encoded: usize, max_decoded: usize) -> Option<Option<String>> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return Some(None);
    };
    if raw.len() > max_encoded {
        return None;
    }
    let bytes = B64.decode(raw).ok()?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    // C0, DEL and C1 — newline and tab included — are forbidden outright.
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(Some(sanitize_display_text(&text)))
}

/// Strip what makes untrusted text lie about itself on screen: bidi overrides
/// and isolates (which can reorder a message into something it does not say),
/// zero-width and other invisible formatting characters, and any control
/// character. Shared by every surface that shows program-supplied text.
pub fn sanitize_display_text(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            !c.is_control()
                && !matches!(c,
                    '\u{061C}'
                    | '\u{00AD}'
                    | '\u{180E}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{FEFF}'
                    | '\u{FFF9}'..='\u{FFFB}')
        })
        .collect()
}

/// A second query this soon after the last answer is not answered.
const REPLY_DEBOUNCE: Duration = Duration::from_millis(250);
/// A query this soon after our reply was written is taken for its echo: the
/// reply is itself a well-formed query, so a program echoing its input would
/// otherwise bounce it back forever.
const REPLY_ECHO_WINDOW: Duration = Duration::from_millis(500);
/// At most this many replies per [`REPLY_CAP_WINDOW`]; a program answered once
/// has no reason to keep asking.
const MAX_REPLIES_PER_WINDOW: usize = 8;
const REPLY_CAP_WINDOW: Duration = Duration::from_secs(60);

/// Shared between the gate that admits a reply and whoever writes it, so the
/// gate knows when the reply actually reached the PTY.
#[derive(Default)]
pub struct ReplyTicket {
    outstanding: AtomicBool,
    written_at: Mutex<Option<Instant>>,
}

impl ReplyTicket {
    /// Record that the reply was written (or that writing it failed).
    pub fn mark_written(&self, at: Instant) {
        *self.written_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(at);
        self.outstanding.store(false, Ordering::Release);
    }
}

/// Decides which feature queries get an answer: one reply outstanding at a
/// time, debounced, rate-capped, and never in answer to our own echo. The
/// reply itself must be written off the read path by the caller.
#[derive(Default)]
pub struct ReplyGate {
    ticket: Arc<ReplyTicket>,
    last_admitted: Option<Instant>,
    recent: VecDeque<Instant>,
}

impl ReplyGate {
    /// A query arrived at `now`. `Some(ticket)` means answer it: write
    /// [`PROGRAM_STATUS_REPLY`], then call `ticket.mark_written`.
    pub fn admit(&mut self, now: Instant) -> Option<Arc<ReplyTicket>> {
        while self
            .recent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= REPLY_CAP_WINDOW)
        {
            self.recent.pop_front();
        }
        let since = |t: Option<Instant>| t.map(|t| now.saturating_duration_since(t));
        let written = *self.ticket.written_at.lock().unwrap_or_else(|p| p.into_inner());
        if self.recent.len() >= MAX_REPLIES_PER_WINDOW
            || self.ticket.outstanding.load(Ordering::Acquire)
            || since(self.last_admitted).is_some_and(|d| d < REPLY_DEBOUNCE)
            || since(written).is_some_and(|d| d < REPLY_ECHO_WINDOW)
        {
            return None;
        }
        self.ticket.outstanding.store(true, Ordering::Release);
        self.last_admitted = Some(now);
        self.recent.push_back(now);
        Some(Arc::clone(&self.ticket))
    }
}

struct Slot {
    id: Vec<String>,
    record: ProgramRecord,
    /// Monotonic update stamp; the smallest is the least recently updated.
    touched: u64,
}

/// Every program-status record of one terminal session, with the protocol's
/// lifetime rules. Small by construction (≤ [`MAX_RECORDS`]), so a flat list
/// beats a tree: every operation is a short linear scan.
#[derive(Default)]
pub struct ProgramStatusStore {
    slots: Vec<Slot>,
    clock: u64,
}

impl ProgramStatusStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The root record (a report without `id`), if any.
    pub fn root(&self) -> Option<&ProgramRecord> {
        self.get(&[])
    }

    pub fn get(&self, id: &[String]) -> Option<&ProgramRecord> {
        self.slots.iter().find(|s| s.id == id).map(|s| &s.record)
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The `app` that applies to `id`: its own, else its nearest ancestor's.
    pub fn app_of(&self, id: &[String]) -> Option<&str> {
        (0..=id.len())
            .rev()
            .find_map(|depth| self.get(&id[..depth]).and_then(|r| r.app.as_deref()))
    }

    /// Apply one report. Returns `true` when the root record changed.
    pub fn apply(&mut self, report: ProgramReport) -> bool {
        self.root_changed_by(|store| match report {
            ProgramReport::Set { id, record } => store.set(id, record),
            ProgramReport::Clear { id: None } => store.slots.clear(),
            ProgramReport::Clear { id: Some(id) } => {
                store.slots.retain(|s| !s.id.starts_with(&id));
            }
        })
    }

    /// The attached process exited, or a new shell prompt began (OSC 133 A):
    /// what it was doing is over, but what it finished with stays visible.
    /// Returns `true` when the root record changed.
    pub fn end_activity(&mut self) -> bool {
        self.root_changed_by(|store| {
            store
                .slots
                .retain(|s| matches!(s.record.state, ProgramState::Done | ProgramState::Error));
        })
    }

    /// Full reset (RIS): every record goes. Returns `true` when the root changed.
    pub fn reset(&mut self) -> bool {
        self.root_changed_by(|store| store.slots.clear())
    }

    fn set(&mut self, id: Vec<String>, record: ProgramRecord) {
        self.clock += 1;
        let touched = self.clock;
        if let Some(slot) = self.slots.iter_mut().find(|s| s.id == id) {
            slot.record = record;
            slot.touched = touched;
            return;
        }
        if self.slots.len() >= MAX_RECORDS
            && let Some(oldest) = self
                .slots
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.touched)
                .map(|(i, _)| i)
        {
            self.slots.swap_remove(oldest);
        }
        self.slots.push(Slot { id, record, touched });
    }

    fn root_changed_by(&mut self, f: impl FnOnce(&mut Self)) -> bool {
        let before = self.root().cloned();
        f(self);
        self.root() != before.as_ref()
    }
}

pub(crate) mod tracker;

#[cfg(test)]
#[path = "program_status_tests.rs"]
mod tests;
