//! Regex-driven status machine.
//!
//! Owns a small ring buffer of the last 1024 output bytes per session and
//! scans it against the adapter's `StatusPattern` table on every chunk.
//! First match wins (pattern order in the adapter slice is significant).
//!
//! Fallback rules when no pattern matches:
//! - any output while `Idle`         → `Running`
//! - no output for `IDLE_AFTER` while `Running` → `Idle`
//! - `WaitingForInput` / `NeedsApproval` do **not** decay to `Idle`
//!   (the user is being asked something; slow user is not idle agent).
//! - terminal states (`Done` / `Failed`) never transition out.
//!
//! Pure logic — no I/O, no tokio, no PTY. `feed()` takes the wall-clock
//! instant from the caller so tests can drive synthetic time.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oximux_core::AgentStatus;

use crate::cli::adapter::StatusPattern;

/// How many bytes of recent output the regex engine sees per scan.
/// Matches the Phase 3 spec; small enough to keep regex cost well under
/// the 1 ms-per-chunk budget.
pub const SCAN_WINDOW_BYTES: usize = 1024;

/// `Running` decays to `Idle` after this much silence.
pub const IDLE_AFTER: Duration = Duration::from_secs(5);

/// One transition emitted by `feed()` / `tick()` / `note_exit()`.
/// `from == to` is never emitted — callers can compare directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusTransition {
    pub from: AgentStatus,
    pub to: AgentStatus,
}

/// Per-session status machine. Cheap to construct (just an `Arc` clone of
/// the adapter's pattern slice + a 1 KiB ring); one per live session.
pub struct StatusMachine {
    patterns: Arc<[StatusPattern]>,
    ring: VecDeque<u8>,
    current: AgentStatus,
    last_output_at: Option<Instant>,
}

impl StatusMachine {
    /// Build a fresh machine. Initial status is `Idle`.
    pub fn new(patterns: Arc<[StatusPattern]>) -> Self {
        Self {
            patterns,
            ring: VecDeque::with_capacity(SCAN_WINDOW_BYTES),
            current: AgentStatus::Idle,
            last_output_at: None,
        }
    }

    /// Current status (last successful transition's target, or `Idle`).
    pub fn current(&self) -> &AgentStatus {
        &self.current
    }

    /// Feed a chunk of PTY output. Returns a transition if the status
    /// changed, else `None`. `now` is the caller's clock — typically
    /// `Instant::now()`, but tests pass synthetic instants.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Option<StatusTransition> {
        if bytes.is_empty() || self.current.is_terminal() {
            return None;
        }
        self.push_to_ring(bytes);
        self.last_output_at = Some(now);

        // 1. Adapter pattern table — first match wins.
        let haystack = self.ring.make_contiguous();
        for pat in self.patterns.iter() {
            if pat.regex.is_match(haystack) {
                return self.transition_to(pat.transition.clone());
            }
        }

