//! OSC-9999 structured-status sideband scanner.
//!
//! Agent CLIs (or a thin hook) can emit a private OSC sequence carrying a
//! JSON status payload:
//!
//! ```text
//! ESC ] 9999 ; {"v":1,"state":"working","tool":"Edit","tool_input":"src/x.rs"} BEL
//! ```
//!
//! `AgentOscScanner` is a byte-level state machine that runs inside the
//! per-session poll loop, alongside the existing regex `StatusMachine`. It
//! does two jobs in one pass over each PTY `Output` chunk:
//!
//!  1. **Extract** the OSC-9999 JSON payload into a `SidebandEvent` so the
//!     status machine can be `force()`d to the reported state — this is what
//!     closes the `EMPTY_PATTERNS` gap for adapters (Codex/Aider) whose raw
//!     output the regex table can't classify.
//!  2. **Strip** the OSC-9999 bytes out of a `cleaned` copy of the chunk so
//!     the regex machine never sees them. This matters because the regex
//!     machine's fallback rule ("any output while not Running → Running")
//!     would otherwise fire on a chunk that is *purely* a sideband sequence
//!     and immediately clobber the status the sideband just set. Only the
//!     9999 sequences are removed; every other byte (text, colors, other
//!     OSCs) passes through untouched.
//!
//! The scanner is self-contained — no `TerminalEvent` variant, no backend
//! routing change. It is stateful so a sequence split across two PTY reads
//! (chunk boundary mid-payload) still parses on the second `feed()`.

use std::borrow::Cow;

use oximux_core::{AgentSidebandState, AgentStatus, SidebandDetail};

/// Hard cap on an accumulated OSC-9999 payload. The Phase-1 schema tops out
/// near ~840 bytes (512 msg + 256 input + 64 tool + framing); 4 KiB leaves
/// generous headroom while bounding the cost of an adversarial unterminated
/// payload. On overflow the scanner keeps consuming (to stay in sync for the
/// terminator) but stops accumulating and discards the truncated payload.
const MAX_OSC_PAYLOAD: usize = 4096;

/// The numeric OSC identifier OxideADE reserves for the status sideband.
const OSC_NUM: &[u8] = b"9999";

// Field caps — applied at parse time so oversized payloads can't bloat the
// UI. Mirrors the wire-format contract in the phase plan.
const MAX_TOOL: usize = 64;
const MAX_TOOL_INPUT: usize = 256;
const MAX_MSG: usize = 512;
const MAX_SESSION_ID: usize = 64;
const MAX_PROMPT: usize = 256;

const BEL: u8 = 0x07;
const ESC: u8 = 0x1B;

/// One decoded sideband event. `detail` is always present (its fields may
/// all be `None`); `state` is the mapped lifecycle word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebandEvent {
    pub state: AgentSidebandState,
    pub detail: SidebandDetail,
}

/// Result of feeding one chunk to the scanner: the chunk with all OSC-9999
/// sequences removed (safe to hand the regex machine) plus the latest
/// sideband event decoded in this chunk, if any.
///
/// When a chunk has no escape bytes at all and the scanner is idle, `cleaned`
/// borrows the input — the common plain-text path allocates nothing.
pub struct ScanOutput<'a> {
    pub cleaned: Cow<'a, [u8]>,
    /// Last sideband event completed in this chunk. Multiple sideband
    /// sequences in one chunk collapse to the latest, matching the
    /// last-writer-wins semantics of the status watch channel.
    pub event: Option<SidebandEvent>,
}

/// Byte-level parse state. `Esc`/`OscNum` buffer the `ESC ] <num> ;` prefix
/// in `pending` until we know whether the sequence is ours (9999, suppress)
/// or someone else's (flush to `cleaned`, pass through).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Esc,
    OscNum,
    /// Accumulating our 9999 payload (bytes suppressed from `cleaned`).
    Payload,
    /// Saw ESC inside our payload — may be the `ESC \` ST terminator.
    PayloadEsc,
    /// Inside a non-9999 OSC — bytes pass through to `cleaned`, we only
    /// track the terminator so we know when to return to `Normal`.
    Ignore,
    /// Saw ESC inside a passthrough OSC — may be the ST terminator.
    IgnoreEsc,
}

