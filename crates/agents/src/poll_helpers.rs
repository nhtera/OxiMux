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

use oximux_core::{AgentSnapshot, AgentStatus, SidebandDetail};
use oximux_pty::{TerminalEvent, TerminalSessionId};
use tokio::sync::watch;

use crate::osc_sideband::AgentOscScanner;
use crate::status_machine::StatusMachine;

/// Process one drained batch of terminal events for `term_id`. Returns
/// `true` when an `Exit` event was seen (the caller breaks the poll loop).
///
/// `last_prompt` is the poll loop's cache of the user's most recent prompt
/// (captured from a prompt-submit sideband). It is the agent's stable title:
/// a prompt arrives once, but the tool-step and idle events that follow carry
/// none, so the cached value is re-attached to every snapshot until the next
/// prompt replaces it. The volatile tool fields are NOT cached — only the
/// prompt rides a plain status edge.
///
/// Publishes on `status_tx`:
/// - regex/heuristic transitions carrying only the cached prompt (stale tool
///   detail is cleared, the title survives),
/// - sideband events as `AgentSnapshot { detail: Some(..) }` reflecting the
///   machine's current status (so detail-only changes, e.g. Edit→Bash while
///   still Running, still propagate),
/// - the idle-decay `tick()` transition.
#[allow(clippy::too_many_arguments)]
pub fn process_poll_events(
    events: Vec<TerminalEvent>,
    term_id: TerminalSessionId,
    machine: &mut StatusMachine,
    scanner: &mut AgentOscScanner,
    status_tx: &watch::Sender<AgentSnapshot>,
    last_prompt: &mut Option<String>,
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
                    let prev_msg = prev_last_message(status_tx);
                    let _ = status_tx.send(status_snapshot(t.to, last_prompt, prev_msg));
                }
                // Sideband path: force the reported state, then publish the
                // machine's current status with the structured detail. Skip
                // once terminal — a Done/Failed session ignores late sideband.
                if let Some(mut sb) = scan.event {
                    // Observable signal that an OSC-9999 packet was received and
                    // decoded — the one link in the emission→scanner path that
                    // can't be unit-tested (needs a live agent emitting to the
                    // PTY). `RUST_LOG=oximux_agents=debug` surfaces it.
                    tracing::debug!(
                        state = ?sb.state,
                        tool = ?sb.detail.tool_name,
                        prompt = ?sb.detail.prompt,
                        "agent OSC-9999 sideband decoded"
                    );
                    // A prompt-submit event carries the title; cache it so the
                    // tool-step and idle events that follow keep surfacing it.
                    if sb.detail.prompt.is_some() {
                        *last_prompt = sb.detail.prompt.clone();
                    }
                    // Carry the last reply forward when this event brings none:
                    // only a finished turn (`Stop`) supplies a message; the
                    // prompt/tool events that follow carry none and must not blank
                    // it, so a finished-turn agent keeps showing its reply across
                    // the next turn — matching the reference cockpit, which holds
                    // the last assistant message until a newer reply replaces it.
                    if sb.detail.last_message.is_none() {
                        sb.detail.last_message = status_tx
                            .borrow()
                            .detail
                            .as_ref()
                            .and_then(|d| d.last_message.clone());
                    }
                    let tool = sb.detail.tool_name.clone();
                    machine.feed_sideband(sb.state, tool);
                    if !machine.current().is_terminal() {
                        // Re-attach the cached prompt: an event with only a tool
                        // step (or none) still surfaces the agent's title.
                        sb.detail.prompt = last_prompt.clone();
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
                    let prev_msg = prev_last_message(status_tx);
                    let _ = status_tx.send(status_snapshot(t.to, last_prompt, prev_msg));
                }
                saw_exit = true;
            }
            _ => {}
        }
    }
    if let Some(t) = machine.tick(now) {
        let prev_msg = prev_last_message(status_tx);
        let _ = status_tx.send(status_snapshot(t.to, last_prompt, prev_msg));
    }
    saw_exit
}

/// The last assistant reply on the currently-published snapshot, if any. Read
/// before building a detail-less status edge so the message survives the
/// transition instead of being blanked. The `watch` borrow is dropped before
/// the caller's `send`, so it never contends with the publish.
fn prev_last_message(status_tx: &watch::Sender<AgentSnapshot>) -> Option<String> {
    status_tx
        .borrow()
        .detail
        .as_ref()
        .and_then(|d| d.last_message.clone())
}

