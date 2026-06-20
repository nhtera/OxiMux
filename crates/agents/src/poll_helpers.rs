//! Per-poll event processing extracted from `runtime_impl::poll_loop`.
//!
//! `process_poll_events` is the inner body of the poll loop: drain a batch of
//! `TerminalEvent`s, advance the `StatusMachine` (regex path) and the
//! `AgentOscScanner` (OSC-9999 sideband path), and publish `AgentSnapshot`s on
//! the watch channel. Pulling it out of `runtime_impl.rs` keeps that file
//! under the size cap and makes the regex/sideband interaction unit-testable
//! by feeding synthetic events — no PTY required.
//!
//! ## Why the scanner runs before the regex machine
//!
//! The regex machine's fallback rule is "any output while not Running →
//! Running". A chunk that is *purely* an OSC-9999 sideband sequence still
//! contains bytes, so feeding the raw chunk to the regex machine would trip
//! that fallback and clobber the status the sideband just set. So we strip the
//! OSC-9999 bytes first (`scanner.feed` returns `cleaned`) and feed only the
//! cleaned bytes to the regex machine. The sideband then applies last, via
//! `force()`, so it wins on a tie.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use oximux_core::AgentSnapshot;
use oximux_pty::{TerminalEvent, TerminalSessionId};
use tokio::sync::watch;

use crate::osc_sideband::AgentOscScanner;
use crate::status_machine::StatusMachine;