/// Stateful scanner; one per live session, owned by the poll loop. Cheap to
/// construct and reset.
pub struct AgentOscScanner {
    state: State,
    /// Buffered `ESC ] <digits> ;` prefix, replayed to `cleaned` if the OSC
    /// turns out not to be ours. Bounded by `OSC_NUM` length checks.
    pending: Vec<u8>,
    /// Accumulated 9999 payload bytes (JSON), capped at `MAX_OSC_PAYLOAD`.
    payload: Vec<u8>,
    /// Set once the payload exceeds the cap; the completed payload is then
    /// discarded rather than parsed.
    truncated: bool,
}

impl Default for AgentOscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentOscScanner {
    pub fn new() -> Self {
        Self {
            state: State::Normal,
            pending: Vec::new(),
            payload: Vec::new(),
            truncated: false,
        }
    }

    /// True while a sideband sequence is mid-parse (a payload split across two
    /// PTY reads). A caller that gates `feed` on a fresh marker must still feed
    /// the next chunk while this holds, or it would drop the sequence's tail.
    pub fn is_active(&self) -> bool {
        self.state != State::Normal
    }

    /// Feed one PTY output chunk. Returns the OSC-9999-stripped bytes plus
    /// the latest decoded sideband event (if a sequence completed here).
    pub fn feed<'a>(&mut self, bytes: &'a [u8]) -> ScanOutput<'a> {
        // Fast path: idle scanner + no escape bytes ⇒ nothing to strip, no
        // state to advance. Borrow the input, allocate nothing.
        if self.state == State::Normal && !bytes.contains(&ESC) {
            return ScanOutput {
                cleaned: Cow::Borrowed(bytes),
                event: None,
            };
        }

        let mut cleaned: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut event: Option<SidebandEvent> = None;
        for &b in bytes {
            self.step(b, &mut cleaned, &mut event);
        }
        ScanOutput {
            cleaned: Cow::Owned(cleaned),
            event,
        }
    }

    /// Process one byte, appending pass-through bytes to `cleaned` and
    /// overwriting `event` when a 9999 sequence completes.
    fn step(&mut self, b: u8, cleaned: &mut Vec<u8>, event: &mut Option<SidebandEvent>) {
        match self.state {
            State::Normal => {
                if b == ESC {
                    self.pending.clear();
                    self.pending.push(ESC);
                    self.state = State::Esc;
                } else {
                    cleaned.push(b);
                }
            }
            State::Esc => {
                if b == b']' {
                    self.pending.push(b);
                    self.state = State::OscNum;
                } else {
                    // Not an OSC introducer — flush the buffered ESC and
                    // re-handle this byte from Normal (it may start a fresh
                    // escape).
                    cleaned.extend_from_slice(&self.pending);
                    self.pending.clear();
                    self.state = State::Normal;
                    self.step(b, cleaned, event);
                }
            }
            State::OscNum => match b {
                b'0'..=b'9' => {
                    self.pending.push(b);
                    // Guard against an unbounded digit run that can never be
                    // our 4-digit id.
                    if self.pending.len() > 2 + OSC_NUM.len() {
                        cleaned.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = State::Ignore;
                    }
                }
                b';' => {
                    let is_ours = &self.pending[2..] == OSC_NUM;
                    if is_ours {
                        // Suppress the whole prefix; start collecting payload.
                        self.pending.clear();
                        self.payload.clear();
                        self.truncated = false;
                        self.state = State::Payload;
                    } else {
                        // Someone else's OSC — replay prefix + this `;` and
                        // pass the rest through.
                        cleaned.extend_from_slice(&self.pending);
                        cleaned.push(b);
                        self.pending.clear();
                        self.state = State::Ignore;
                    }
                }
                _ => {
                    // Malformed OSC number — flush and re-handle from Normal.
                    cleaned.extend_from_slice(&self.pending);
                    self.pending.clear();
                    self.state = State::Normal;
                    self.step(b, cleaned, event);
                }
            },
            State::Payload => match b {
                BEL => {
                    self.finish_payload(event);
                    self.state = State::Normal;
                }
                ESC => self.state = State::PayloadEsc,
                _ => {
                    if !self.truncated {
                        if self.payload.len() < MAX_OSC_PAYLOAD {
                            self.payload.push(b);
                        } else {
                            self.truncated = true;
                        }
                    }
                }
            },
            State::PayloadEsc => {
                if b == b'\\' {
                    // ESC \ = ST terminator.
                    self.finish_payload(event);
                    self.state = State::Normal;
                } else {
                    // A bare ESC mid-payload ends the OSC abnormally. Emit
                    // what we have, then re-handle this byte from Normal.
                    self.finish_payload(event);
                    self.state = State::Normal;
                    self.step(b, cleaned, event);
                }
            }
            State::Ignore => match b {
                BEL => {
                    cleaned.push(b);
                    self.state = State::Normal;
                }
                ESC => self.state = State::IgnoreEsc,
                _ => cleaned.push(b),
            },
            State::IgnoreEsc => {
                if b == b'\\' {
                    cleaned.push(ESC);
                    cleaned.push(b);
                    self.state = State::Normal;
                } else {
                    // Abnormal end of the passthrough OSC. Replay the ESC,
                    // then re-handle this byte from Normal.
                    cleaned.push(ESC);
                    self.state = State::Normal;
                    self.step(b, cleaned, event);
                }
            }
        }
    }

    /// Parse the accumulated payload (unless truncated) into a `SidebandEvent`
    /// and store it as the chunk's latest event. Always resets payload state.
    fn finish_payload(&mut self, event: &mut Option<SidebandEvent>) {
        if !self.truncated
            && let Some(ev) = parse_sideband_json(&self.payload)
        {
            *event = Some(ev);
        }
        self.payload.clear();
        self.truncated = false;
    }
}

