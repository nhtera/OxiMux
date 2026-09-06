//! The retry path end to end, from wire lines to a scheduled wake time.
//!
//! The unit tests either side of this seam are thorough but each mocks the
//! other's output. This drives real `stream-json` lines through the real
//! decoder and the real fold, then asks the real classifier and policy what to
//! do — so a decoder that stops emitting the reading, or a fold that stops
//! keeping it, fails here rather than silently disabling retries in a way every
//! unit test still passes.

use oximux_agent_core::thread::state::ChatThread;
use oximux_agent_core::thread::stream_json::decode_line;
use oximux_agents::retry::{MAX_ATTEMPTS, RetryClass, RetrySettings, classify_failure, schedule};

/// Unix ms well clear of the fixture's reset, so "past the reset" is meaningful.
const NOW: i64 = 1_788_400_000_000;
/// The `resetsAt` the fixture reports, in ms (the wire says seconds).
const RESET_MS: i64 = 1_788_462_000_000;

fn fold(lines: &[&str]) -> ChatThread {
    let mut thread = ChatThread::default();
    for line in lines {
        for ev in decode_line(line) {
            thread.apply(&ev);
        }
    }
    thread
}

const REJECTED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","utilization":100,"resetsAt":1788462000},"session_id":"sid"}"#;
const OVERAGE: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"overage","utilization":100},"session_id":"sid"}"#;
const LIMIT_ERROR: &str = r#"{"type":"result","subtype":"error","is_error":true,"api_error_status":429,"result":"rate limit","terminal_reason":"api_error","session_id":"sid"}"#;
const OVERLOAD_ERROR: &str = r#"{"type":"result","subtype":"error","is_error":true,"api_error_status":529,"result":"overloaded","terminal_reason":"api_error","session_id":"sid"}"#;
const PLAIN_ERROR: &str = r#"{"type":"result","subtype":"error","is_error":true,"api_error_status":400,"result":"bad request","terminal_reason":"api_error","session_id":"sid"}"#;

#[test]
fn a_closed_window_on_the_wire_becomes_a_wake_time_past_its_reset() {
    let thread = fold(&[REJECTED, LIMIT_ERROR]);
    let class = classify_failure(thread.last_rate_limit.as_ref(), thread.last_turn_failure.clone());
    assert_eq!(class, RetryClass::Window { resets_at_ms: RESET_MS });

    let wake = schedule(class, 0, NOW, &RetrySettings { max_automatic_wait: None }, 1)
        .expect("a closed window with a reset schedules");
    assert!(wake > RESET_MS, "must wait past the reset, got {wake}");
}

/// The money case, driven from the wire rather than from a hand-built struct.
#[test]
fn an_overage_on_the_wire_schedules_nothing() {
    let thread = fold(&[OVERAGE, LIMIT_ERROR]);
    let class = classify_failure(thread.last_rate_limit.as_ref(), thread.last_turn_failure.clone());
    assert_eq!(class, RetryClass::Spend);
    for attempt in 0..MAX_ATTEMPTS {
        assert!(
            schedule(class, attempt, NOW, &RetrySettings::default(), 1).is_none(),
            "an overage must never be scheduled"
        );
    }
}

#[test]
fn an_overloaded_provider_is_retried_without_any_rate_limit_reading() {
    let thread = fold(&[OVERLOAD_ERROR]);
    assert!(thread.last_rate_limit.is_none(), "no rate_limit_event arrived");
    let class = classify_failure(thread.last_rate_limit.as_ref(), thread.last_turn_failure.clone());
    assert_eq!(class, RetryClass::Overload);
    assert!(schedule(class, 0, NOW, &RetrySettings::default(), 1).is_some());
}

#[test]
fn an_ordinary_error_is_never_retried() {
    let thread = fold(&[PLAIN_ERROR]);
    let class = classify_failure(thread.last_rate_limit.as_ref(), thread.last_turn_failure.clone());
    assert_eq!(class, RetryClass::Other);
    assert!(schedule(class, 0, NOW, &RetrySettings::default(), 1).is_none());
}

/// A limit that closed and then reopened must not keep classifying failures as
/// window failures — the fold keeps the latest reading, so a later `allowed`
/// supersedes the earlier `rejected`.
#[test]
fn a_window_that_reopened_no_longer_explains_a_later_failure() {
    let allowed = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":3,"resetsAt":1788480000},"session_id":"sid"}"#;
    let thread = fold(&[REJECTED, allowed, PLAIN_ERROR]);
    let class = classify_failure(thread.last_rate_limit.as_ref(), thread.last_turn_failure.clone());
    assert_eq!(class, RetryClass::Other, "the stale rejection must not be reused");
}

/// A new turn clears the previous turn's typed failure, so a later failure the
/// provider says nothing about cannot inherit an older explanation.
#[test]
fn starting_a_turn_clears_the_previous_failure_detail() {
    let mut thread = fold(&[OVERLOAD_ERROR]);
    assert!(thread.last_turn_failure.is_some());
    thread.push_user_message_with_images("next", Vec::new());
    assert!(thread.last_turn_failure.is_none(), "a fresh turn starts with no failure");
    assert_eq!(
        classify_failure(thread.last_rate_limit.as_ref(), thread.last_turn_failure.clone()),
        RetryClass::Other
    );
}