/// Process one drained batch of terminal events for `term_id`. Returns
/// `true` when an `Exit` event was seen (the caller breaks the poll loop).
///
/// Publishes on `status_tx`:
/// - regex/heuristic transitions as `AgentSnapshot { detail: None }` (any
///   stale sideband detail is cleared),
/// - sideband events as `AgentSnapshot { detail: Some(..) }` reflecting the
///   machine's current status (so detail-only changes, e.g. Edit→Bash while
///   still Running, still propagate),
/// - the idle-decay `tick()` transition.
pub fn process_poll_events(
    events: Vec<TerminalEvent>,
    term_id: TerminalSessionId,
    machine: &mut StatusMachine,
    scanner: &mut AgentOscScanner,
    status_tx: &watch::Sender<AgentSnapshot>,
    cancel_requested: &AtomicBool,
    now: Instant,
) -> bool {
    let mut saw_exit = false;
    // Defensive id filter. We use a per-session backend so all events here
    // are ours, but if that invariant ever changes (shared backend, fixture
    // replay reusing a backend) this guard keeps sessions from polluting each
    // other's status.
    for ev in events {
        match ev {
            TerminalEvent::Output { id, bytes } if id == term_id => {
                let scan = scanner.feed(&bytes);
                // Regex path on the OSC-9999-stripped bytes only.
                if let Some(t) = machine.feed(scan.cleaned.as_ref(), now) {
                    let _ = status_tx.send(AgentSnapshot::from_status(t.to));
                }
                // Sideband path: force the reported state, then publish the
                // machine's current status with the structured detail. Skip
                // once terminal — a Done/Failed session ignores late sideband.
                if let Some(sb) = scan.event {
                    let tool = sb.detail.tool_name.clone();
                    machine.feed_sideband(sb.state, tool);
                    if !machine.current().is_terminal() {
                        let _ = status_tx.send(AgentSnapshot {
                            status: machine.current().clone(),
                            detail: Some(sb.detail),
                        });
                    }
                }
            }
            TerminalEvent::Exit { id, code } if id == term_id => {
                // A cancel-triggered exit is a user decision, not a process
                // outcome — publish Interrupted ("Stopped") instead of the
                // Done/Failed exit-code mapping.
                let transition = if cancel_requested.load(Ordering::SeqCst) {
                    machine.note_interrupted()
                } else {
                    machine.note_exit(code)
                };
                if let Some(t) = transition {
                    let _ = status_tx.send(AgentSnapshot::from_status(t.to));
                }
                saw_exit = true;
            }
            _ => {}
        }
    }
    if let Some(t) = machine.tick(now) {
        let _ = status_tx.send(AgentSnapshot::from_status(t.to));
    }
    saw_exit
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use oximux_core::AgentStatus;

    use crate::cli::adapter::StatusPattern;

    const TERM: TerminalSessionId = TerminalSessionId(1);

    fn empty_machine() -> StatusMachine {
        let patterns: Arc<[StatusPattern]> = Vec::new().into();
        StatusMachine::new(patterns)
    }

    fn osc(payload: &str) -> Vec<u8> {
        let mut v = vec![0x1B, b']'];
        v.extend_from_slice(b"9999;");
        v.extend_from_slice(payload.as_bytes());
        v.push(0x07);
        v
    }

    fn run(events: Vec<TerminalEvent>) -> (AgentSnapshot, bool) {
        let mut machine = empty_machine();
        let mut scanner = AgentOscScanner::new();
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);
        let saw_exit = process_poll_events(
            events,
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        (snap, saw_exit)
    }

    #[test]
    fn sideband_needs_approval_overrides_empty_patterns() {
        // The regex table is empty (the Codex/Aider EMPTY_PATTERNS gap), yet
        // the OSC-9999 sideband still drives NeedsApproval with its tool.
        let bytes = osc(r#"{"v":1,"state":"needs_approval","tool":"Bash","tool_input":"ls"}"#);
        let (snap, saw_exit) = run(vec![TerminalEvent::Output { id: TERM, bytes }]);
        assert!(!saw_exit);
        assert_eq!(snap.status, AgentStatus::NeedsApproval("Bash".into()));
        let d = snap.detail.expect("detail");
        assert_eq!(d.tool_name.as_deref(), Some("Bash"));
        assert_eq!(d.tool_input_summary.as_deref(), Some("ls"));
    }

    #[test]
    fn pure_sideband_chunk_does_not_trip_running_fallback() {
        // Regression: a chunk that is ONLY the OSC-9999 sequence must not let
        // the regex "any output → Running" fallback clobber the sideband. The
        // cleaned bytes are empty, so the regex machine never fires.
        let bytes = osc(r#"{"v":1,"state":"needs_approval","tool":"Edit"}"#);
        let (snap, _) = run(vec![TerminalEvent::Output { id: TERM, bytes }]);
        assert_eq!(snap.status, AgentStatus::NeedsApproval("Edit".into()));
    }

    #[test]
    fn real_output_with_sideband_ends_on_sideband_state() {
        // Mixed chunk: real text (regex → Running) + an idle sideband. The
        // sideband applies last and wins.
        let mut bytes = b"some build output\n".to_vec();
        bytes.extend_from_slice(&osc(r#"{"v":1,"state":"idle"}"#));
        let (snap, _) = run(vec![TerminalEvent::Output { id: TERM, bytes }]);
        assert_eq!(snap.status, AgentStatus::Idle);
        assert!(snap.detail.is_some());
    }

    #[test]
    fn regex_path_clears_stale_sideband_detail() {
        // First a sideband sets detail; then plain output drives a regex
        // transition whose snapshot must carry detail: None.
        let mut machine = empty_machine();
        let mut scanner = AgentOscScanner::new();
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);

        let sb = osc(r#"{"v":1,"state":"working","tool":"Read"}"#);
        process_poll_events(
            vec![TerminalEvent::Output {
                id: TERM,
                bytes: sb,
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &cancel,
            Instant::now(),
        );
        assert!(rx.borrow().detail.is_some(), "sideband attached detail");

        // Force the machine off Running so plain output produces a transition.
        machine.force(AgentStatus::Idle);
        process_poll_events(
            vec![TerminalEvent::Output {
                id: TERM,
                bytes: b"plain text".to_vec(),
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        assert_eq!(snap.status, AgentStatus::Running);
        assert!(snap.detail.is_none(), "regex transition clears detail");
    }

    #[test]
    fn exit_event_publishes_terminal_and_reports_saw_exit() {
        let (snap, saw_exit) = run(vec![TerminalEvent::Exit {
            id: TERM,
            code: Some(0),
        }]);
        assert!(saw_exit);
        assert_eq!(snap.status, AgentStatus::Done { code: Some(0) });
        assert!(snap.detail.is_none());
    }

    #[test]
    fn cancel_requested_exit_maps_to_interrupted() {
        let mut machine = empty_machine();
        let mut scanner = AgentOscScanner::new();
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(true);
        let saw_exit = process_poll_events(
            vec![TerminalEvent::Exit {
                id: TERM,
                code: Some(9),
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &cancel,
            Instant::now(),
        );
        assert!(saw_exit);
        assert_eq!(rx.borrow().status, AgentStatus::Interrupted);
    }

    #[test]
    fn events_for_other_term_ids_are_ignored() {
        let other = TerminalSessionId(2);
        let bytes = osc(r#"{"v":1,"state":"working"}"#);
        let (snap, saw_exit) = run(vec![
            TerminalEvent::Output { id: other, bytes },
            TerminalEvent::Exit {
                id: other,
                code: Some(0),
            },
        ]);
        assert!(!saw_exit, "another session's Exit must not break our loop");
        assert_eq!(snap.status, AgentStatus::Idle, "status untouched");
    }

    #[test]
    fn sideband_after_terminal_is_ignored() {
        let mut machine = empty_machine();
        let mut scanner = AgentOscScanner::new();
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);
        // Terminal first.
        process_poll_events(
            vec![TerminalEvent::Exit {
                id: TERM,
                code: Some(0),
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &cancel,
            Instant::now(),
        );
        // Late sideband must not resurrect the session.
        let sb = osc(r#"{"v":1,"state":"working","tool":"Edit"}"#);
        process_poll_events(
            vec![TerminalEvent::Output {
                id: TERM,
                bytes: sb,
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        assert_eq!(snap.status, AgentStatus::Done { code: Some(0) });
        // Force-guard kept it terminal; detail stays cleared.
        assert!(snap.detail.is_none());
    }
}