/// Wire shape of the OSC-9999 JSON. Deserialized leniently — unknown fields
/// are ignored so the schema can grow additive fields without breaking old
/// scanners.
#[derive(serde::Deserialize)]
struct WireSideband {
    v: u8,
    state: String,
    tool: Option<String>,
    tool_input: Option<String>,
    msg: Option<String>,
    session_id: Option<String>,
    /// The user's prompt, carried by the prompt-submit hook. Additive: an
    /// older payload without it deserializes fine (the field defaults `None`).
    prompt: Option<String>,
}

/// Parse one OSC-9999 payload. Returns `None` on malformed JSON, an
/// unsupported schema version (`v > 1`), or an unknown `state` word —
/// never panics on hostile input.
fn parse_sideband_json(bytes: &[u8]) -> Option<SidebandEvent> {
    let wire: WireSideband = serde_json::from_slice(bytes).ok()?;
    if wire.v > 1 {
        return None;
    }
    let state = parse_state(&wire.state)?;
    let detail = SidebandDetail {
        tool_name: wire.tool.map(|s| cap_bytes(s, MAX_TOOL)),
        tool_input_summary: wire.tool_input.map(|s| cap_bytes(s, MAX_TOOL_INPUT)),
        last_message: wire.msg.map(|s| cap_bytes(s, MAX_MSG)),
        session_id: wire.session_id.map(|s| cap_bytes(s, MAX_SESSION_ID)),
        prompt: wire.prompt.map(|s| cap_bytes(s, MAX_PROMPT)),
    };
    Some(SidebandEvent { state, detail })
}

/// Map the wire `state` word to the typed enum. Unknown → `None` (reject).
fn parse_state(s: &str) -> Option<AgentSidebandState> {
    Some(match s {
        "working" => AgentSidebandState::Working,
        "idle" => AgentSidebandState::Idle,
        "waiting" => AgentSidebandState::Waiting,
        "needs_approval" => AgentSidebandState::NeedsApproval,
        "done" => AgentSidebandState::Done,
        _ => return None,
    })
}

/// Truncate a string to at most `max` bytes without splitting a UTF-8 char.
fn cap_bytes(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s
}

