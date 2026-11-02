//! `oximux send` — a prompt into an existing session. Accepted ≠ finished:
//! the Ack means the host took the prompt (queueing mid-turn is normal); the
//! default mode then streams the turn, `--no-wait` returns immediately.

use oximux_remote_proto::delivery::{ReceiptStateWire, SendOutcomeWire, SendV2Req};
use oximux_remote_proto::messages::SendPromptReq;
use oximux_remote_proto::proto::{Request, Response, RpcError, SEND_V2_MIN_VERSION};
use serde_json::{Value, json};

use super::attach::{Stop, StreamEnd, StreamOpts, stream_session};
use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// The verb as dispatched, `--output-schema` included.
///
/// Split from [`run`] so the schema loop can send its corrections through the
/// plain path: a correction that re-entered the checking path would recurse
/// once per retry instead of being bounded by the retry counter.
/// What one `send` invocation was asked to do, past the session and prompt.
///
/// Grouped because the verb accumulated four independent bounds and switches,
/// and a call site reading `(…, None, false, None, None, true)` communicates
/// none of them.
pub struct SendArgs<'a> {
    pub output_schema: Option<&'a str>,
    pub no_wait: bool,
    pub turn_timeout: Option<u64>,
    pub stalled_after: Option<u64>,
    pub json_mode: bool,
}

pub async fn run_checked(
    client: &Client,
    session: &str,
    prompt: String,
    args: SendArgs<'_>,
) -> Result<(Value, String), Failure> {
    let SendArgs { output_schema, no_wait, turn_timeout, stalled_after, json_mode } = args;
    // The prompt arrives already resolved (`-` became stdin text in `precheck`).
    // The schema is compiled before the prompt is sent: a bad one must not cost
    // an agent turn.
    let schema = output_schema.map(crate::output_schema::OutputSchema::load).transpose()?;
    let (mut base, human) =
        run(client, session, &prompt, no_wait, turn_timeout, stalled_after, json_mode).await?;
    let Some(schema) = schema else { return Ok((base, human)) };
    let value =
        crate::output_schema::enforce(client, session, &schema, turn_timeout, json_mode).await?;
    let human = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    base["output"] = value;
    Ok((base, human))
}

/// Send `prompt` without waiting for the turn, under `key` when the caller
/// has one (else a fresh one, returned): a v37 host sends a key at most once
/// and answers a repeat with where the first send stands.
///
/// A host that cannot keep that promise (older, or one that keeps no
/// receipts) gets the plain send only when the caller gave no key; a caller
/// retrying under its key is refused instead, since a plain send could
/// deliver the message twice.
pub async fn send_keyed(client: &Client, session: &str, prompt: &str, key: Option<&str>) -> Result<(Value, String), Failure> {
    let minted = key.is_none().then(|| uuid::Uuid::new_v4().to_string());
    let key_text = key.or(minted.as_deref()).unwrap_or_default();
    if client.host_version >= SEND_V2_MIN_VERSION {
        let req = Request::SendV2(SendV2Req {
            session_id: session.into(),
            text: prompt.into(),
            images: vec![],
            idempotency_key: key_text.into(),
            corr_id: super::next_corr_id(),
        });
        match client.call(req).await? {
            Response::SendOutcome(SendOutcomeWire::Queued) => {
                let base = json!({ "session_id": session, "accepted": true, "outcome": "queued", "idempotency_key": key_text });
                return Ok((base, format!("sent to {session} (key {key_text})")));
            }
            Response::SendOutcome(SendOutcomeWire::Duplicate(state)) => {
                let human = duplicate_note(session, state);
                let base = json!({
                    "session_id": session, "accepted": true, "outcome": "duplicate", "state": receipt_word(state),
                    "idempotency_key": key_text, "note": human,
                });
                return Ok((base, human));
            }
            Response::Error(RpcError::Unsupported) => {}
            Response::Error(e) => return Err(rpc_failure(e)),
            other => return Err(unexpected_reply("SendV2", &other)),
        }
    }
    if key.is_some() {
        return Err(Failure::new(
            "not-idempotent",
            exit::ERROR,
            format!("this host cannot hold a send to its key, so it was not sent again to {session}"),
        )
        .with_steps([format!(
            "read {session}'s conversation first; send without a key only if the message is not there"
        )]));
    }
    let (mut base, human) = run(client, session, prompt, true, None, None, true).await?;
    base["outcome"] = json!("sent");
    base["idempotent"] = json!(false);
    Ok((base, human))
}

