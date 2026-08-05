//! `oximux wait` — block until a session reaches a state, bounded by
//! `--timeout` (exit 4). The current state is checked first, so waiting for a
//! state the session is already in returns immediately instead of hanging on
//! an event that already happened.

use oximux_agent_core::thread::ThreadEvent;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use super::attach::{Stop, StreamEnd, stream_session};
use crate::cli::{WaitUntil, exit};
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// What the retained backlog says about the session right now.
struct ScanState {
    /// The last turn-driving event is a completed turn (or nothing has ever
    /// run) — i.e. no user prompt is newer than the newest `TurnEnded`.
    done: bool,
    last_seq: u64,
}

/// Classify the backlog. The scan reads only the retained ring, which is
/// enough: a prompt that aged out of the ring has either ended its turn (a
/// newer `TurnEnded` is retained or the ring is empty) or is still producing
/// events (which are retained).
fn scan(frames: &[oximux_remote_proto::messages::HostEvent]) -> ScanState {
    let mut last_user = None;
    let mut last_end = None;
    let mut last_seq = 0;
    for frame in frames {
        last_seq = frame.seq;
        match frame.event() {
            Ok(ThreadEvent::UserMessage { .. }) => last_user = Some(frame.seq),
            Ok(ThreadEvent::TurnEnded { .. }) => last_end = Some(frame.seq),
            _ => {}
        }
    }
    let done = match (last_user, last_end) {
        (Some(user), Some(end)) => end > user,
        (Some(_), None) => false,
        // No prompt in the retained window: nothing is running.
        (None, _) => true,
    };
    ScanState { done, last_seq }
}

/// Is a decision still outstanding on this session?
///
/// One `GetSessionInfo`, used only to settle the second half of `--until idle`
/// after the stream has settled the first.
async fn still_awaiting(client: &Client, session: &str) -> Result<bool, Failure> {
    match client.call(Request::GetSessionInfo { session_id: session.into() }).await? {
        Response::SessionInfo(info) => Ok(info.summary.awaiting_permission),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("GetSessionInfo", &other)),
    }
}

pub async fn run(
    client: &Client,
    session: &str,
    until: WaitUntil,
    timeout_secs: u64,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(1));

    // Current state first.
    let awaiting = match client.call(Request::GetSessionInfo { session_id: session.into() }).await? {
        Response::SessionInfo(info) => info.summary.awaiting_permission,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("GetSessionInfo", &other)),
    };
    let backlog = match client
        .call(Request::EventsSince { session_id: session.into(), after_seq: 0 })
        .await?
    {
        Response::Events(frames) => frames,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("EventsSince", &other)),
    };
    let state = scan(&backlog);

    let satisfied_now = match until {
        WaitUntil::NeedsApproval => awaiting,
        WaitUntil::Done => state.done,
        WaitUntil::Idle => state.done && !awaiting,
    };
    let reached = |how: &str| {
        Ok((
            json!({ "session_id": session, "state": format!("{until:?}").to_lowercase(), "via": how }),
            format!("session {session} reached the awaited state"),
        ))
    };
    if satisfied_now {
        return reached("already");
    }

    // Not there yet: watch the live stream (quietly) from where the scan left
    // off until the condition or the deadline.
    let stop = match until {
        WaitUntil::NeedsApproval => Stop::NeedsApproval,
        WaitUntil::Done | WaitUntil::Idle => Stop::TurnEnded,
    };
    match stream_session(
        client,
        session,
        Some(state.last_seq),
        json_mode,
        true,
        stop,
        Some(deadline),
    )
    .await?
    {
        // `idle` is BOTH halves: done, and nothing awaiting a decision. The
        // stream stops on the turn ending, which settles only the first — so
        // the second is re-read here rather than assumed. Without this, a
        // session carrying a pending request into the turn that just ended
        // would be reported idle while a decision is still outstanding, and a
        // script gated on `--until idle` would proceed against a blocked agent.
        StreamEnd::TurnEnded { .. } if until == WaitUntil::Idle => {
            if still_awaiting(client, session).await? {
                return Err(Failure::new(
                    "not-idle",
                    exit::ERROR,
                    "the turn ended but the session is awaiting a decision",
                )
                .with_steps([
                    format!("`oximux permit ls {session}` shows what is pending"),
                    "decide it, then wait again".into(),
                ]));
            }
            reached("stream")
        }
        StreamEnd::TurnEnded { .. } | StreamEnd::NeedsApproval => reached("stream"),
        StreamEnd::Deadline => Err(Failure::new(
            "timeout",
            exit::TIMEOUT,
            format!("timed out waiting for the session to reach that state ({timeout_secs}s)"),
        )
        .with_steps(["raise --timeout, or check the session with `oximux attach`".into()])),
        StreamEnd::Detached => Err(Failure::new(
            "detached",
            exit::ERROR,
            "interrupted before the state was reached",
        )),
    }
}
