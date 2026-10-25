//! Provider turn brackets: after its user turn ends, the CLI wakes itself
//! once per finished background task, and those continuation turns must keep
//! the session reading as working.

use super::*;
use crate::thread::state::ChatThread;

/// Replay a captured stream through the fold, starting a host turn where the
/// host sent a line (`command_lifecycle` `queued`), and record
/// `(turn_active, continuations_open)` after every frame.
fn replay(fixture: &str) -> Vec<(bool, u32)> {
    let mut thread = ChatThread::new();
    // An older capture has no lifecycle frames: the host sent before it began.
    if !fixture.contains(r#""command_lifecycle""#) {
        thread.push_user_message("prompt");
    }
    let mut trace = Vec::new();
    for line in fixture.lines() {
        if line.contains(r#""command_lifecycle""#) && line.contains(r#""queued""#) {
            thread.push_user_message("prompt");
        }
        for ev in decode_line(line) {
            thread.apply(&ev);
        }
        trace.push((thread.turn_active, thread.continuations_open));
    }
    trace
}

/// Captured on CLI 2.1.310: one user turn starting a background subagent and
/// two background shells, two continuations after its result (each opened by
/// an `init`, closed by a task-notification `result`), then a second user turn.
#[test]
fn continuations_after_the_user_turn_are_counted_and_closed() {
    let trace = replay(include_str!("testdata/stream_json_continuations_2_1_310.jsonl"));
    let lines: Vec<&str> =
        include_str!("testdata/stream_json_continuations_2_1_310.jsonl").lines().collect();
    let first_end = lines.iter().position(|l| l.contains(r#""user_message_uuids""#)).unwrap();
    // The init inside the user's own turn opens nothing.
    assert!(trace[..first_end].iter().all(|&(_, open)| open == 0), "{trace:?}");
    // After the result: each continuation's init opens it, its result closes it.
    let after: Vec<u32> = trace[first_end + 1..].iter().map(|&(_, open)| open).collect();
    assert!(after.contains(&1), "a continuation was counted: {after:?}");
    assert!(after.iter().all(|&open| open <= 1), "never more than the one running: {after:?}");
    assert_eq!(trace.last(), Some(&(false, 0)), "all closed at the end");
    // Every continuation's init leaves the session counted as working.
    for (i, line) in lines.iter().enumerate().skip(first_end + 1) {
        if line.contains(r#""init""#) {
            assert!(trace[i].0 || trace[i].1 > 0, "line {i} reads idle: {trace:?}");
        }
    }
}

/// The older capture (2.1.201) has an `init` arrive while the user turn is
/// still running; pairing by counter with a floor of 0 still ends at 0.
#[test]
fn an_init_inside_the_user_turn_does_not_leave_the_count_open() {
    let trace = replay(include_str!("testdata/stream_json_subagent.jsonl"));
    assert_eq!(trace.last().map(|&(_, open)| open), Some(0), "{trace:?}");
}

/// Background-task frames and subagent-tagged frames never open or close
/// anything.
#[test]
fn task_frames_and_subagent_frames_are_not_brackets() {
    let fixture = include_str!("testdata/stream_json_subagent.jsonl");
    for line in fixture.lines() {
        if line.contains(r#""parent_tool_use_id":"toolu"#) || line.contains(r#""subtype":"task_"#) {
            assert!(decode_line(line).iter().all(|e| !e.is_turn_bracket()), "{line}");
        }
    }
}

/// A provider turn that ends with an origin-less result while no host turn
/// runs still closes: the count never sticks open on an unexpected origin.
#[test]
fn any_result_outside_a_host_turn_closes_a_continuation() {
    let mut thread = ChatThread::new();
    thread.apply(&ThreadEvent::ProviderTurnStarted);
    assert_eq!(thread.continuations_open, 1);
    thread.apply(&ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None });
    assert_eq!(thread.continuations_open, 0);
}

/// Captured on CLI 2.1.310: the user sent a line while a continuation ran.
/// The CLI folded it in, and the one result that followed is marked as the
/// continuation's while carrying the line. It must end the user's turn too,
/// or the chat would stay "running" forever.
#[test]
fn a_user_line_folded_into_a_continuation_ends_its_turn() {
    let fixture = include_str!("testdata/stream_json_folded_continuation_2_1_310.jsonl");
    let trace = replay(fixture);
    assert_eq!(trace.last(), Some(&(false, 0)), "turn over, nothing open: {trace:?}");
    let events: Vec<ThreadEvent> = fixture.lines().flat_map(decode_line).collect();
    let ends = events.iter().filter(|e| matches!(e, ThreadEvent::TurnEnded { .. })).count();
    assert_eq!(ends, 2, "the first turn's result and the folded line's");
    assert!(
        events.iter().any(|e| matches!(e, ThreadEvent::UserMessagesDelivered { uuids } if uuids.len() == 1)),
        "the folded line is reported delivered"
    );
}
