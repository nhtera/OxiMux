//! `oximux stop` / `oximux steer` / `oximux release` — the one-shot session
//! controls. Stop interrupts the in-flight turn (the session stays open);
//! steer injects guidance into a running turn on backends that support it;
//! release lets go of the queue a host restart put on hold.

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

pub async fn stop(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::Cancel { session_id: session.into() }).await? {
        Response::Ack => Ok((
            json!({ "session_id": session, "cancelled": true }),
            format!("interrupt sent to {session} — the session stays open"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("Cancel", &other)),
    }
}

/// `Unsupported` from `Steer` has exactly one cause — the session's backend has no
/// mid-turn queue — so say that, and say what to do instead.
///
/// The same shape as `term`'s own mapper: a verb with a single knowable cause supplies
/// its own sentence, while the shared [`rpc_failure`] stays generic for every verb that
/// has no such certainty.
fn steer_failure(err: oximux_remote_proto::proto::RpcError) -> Failure {
    use oximux_remote_proto::proto::RpcError;
    if matches!(err, RpcError::Unsupported) {
        return Failure::new(
            "unsupported",
            crate::cli::exit::ERROR,
            "this session's agent cannot take guidance mid-turn",
        )
        .with_steps([
            "wait for the turn to end, then use `oximux send`".into(),
            "or `oximux stop` to interrupt it and send a fresh prompt".into(),
            "mid-turn steering needs a backend with a message queue (pi, omp, \
             claude 2.1.310 or later); codex and ACP agents have none"
                .into(),
        ]);
    }
    rpc_failure(err)
}

pub async fn steer(client: &Client, session: &str, text: &str) -> Result<(Value, String), Failure> {
    match client
        .call(Request::Steer { session_id: session.into(), text: text.into() })
        .await?
    {
        Response::Ack => Ok((
            json!({ "session_id": session, "steered": true }),
            format!("guidance sent to {session}"),
        )),
        Response::Error(e) => Err(steer_failure(e)),
        other => Err(unexpected_reply("Steer", &other)),
    }
}

/// `oximux release` — let go of the queue a restart is holding.
///
/// Reports the count the host returns rather than inferring one: the caller
/// can read how *many* lines a session holds (the health's `held_restored`)
/// and never which, so the number that went is the host's to say. Zero is a
/// success — "nothing was held" is an answer, not a failure — and the sentence
/// says so plainly so a script's operator is not left wondering whether the
/// verb worked.
pub async fn release(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::ReleaseHeld { session_id: session.into() }).await? {
        Response::Released { released } => Ok((
            json!({ "session_id": session, "released": released }),
            match released {
                // "Queued to go out", not "released to <session>": the hold
                // is lifted and the lines are ordinary queued sends now, so
                // with a turn running or a prompt open they wait. Saying
                // they reached the agent would be the overclaim the status
                // verb's own held-queue line had.
                0 => format!("{session} was holding nothing — no messages to release"),
                1 => format!("1 held message released in {session} — queued to go out"),
                n => format!("{n} held messages released in {session} — queued to go out"),
            },
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("ReleaseHeld", &other)),
    }
}
