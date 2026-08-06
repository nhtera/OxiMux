//! `oximux send` — a prompt into an existing session. Accepted ≠ finished:
//! the Ack means the host took the prompt (queueing mid-turn is normal); the
//! default mode then streams the turn, `--no-wait` returns immediately.

use oximux_remote_proto::messages::SendPromptReq;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use super::attach::{Stop, StreamEnd, stream_session};
use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// The verb as dispatched, `--output-schema` included.
///
/// Split from [`run`] so the schema loop can send its corrections through the
/// plain path: a correction that re-entered the checking path would recurse
/// once per retry instead of being bounded by the retry counter.
pub async fn run_checked(
    client: &Client,
    session: &str,
    prompt: String,
    output_schema: Option<&str>,
    no_wait: bool,
    turn_timeout: Option<u64>,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    // The prompt arrives already resolved (`-` became stdin text in `precheck`).
    // The schema is compiled before the prompt is sent: a bad one must not cost
    // an agent turn.
    let schema = output_schema.map(crate::output_schema::OutputSchema::load).transpose()?;
    let (mut base, human) = run(client, session, &prompt, no_wait, turn_timeout, json_mode).await?;
    let Some(schema) = schema else { return Ok((base, human)) };
    let value =
        crate::output_schema::enforce(client, session, &schema, turn_timeout, json_mode).await?;
    let human = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    base["output"] = value;
    Ok((base, human))
}

pub async fn run(
    client: &Client,
    session: &str,
    prompt: &str,
    no_wait: bool,
    turn_timeout: Option<u64>,
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
    match stream_session(
        client,
        session,
        Some(from),
        json_mode,
        false,
        Stop::TurnEnded,
        super::turn_deadline(turn_timeout),
    )
    .await?
    {
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
        _ => Err(Failure::new("protocol", exit::ERROR, "the stream ended unexpectedly")),
    }
}