/// Build a snapshot for a path with no sideband detail of its own (regex
/// transition, exit, idle decay). The volatile tool fields are dropped, but the
/// cached prompt (the agent's title) and the last assistant reply are
/// re-attached so a plain status edge never blanks the row's label or its
/// finished-turn message.
fn status_snapshot(
    status: AgentStatus,
    last_prompt: &Option<String>,
    last_message: Option<String>,
) -> AgentSnapshot {
    if last_prompt.is_some() || last_message.is_some() {
        AgentSnapshot {
            status,
            detail: Some(SidebandDetail {
                prompt: last_prompt.clone(),
                last_message,
                ..Default::default()
            }),
        }
    } else {
        AgentSnapshot::from_status(status)
    }
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

    /// A machine with one blocking pattern, so the regex path can drive a
    /// transition even after hooks take over (under hooks the output→Running
    /// *fallback* is suppressed, but pattern matches still fire — that is the
    /// immediate NeedsApproval/WaitingForInput backstop).
    fn approval_pattern_machine() -> StatusMachine {
        let patterns: Arc<[StatusPattern]> = vec![StatusPattern {
            regex: regex::bytes::Regex::new(r"Approval needed").unwrap(),
            transition: AgentStatus::NeedsApproval("x".into()),
        }]
        .into();
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
        let mut last_prompt = None;
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);
        let saw_exit = process_poll_events(
            events,
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        (snap, saw_exit)
    }

    #[test]
    fn idle_sideband_carries_the_last_assistant_message() {
        // The Stop hook reports idle WITH the agent's last reply (`msg`). The
        // published snapshot must carry it as `last_message` so the rail renders
        // a finished-turn agent's reply (and the emerald done-check), not "Idle".
        let bytes = osc(r#"{"v":1,"state":"idle","msg":"All done — tests pass."}"#);
        let (snap, _) = run(vec![TerminalEvent::Output { id: TERM, bytes }]);
        assert_eq!(snap.status, AgentStatus::Idle);
        assert_eq!(
            snap.detail.expect("detail").last_message.as_deref(),
            Some("All done — tests pass.")
        );
    }

    #[test]
    fn last_assistant_message_survives_the_next_turn() {
        // A finished turn brings the reply (idle + msg). The next prompt and its
        // tool steps carry none — the reply must persist so the row keeps showing
        // it instead of reverting to a bare status verb (reference-cockpit parity).
        let mut machine = empty_machine();
        let mut scanner = AgentOscScanner::new();
        let mut last_prompt = None;
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);

        let feed = |machine: &mut StatusMachine,
                    scanner: &mut AgentOscScanner,
                    last_prompt: &mut Option<String>,
                    bytes: Vec<u8>| {
            process_poll_events(
                vec![TerminalEvent::Output { id: TERM, bytes }],
                TERM,
                machine,
                scanner,
                &tx,
                last_prompt,
                &cancel,
                Instant::now(),
            );
        };

        feed(
            &mut machine,
            &mut scanner,
            &mut last_prompt,
            osc(r#"{"v":1,"state":"idle","msg":"All done — tests pass."}"#),
        );
        assert_eq!(
            rx.borrow().detail.clone().and_then(|d| d.last_message).as_deref(),
            Some("All done — tests pass.")
        );

        // Next prompt — no message; the reply must persist.
        feed(
            &mut machine,
            &mut scanner,
            &mut last_prompt,
            osc(r#"{"v":1,"state":"working","prompt":"now refactor"}"#),
        );
        assert_eq!(
            rx.borrow().detail.clone().and_then(|d| d.last_message).as_deref(),
            Some("All done — tests pass."),
            "reply persists across the next prompt"
        );

        // A tool step — still no message; reply persists, tool surfaces fresh.
        feed(
            &mut machine,
            &mut scanner,
            &mut last_prompt,
            osc(r#"{"v":1,"state":"working","tool":"Edit","tool_input":"y.rs"}"#),
        );
        let d = rx.borrow().detail.clone().expect("detail");
        assert_eq!(d.last_message.as_deref(), Some("All done — tests pass."));
        assert_eq!(d.tool_name.as_deref(), Some("Edit"));

        // A newer finished turn replaces the reply.
        feed(
            &mut machine,
            &mut scanner,
            &mut last_prompt,
            osc(r#"{"v":1,"state":"idle","msg":"Refactor complete."}"#),
        );
        assert_eq!(
            rx.borrow().detail.clone().and_then(|d| d.last_message).as_deref(),
            Some("Refactor complete.")
        );
    }

    #[test]
    fn sideband_needs_approval_overrides_empty_patterns() {
        // The regex table is empty (the Codex/Pi EMPTY_PATTERNS gap), yet
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
        // A sideband sets stale tool detail; then a regex pattern match drives a
        // transition whose snapshot must carry detail: None. The pattern path
        // still fires under hooks — only the output→Running fallback is gated.
        let mut machine = approval_pattern_machine();
        let mut scanner = AgentOscScanner::new();
        let mut last_prompt = None;
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
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        assert!(rx.borrow().detail.is_some(), "sideband attached detail");

        // A blocking-prompt pattern match transitions Working → NeedsApproval;
        // its snapshot drops the stale tool detail.
        process_poll_events(
            vec![TerminalEvent::Output {
                id: TERM,
                bytes: b"Approval needed: proceed?".to_vec(),
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        assert!(matches!(snap.status, AgentStatus::NeedsApproval(_)));
        // No prompt was ever submitted, so the regex transition carries no
        // detail at all — stale tool info is cleared.
        assert!(snap.detail.is_none(), "regex transition clears tool detail");
    }

    #[test]
    fn prompt_caches_and_rides_a_later_tool_step() {
        // A prompt-submit event carries the title; a later working event
        // carries only a tool. The cached prompt must still be on the snapshot
        // so the row keeps its label while the tool subline updates.
        let mut machine = empty_machine();
        let mut scanner = AgentOscScanner::new();
        let mut last_prompt = None;
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);

        let submit = osc(r#"{"v":1,"state":"working","prompt":"fix the parser"}"#);
        process_poll_events(
            vec![TerminalEvent::Output { id: TERM, bytes: submit }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        assert_eq!(last_prompt.as_deref(), Some("fix the parser"));

        // Next: a tool step with no prompt field.
        let tool = osc(r#"{"v":1,"state":"working","tool":"Edit","tool_input":"x.rs"}"#);
        process_poll_events(
            vec![TerminalEvent::Output { id: TERM, bytes: tool }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        let d = rx.borrow().detail.clone().expect("detail");
        assert_eq!(d.prompt.as_deref(), Some("fix the parser"), "title persists");
        assert_eq!(d.tool_name.as_deref(), Some("Edit"), "tool updates");
    }

    #[test]
    fn cached_prompt_survives_a_regex_transition() {
        // After a prompt is cached, a regex pattern transition must keep the
        // prompt (the title) even though it drops the volatile tool fields.
        let mut machine = approval_pattern_machine();
        let mut scanner = AgentOscScanner::new();
        let mut last_prompt = None;
        let (tx, rx) = watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
        let cancel = AtomicBool::new(false);

        let submit = osc(r#"{"v":1,"state":"working","prompt":"add tests"}"#);
        process_poll_events(
            vec![TerminalEvent::Output { id: TERM, bytes: submit }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        // A blocking-prompt pattern match → regex transition (Working →
        // NeedsApproval); the cached prompt must ride the new snapshot.
        process_poll_events(
            vec![TerminalEvent::Output {
                id: TERM,
                bytes: b"Approval needed: proceed?".to_vec(),
            }],
            TERM,
            &mut machine,
            &mut scanner,
            &tx,
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        assert!(matches!(snap.status, AgentStatus::NeedsApproval(_)));
        let d = snap.detail.expect("regex snapshot keeps the cached prompt");
        assert_eq!(d.prompt.as_deref(), Some("add tests"));
        assert!(d.tool_name.is_none(), "tool fields stay cleared");
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
        let mut last_prompt = None;
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
            &mut last_prompt,
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
        let mut last_prompt = None;
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
            &mut last_prompt,
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
            &mut last_prompt,
            &cancel,
            Instant::now(),
        );
        let snap = rx.borrow().clone();
        assert_eq!(snap.status, AgentStatus::Done { code: Some(0) });
        // Force-guard kept it terminal; detail stays cleared.
        assert!(snap.detail.is_none());
    }
}