        // 2. Fallback: output without any matching pattern.
        //    Fires from Idle (burst → Running) AND from blocking states
        //    (the prompt scrolled out of the ring; user must have replied).
        //    Stays put when already Running (most common case, no work).
        if !matches!(self.current, AgentStatus::Running) {
            return self.transition_to(AgentStatus::Running);
        }
        None
    }

    /// Wall-clock tick — typically driven by the same 500 ms tokio timer
    /// the UI repaint uses. Decays `Running` → `Idle` after `IDLE_AFTER`.
    ///
    /// Note: `feed_sideband` does not touch `last_output_at` (it carries no
    /// bytes), so a session driven purely by OSC-9999 packets never decays
    /// here — it stays in the agent-reported state until the next sideband
    /// packet or the exit event. That is intentional: when the agent says it
    /// is running we trust it over the output-absence heuristic.
    pub fn tick(&mut self, now: Instant) -> Option<StatusTransition> {
        if !matches!(self.current, AgentStatus::Running) {
            return None;
        }
        let last = self.last_output_at?;
        if now.saturating_duration_since(last) >= IDLE_AFTER {
            self.transition_to(AgentStatus::Idle)
        } else {
            None
        }
    }

    /// Record process exit. Maps `code` to `Done` (0 or signal-killed) or
    /// `Failed` (non-zero). Idempotent: returns `None` if already terminal.
    pub fn note_exit(&mut self, code: Option<i32>) -> Option<StatusTransition> {
        if self.current.is_terminal() {
            return None;
        }
        let to = match code {
            Some(0) => AgentStatus::Done { code: Some(0) },
            Some(c) => AgentStatus::Failed(format!("exit {c}")),
            None => AgentStatus::Done { code: None },
        };
        self.transition_to(to)
    }

    /// Record a user-initiated cancel. The PTY exit that follows a cancel
    /// is a kill signal, which `note_exit` would misreport as `Done` /
    /// `Failed` — this maps it to `Interrupted` instead so the UI can show
    /// "Stopped" rather than a false failure. Idempotent like `note_exit`:
    /// returns `None` if already terminal.
    pub fn note_interrupted(&mut self) -> Option<StatusTransition> {
        if self.current.is_terminal() {
            return None;
        }
        self.transition_to(AgentStatus::Interrupted)
    }

    /// Force a status — used by the manual badge-click override when the
    /// regex table misclassifies. Refuses to leave a terminal state (a
    /// `Done`/`Failed` session cannot be resurrected by clicking; cancel
    /// + relaunch is the right path). Refuses identity transitions.
    pub fn force(&mut self, status: AgentStatus) -> Option<StatusTransition> {
        if self.current.is_terminal() {
            return None;
        }
        self.transition_to(status)
    }

    /// Apply an OSC-9999 sideband event. Maps the reported `state` (+ `tool`
    /// for the `NeedsApproval` reason) to an `AgentStatus` and drives it
    /// through `force()`, so the sideband path inherits every invariant the
    /// manual override has — terminal-state guard, identity no-op, and the
    /// blocking-entry ring wipe. Unlike the regex path it needs no matching
    /// pattern, which is exactly what closes the `EMPTY_PATTERNS` gap for
    /// adapters whose raw output the regex table can't classify.
    pub fn feed_sideband(
        &mut self,
        state: oximux_core::AgentSidebandState,
        tool: Option<String>,
    ) -> Option<StatusTransition> {
        let derived = crate::osc_sideband::map_state_to_status(state, tool);
        self.force(derived)
    }

    fn push_to_ring(&mut self, bytes: &[u8]) {
        // Only the tail of an oversized chunk matters — drop the head before
        // pushing so we never grow past `SCAN_WINDOW_BYTES` then trim back.
        let tail = if bytes.len() > SCAN_WINDOW_BYTES {
            &bytes[bytes.len() - SCAN_WINDOW_BYTES..]
        } else {
            bytes
        };
        let total = self.ring.len() + tail.len();
        if total > SCAN_WINDOW_BYTES {
            let drop = total - SCAN_WINDOW_BYTES;
            self.ring.drain(..drop);
        }
        self.ring.extend(tail);
    }

    fn transition_to(&mut self, to: AgentStatus) -> Option<StatusTransition> {
        if self.current == to {
            return None;
        }
        let entering_block = to.is_blocking();
        let from = std::mem::replace(&mut self.current, to.clone());
        if entering_block {
            // H2: the bytes that just matched a blocking prompt remain in
            // the scan window. Without clearing, every subsequent feed()
            // re-matches the stale prompt and the machine stays pinned in
            // NeedsApproval / WaitingForInput forever. Wipe the ring on
            // entry so the next chunk starts from a fresh window — if a
            // re-prompt comes through the new chunk re-matches naturally.
            self.ring.clear();
        }
        Some(StatusTransition { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::bytes::Regex;

    fn patterns(pairs: &[(&str, AgentStatus)]) -> Arc<[StatusPattern]> {
        pairs
            .iter()
            .map(|(p, s)| StatusPattern {
                regex: Regex::new(p).unwrap(),
                transition: s.clone(),
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn idle_then_output_burst_transitions_to_running() {
        let mut sm = StatusMachine::new(patterns(&[]));
        let t = sm.feed(b"hello", t0()).unwrap();
        assert_eq!(t.from, AgentStatus::Idle);
        assert_eq!(t.to, AgentStatus::Running);
        assert_eq!(sm.current(), &AgentStatus::Running);
    }

    #[test]
    fn empty_feed_is_noop() {
        let mut sm = StatusMachine::new(patterns(&[]));
        assert!(sm.feed(&[], t0()).is_none());
        assert_eq!(sm.current(), &AgentStatus::Idle);
    }

    #[test]
    fn pattern_match_overrides_running_fallback() {
        let pats = patterns(&[(
            r"Approval needed:",
            AgentStatus::NeedsApproval(String::new()),
        )]);
        let mut sm = StatusMachine::new(pats);
        let t = sm.feed(b"... Approval needed: do thing?", t0()).unwrap();
        assert!(matches!(t.to, AgentStatus::NeedsApproval(_)));
    }

    #[test]
    fn first_match_wins() {
        let pats = patterns(&[
            (r"\? ", AgentStatus::WaitingForInput),
            (r"\? ", AgentStatus::NeedsApproval("never".into())),
        ]);
        let mut sm = StatusMachine::new(pats);
        let t = sm.feed(b"continue? ", t0()).unwrap();
        assert_eq!(t.to, AgentStatus::WaitingForInput);
    }

    #[test]
    fn no_change_when_pattern_resolves_same_status() {
        let pats = patterns(&[(r">", AgentStatus::WaitingForInput)]);
        let mut sm = StatusMachine::new(pats);
        assert_eq!(
            sm.feed(b"> ", t0()).unwrap().to,
            AgentStatus::WaitingForInput
        );
        // Second chunk still has `>` — current is already WaitingForInput, no transition.
        assert!(sm.feed(b"> ", t0()).is_none());
    }

    #[test]
    fn running_decays_to_idle_after_idle_after() {
        let mut sm = StatusMachine::new(patterns(&[]));
        let start = t0();
        sm.feed(b"x", start);
        assert_eq!(sm.current(), &AgentStatus::Running);
        let t = sm.tick(start + IDLE_AFTER).unwrap();
        assert_eq!(t.to, AgentStatus::Idle);
    }

    #[test]
    fn waiting_for_input_does_not_decay() {
        let pats = patterns(&[(r"> $", AgentStatus::WaitingForInput)]);
        let mut sm = StatusMachine::new(pats);
        let start = t0();
        sm.feed(b"prompt: > ", start);
        assert_eq!(sm.current(), &AgentStatus::WaitingForInput);
        assert!(sm.tick(start + IDLE_AFTER * 10).is_none());
        assert_eq!(sm.current(), &AgentStatus::WaitingForInput);
    }

    #[test]
    fn terminal_state_never_transitions() {
        let mut sm = StatusMachine::new(patterns(&[(r".", AgentStatus::Running)]));
        sm.note_exit(Some(0));
        assert!(matches!(sm.current(), AgentStatus::Done { code: Some(0) }));
        // Output after exit is ignored.
        assert!(sm.feed(b"late output", t0()).is_none());
        assert!(matches!(sm.current(), AgentStatus::Done { .. }));
    }

    #[test]
    fn note_exit_nonzero_is_failed() {
        let mut sm = StatusMachine::new(patterns(&[]));
        let t = sm.note_exit(Some(127)).unwrap();
        assert!(matches!(t.to, AgentStatus::Failed(_)));
    }

    #[test]
    fn note_exit_signal_is_done_without_code() {
        let mut sm = StatusMachine::new(patterns(&[]));
        let t = sm.note_exit(None).unwrap();
        assert!(matches!(t.to, AgentStatus::Done { code: None }));
    }

    #[test]
    fn note_exit_idempotent() {
        let mut sm = StatusMachine::new(patterns(&[]));
        assert!(sm.note_exit(Some(0)).is_some());
        assert!(sm.note_exit(Some(1)).is_none());
    }

    #[test]
    fn ring_trims_to_scan_window() {
        // Unanchored pattern so we measure actual eviction, not the `^`
        // anchor preventing a match. `START` at offset 0 of an oversized
        // chunk must be dropped by the tail-only push.
        let pats = patterns(&[(r"START", AgentStatus::NeedsApproval("x".into()))]);
        let mut sm = StatusMachine::new(pats);
        let mut payload = b"START".to_vec();
        payload.extend(std::iter::repeat_n(b'-', SCAN_WINDOW_BYTES + 100));
        sm.feed(&payload, t0());
        // `START` was evicted: ring is 1024 trailing `-`. Pattern misses,
        // fallback runs Idle → Running.
        assert_eq!(sm.current(), &AgentStatus::Running);

        // Control: same pattern on a small payload DOES match — proves
        // the previous miss was eviction, not pattern misconfiguration.
        let pats2 = patterns(&[(r"START", AgentStatus::NeedsApproval("y".into()))]);
        let mut sm2 = StatusMachine::new(pats2);
        sm2.feed(b"START -- here", t0());
        assert!(matches!(sm2.current(), AgentStatus::NeedsApproval(_)));
    }

    #[test]
    fn blocking_unsticks_when_user_replies() {
        // H2: regression test. Without the ring-clear-on-blocking-entry
        // fix, the matched prompt bytes stay in the scan window and the
        // machine pins to NeedsApproval indefinitely.
        let pats = patterns(&[(r"Approval needed", AgentStatus::NeedsApproval("x".into()))]);
        let mut sm = StatusMachine::new(pats);
        sm.feed(b"Approval needed: ok?", t0());
        assert!(matches!(sm.current(), AgentStatus::NeedsApproval(_)));

        // User types "y\n", agent prints non-prompt output. Pattern must
        // miss (ring was cleared) and fallback must transition to Running.
        let t = sm.feed(b"running step 2...", t0()).unwrap();
        assert_eq!(t.to, AgentStatus::Running);
    }

    #[test]
    fn note_interrupted_maps_to_interrupted() {
        let mut sm = StatusMachine::new(patterns(&[]));
        sm.feed(b"working...", t0());
        let t = sm.note_interrupted().unwrap();
        assert_eq!(t.to, AgentStatus::Interrupted);
        assert!(sm.current().is_terminal());
    }

    #[test]
    fn note_interrupted_idempotent_after_terminal() {
        let mut sm = StatusMachine::new(patterns(&[]));
        sm.note_exit(Some(0));
        assert!(sm.note_interrupted().is_none());
        assert!(matches!(sm.current(), AgentStatus::Done { code: Some(0) }));
    }

    #[test]
    fn force_rejects_terminal_state() {
        // L1: clicking the badge on a Done session must not resurrect it;
        // the cancel + relaunch path is the only way back to a live state.
        let mut sm = StatusMachine::new(patterns(&[]));
        sm.note_exit(Some(0));
        assert!(sm.force(AgentStatus::Running).is_none());
        assert!(matches!(sm.current(), AgentStatus::Done { code: Some(0) }));
    }

    #[test]
    fn bytes_regex_handles_non_utf8_input() {
        // 0x80-0xFF is invalid as a UTF-8 leading byte; bytes::Regex must
        // not panic when the haystack contains them.
        let pats = patterns(&[(r"OK", AgentStatus::Running)]);
        let mut sm = StatusMachine::new(pats);
        let chunk = [0x80, 0xC3, 0x28, b'O', b'K', 0xFF];
        let t = sm.feed(&chunk, t0()).unwrap();
        assert_eq!(t.to, AgentStatus::Running);
    }

    #[test]
    fn force_emits_transition() {
        let mut sm = StatusMachine::new(patterns(&[]));
        let t = sm
            .force(AgentStatus::NeedsApproval("manual".into()))
            .unwrap();
        assert!(matches!(t.to, AgentStatus::NeedsApproval(_)));
    }

    #[test]
    fn force_no_op_when_same_status() {
        let mut sm = StatusMachine::new(patterns(&[]));
        assert!(sm.force(AgentStatus::Idle).is_none());
    }

    #[test]
    fn tick_before_any_output_is_noop() {
        let mut sm = StatusMachine::new(patterns(&[]));
        assert!(sm.tick(t0() + IDLE_AFTER * 2).is_none());
    }

    #[test]
    fn feed_sideband_drives_needs_approval_without_patterns() {
        use oximux_core::AgentSidebandState;
        // Empty pattern table (the EMPTY_PATTERNS adapter case) — sideband
        // still transitions the machine to NeedsApproval with its tool.
        let mut sm = StatusMachine::new(patterns(&[]));
        let t = sm
            .feed_sideband(AgentSidebandState::NeedsApproval, Some("Bash".into()))
            .unwrap();
        assert_eq!(t.to, AgentStatus::NeedsApproval("Bash".into()));
        assert_eq!(sm.current(), &AgentStatus::NeedsApproval("Bash".into()));
    }

    #[test]
    fn feed_sideband_respects_terminal_guard() {
        use oximux_core::AgentSidebandState;
        let mut sm = StatusMachine::new(patterns(&[]));
        sm.note_exit(Some(0));
        // A Done session cannot be resurrected by a late sideband.
        assert!(
            sm.feed_sideband(AgentSidebandState::Working, None)
                .is_none()
        );
        assert!(matches!(sm.current(), AgentStatus::Done { code: Some(0) }));
    }

    #[test]
    fn feed_sideband_identity_is_noop() {
        use oximux_core::AgentSidebandState;
        let mut sm = StatusMachine::new(patterns(&[]));
        sm.feed_sideband(AgentSidebandState::Working, None); // → Running
        // Same state again — no transition emitted.
        assert!(
            sm.feed_sideband(AgentSidebandState::Working, None)
                .is_none()
        );
    }
}
