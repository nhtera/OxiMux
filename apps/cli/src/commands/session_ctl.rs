//! `oximux stop` / `oximux steer` — the two one-shot session controls. Stop
//! interrupts the in-flight turn (the session stays open); steer injects
//! guidance into a running turn on backends that support it.

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
            "mid-turn steering needs a backend with a message queue (pi); \
             claude and codex have none"
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