/// What a repeated key means for the caller, by where the first send stands.
fn duplicate_note(session: &str, state: ReceiptStateWire) -> String {
    match state {
        ReceiptStateWire::Pending => format!("not sent again: {session} has it queued or is about to start on it; do not resend"),
        ReceiptStateWire::Accepted => format!("not sent again: {session} already started on it"),
        ReceiptStateWire::Rejected => format!(
            "not sent again: it was refused and nothing will retry it; send it with a new key if {session} still needs it"
        ),
        ReceiptStateWire::Unknown => format!(
            "not sent again: the host restarted before it knew whether {session} got it; read its conversation, \
             then send with a new key only if the message is not there"
        ),
    }
}

fn receipt_word(state: ReceiptStateWire) -> &'static str {
    match state {
        ReceiptStateWire::Pending => "pending",
        ReceiptStateWire::Accepted => "accepted",
        ReceiptStateWire::Rejected => "rejected",
        ReceiptStateWire::Unknown => "unknown",
    }
}

pub async fn run(
    client: &Client,
    session: &str,
    prompt: &str,
    no_wait: bool,
    turn_timeout: Option<u64>,
    stalled_after: Option<u64>,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    // Capture the live edge BEFORE sending, so the follow stream replays this
    // prompt's own turn from its first event rather than joining mid-turn.
    let from = match client.call(Request::GetSessionInfo { session_id: session.into() }).await? {
        Response::SessionInfo(info) => info.summary.last_seq,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("GetSessionInfo", &other)),
    };
    match client
        .call(Request::SendPrompt(SendPromptReq {
            session_id: session.into(),
            text: prompt.into(),
            images: vec![],
            corr_id: super::next_corr_id(),
        }))
        .await?
    {
        Response::Ack => {}
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("SendPrompt", &other)),
    }
    let base = json!({ "session_id": session, "accepted": true });
    // `run_checked` mutates this into the schema-validated output; a plain send
    // returns it untouched.
    if no_wait {
        return Ok((
            base,
            format!("accepted — watch with `oximux attach {session}` or `oximux wait {session} --until done`"),
        ));
    }
    let opts = StreamOpts {
        from: Some(from),
        json_mode,
        quiet: false,
        stop: Stop::TurnEnded,
        deadline: super::turn_deadline(turn_timeout),
        stall_after: stalled_after.map(std::time::Duration::from_secs),
    };
    match stream_session(client, session, opts).await? {
        StreamEnd::TurnEnded { is_error: false } => Ok((base, "✓ done".into())),
        StreamEnd::TurnEnded { is_error: true } => Err(Failure::new(
            "turn-error",
            exit::ERROR,
            format!("the turn ended with an error (session {session})"),
        )),
        StreamEnd::Detached => {
            Ok((base, format!("detached — the agent keeps running (session {session})")))
        }
        // Only reachable with `--turn-timeout`; without it the stream carries no
        // deadline to pass.
        StreamEnd::Deadline => {
            Err(super::turn_timeout_failure(session, turn_timeout.unwrap_or_default()))
        }
        StreamEnd::Stalled { quiet_secs, last_seq } => {
            Err(super::stall_failure(session, quiet_secs, last_seq))
        }
        _ => Err(Failure::new("protocol", exit::ERROR, "the stream ended unexpectedly")),
    }
}
