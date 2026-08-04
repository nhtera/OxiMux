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

pub async fn steer(client: &Client, session: &str, text: &str) -> Result<(Value, String), Failure> {
    match client
        .call(Request::Steer { session_id: session.into(), text: text.into() })
        .await?
    {
        Response::Ack => Ok((
            json!({ "session_id": session, "steered": true }),
            format!("guidance sent to {session}"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("Steer", &other)),
    }
}