/// Map a sideband state to the runtime `AgentStatus`. `tool` supplies the
/// `NeedsApproval` reason payload (empty string when absent).
pub fn map_state_to_status(state: AgentSidebandState, tool: Option<String>) -> AgentStatus {
    match state {
        AgentSidebandState::Working => AgentStatus::Running,
        AgentSidebandState::Idle => AgentStatus::Idle,
        AgentSidebandState::Waiting => AgentStatus::WaitingForInput,
        AgentSidebandState::NeedsApproval => AgentStatus::NeedsApproval(tool.unwrap_or_default()),
        AgentSidebandState::Done => AgentStatus::Done { code: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(payload: &str) -> Vec<u8> {
        let mut v = vec![ESC, b']'];
        v.extend_from_slice(OSC_NUM);
        v.push(b';');
        v.extend_from_slice(payload.as_bytes());
        v.push(BEL);
        v
    }

    #[test]
    fn bel_terminated_payload_decodes() {
        let mut sc = AgentOscScanner::new();
        let bytes = osc(r#"{"v":1,"state":"needs_approval","tool":"Bash"}"#);
        let out = sc.feed(&bytes);
        let ev = out.event.expect("event");
        assert_eq!(ev.state, AgentSidebandState::NeedsApproval);
        assert_eq!(ev.detail.tool_name.as_deref(), Some("Bash"));
        // The whole OSC-9999 sequence is stripped from cleaned output.
        assert!(out.cleaned.is_empty(), "cleaned: {:?}", out.cleaned);
    }

    #[test]
    fn st_terminated_payload_decodes() {
        let mut sc = AgentOscScanner::new();
        let mut bytes = vec![ESC, b']'];
        bytes.extend_from_slice(OSC_NUM);
        bytes.push(b';');
        bytes.extend_from_slice(br#"{"v":1,"state":"working","tool":"Edit"}"#);
        bytes.push(ESC);
        bytes.push(b'\\'); // ST
        let out = sc.feed(&bytes);
        let ev = out.event.expect("event");
        assert_eq!(ev.state, AgentSidebandState::Working);
        assert_eq!(ev.detail.tool_name.as_deref(), Some("Edit"));
        assert!(out.cleaned.is_empty());
    }

    #[test]
    fn chunked_payload_across_two_feeds() {
        let mut sc = AgentOscScanner::new();
        let full = osc(r#"{"v":1,"state":"working","tool":"Read"}"#);
        let (a, b) = full.split_at(12);
        let out_a = sc.feed(a);
        assert!(out_a.event.is_none(), "incomplete payload yields no event");
        let out_b = sc.feed(b);
        let ev = out_b.event.expect("event after second chunk");
        assert_eq!(ev.state, AgentSidebandState::Working);
        assert_eq!(ev.detail.tool_name.as_deref(), Some("Read"));
    }

    #[test]
    fn surrounding_text_passes_through_cleaned() {
        let mut sc = AgentOscScanner::new();
        let mut bytes = b"before".to_vec();
        bytes.extend_from_slice(&osc(r#"{"v":1,"state":"idle"}"#));
        bytes.extend_from_slice(b"after");
        let out = sc.feed(&bytes);
        assert_eq!(&*out.cleaned, b"beforeafter");
        assert_eq!(out.event.expect("event").state, AgentSidebandState::Idle);
    }

    #[test]
    fn plain_text_fast_path_borrows() {
        let mut sc = AgentOscScanner::new();
        let out = sc.feed(b"just normal output\n");
        assert!(matches!(out.cleaned, Cow::Borrowed(_)));
        assert!(out.event.is_none());
    }

    #[test]
    fn non_9999_osc_passes_through_untouched() {
        let mut sc = AgentOscScanner::new();
        // OSC-7 cwd report must survive in cleaned output verbatim.
        let mut bytes = vec![ESC, b']'];
        bytes.extend_from_slice(b"7;file:///tmp");
        bytes.push(BEL);
        let original = bytes.clone();
        let out = sc.feed(&bytes);
        assert_eq!(&*out.cleaned, &original[..]);
        assert!(out.event.is_none());
    }

    #[test]
    fn oversized_payload_does_not_corrupt_next_parse() {
        let mut sc = AgentOscScanner::new();
        let big = "x".repeat(MAX_OSC_PAYLOAD + 100);
        let huge = osc(&format!(r#"{{"v":1,"state":"working","msg":"{big}"}}"#));
        let out = sc.feed(&huge);
        assert!(out.event.is_none(), "truncated payload must not parse");
        // A subsequent valid sequence is still detected — parse sync held.
        let next = osc(r#"{"v":1,"state":"done"}"#);
        let out2 = sc.feed(&next);
        assert_eq!(out2.event.expect("event").state, AgentSidebandState::Done);
    }

    #[test]
    fn malformed_json_returns_none() {
        let mut sc = AgentOscScanner::new();
        let bytes = osc("{not json");
        let out = sc.feed(&bytes);
        assert!(out.event.is_none());
    }

    #[test]
    fn unknown_version_rejected() {
        let mut sc = AgentOscScanner::new();
        let bytes = osc(r#"{"v":2,"state":"working"}"#);
        let out = sc.feed(&bytes);
        assert!(out.event.is_none());
    }

    #[test]
    fn unknown_state_rejected() {
        let mut sc = AgentOscScanner::new();
        let bytes = osc(r#"{"v":1,"state":"frobnicating"}"#);
        let out = sc.feed(&bytes);
        assert!(out.event.is_none());
    }

    #[test]
    fn two_sequences_in_one_chunk_yield_latest() {
        let mut sc = AgentOscScanner::new();
        let mut bytes = osc(r#"{"v":1,"state":"working"}"#);
        bytes.extend_from_slice(&osc(r#"{"v":1,"state":"needs_approval","tool":"Write"}"#));
        let out = sc.feed(&bytes);
        let ev = out.event.expect("event");
        assert_eq!(ev.state, AgentSidebandState::NeedsApproval);
        assert_eq!(ev.detail.tool_name.as_deref(), Some("Write"));
    }

    #[test]
    fn tool_input_and_msg_capped() {
        let mut sc = AgentOscScanner::new();
        let long_input = "a".repeat(MAX_TOOL_INPUT + 50);
        let long_msg = "b".repeat(MAX_MSG + 50);
        let long_sid = "c".repeat(MAX_SESSION_ID + 50);
        let long_prompt = "d".repeat(MAX_PROMPT + 50);
        let bytes = osc(&format!(
            r#"{{"v":1,"state":"working","tool_input":"{long_input}","msg":"{long_msg}","session_id":"{long_sid}","prompt":"{long_prompt}"}}"#
        ));
        let out = sc.feed(&bytes);
        let d = out.event.expect("event").detail;
        assert_eq!(d.tool_input_summary.unwrap().len(), MAX_TOOL_INPUT);
        assert_eq!(d.last_message.unwrap().len(), MAX_MSG);
        assert_eq!(d.session_id.unwrap().len(), MAX_SESSION_ID);
        assert_eq!(d.prompt.unwrap().len(), MAX_PROMPT);
    }

    #[test]
    fn prompt_field_decodes() {
        let mut sc = AgentOscScanner::new();
        let bytes = osc(r#"{"v":1,"state":"working","prompt":"fix the parser bug"}"#);
        let ev = sc.feed(&bytes).event.expect("event");
        assert_eq!(ev.detail.prompt.as_deref(), Some("fix the parser bug"));
    }

    #[test]
    fn map_state_needs_approval_carries_tool() {
        let s = map_state_to_status(AgentSidebandState::NeedsApproval, Some("Edit".into()));
        assert_eq!(s, AgentStatus::NeedsApproval("Edit".into()));
        let s2 = map_state_to_status(AgentSidebandState::Done, None);
        assert_eq!(s2, AgentStatus::Done { code: None });
    }

    #[test]
    fn cap_bytes_respects_utf8_boundary() {
        // 'é' is 2 bytes; capping at 3 must not split the second 'é'.
        let s = "aéé".to_string(); // bytes: a(1) é(2) é(2) = 5
        let capped = cap_bytes(s, 3);
        assert_eq!(capped, "aé");
    }

    #[test]
    fn bare_esc_in_payload_then_text_recovers() {
        let mut sc = AgentOscScanner::new();
        // ESC ] 9999 ; {partial ESC (not ST) then a fresh valid sequence.
        let mut bytes = vec![ESC, b']'];
        bytes.extend_from_slice(OSC_NUM);
        bytes.push(b';');
        bytes.extend_from_slice(b"{partial");
        bytes.push(ESC); // bare ESC, not followed by '\\'
        bytes.push(b'X'); // abnormal end, reprocessed from Normal
        // Then a clean sequence the scanner must still find.
        bytes.extend_from_slice(&osc(r#"{"v":1,"state":"idle"}"#));
        let out = sc.feed(&bytes);
        assert_eq!(out.event.expect("event").state, AgentSidebandState::Idle);
        // The stray 'X' survives in cleaned output.
        assert!(out.cleaned.contains(&b'X'));
    }
}
