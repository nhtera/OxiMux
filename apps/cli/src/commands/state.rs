//! `oximux state` — the coordination blackboard.
//!
//! `--if-version` is the point of the verb. A script that reads a value, edits
//! it, and writes it back passes the version it read; if another agent got
//! there first the write is refused with **exit 5** and the current value on
//! stdout, so the caller can merge and retry rather than clobber. Without that
//! the board would be a race with extra steps.

use oximux_remote_proto::messages::{StateEntryWire, StateSetReq};
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, is_push, rpc_failure, unexpected_reply};
use crate::output::Failure;

fn entry_json(e: &StateEntryWire) -> Value {
    json!({
        "key": e.key,
        // Parsed back into real JSON for the envelope: a caller piping this to
        // `jq` wants the value, not a string containing it.
        "value": serde_json::from_str::<Value>(&e.value_json).unwrap_or(Value::Null),
        "version": e.version,
        "updated_at": e.updated_at,
    })
}

fn entry_line(e: &StateEntryWire) -> String {
    format!("{}  v{}  {}", e.key, e.version, e.value_json)
}

pub async fn get(client: &Client, key: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::StateGet { key: key.into() }).await? {
        Response::StateValue(Some(entry)) => {
            let human = entry.value_json.clone();
            Ok((entry_json(&entry), human))
        }
        // Absent is a normal answer, not a failure: a script asking "has anyone
        // claimed this yet" must be able to tell "no" from "the host is down",
        // and exit 0 with a null value is that distinction.
        Response::StateValue(None) => {
            Ok((json!({ "key": key, "value": null, "version": 0 }), "(unset)".into()))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("StateGet", &other)),
    }
}

pub async fn set(
    client: &Client,
    key: &str,
    value: &str,
    if_version: Option<u64>,
) -> Result<(Value, String), Failure> {
    // Validated here as well as host-side so a typo costs a round trip, not an
    // opaque BadRequest.
    serde_json::from_str::<Value>(value).map_err(|e| {
        Failure::new("usage", exit::USAGE, format!("the value must be JSON: {e}"))
            .with_steps(["quote strings as JSON, e.g. '\"claimed\"' or '{\"n\":1}'".into()])
    })?;
    let reply = client
        .call(Request::StateSet(StateSetReq {
            key: key.into(),
            value_json: value.into(),
            if_version,
        }))
        .await?;
    match reply {
        Response::StateValue(Some(entry)) => {
            let human = format!("{}  v{}", entry.key, entry.version);
            Ok((entry_json(&entry), human))
        }
        // The conditional write lost. Exit 5 (denied) rather than a generic
        // error, so a retry loop can branch on the code alone — and the current
        // entry rides in the message so it needs no second call to learn what
        // it collided with.
        Response::StateConflict(current) => {
            let version = current.as_ref().map(|e| e.version).unwrap_or(0);
            let shown = current.as_ref().map(entry_json).unwrap_or(Value::Null);
            Err(Failure::new(
                "version-conflict",
                exit::DENIED,
                format!("`{key}` is at version {version}, not the {} this write required",
                    if_version.unwrap_or(0)),
            )
            .with_steps([
                format!("current value: {shown}"),
                format!("re-read, merge, then retry with --if-version {version}"),
            ]))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("StateSet", &other)),
    }
}

pub async fn delete(client: &Client, key: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::StateDelete { key: key.into() }).await? {
        Response::Ack => Ok((json!({ "deleted": key }), format!("deleted {key}"))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("StateDelete", &other)),
    }
}

/// Stream changes until Ctrl+C. The baseline (every matching key as it stands
/// now) is printed first, so a watcher never has to `get` before it watches.
pub async fn watch(
    client: &Client,
    prefix: Option<String>,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    use std::io::Write as _;

    let reply = client.call(Request::StateWatch { prefix: prefix.clone() }).await?;
    let baseline = match reply {
        Response::StateSnapshot(entries) => entries,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("StateWatch", &other)),
    };
    for entry in &baseline {
        if json_mode {
            println!("{}", json!({ "baseline": true, "entry": entry_json(entry) }));
        } else {
            println!("{}", entry_line(entry));
        }
    }
    let _ = std::io::stdout().flush();

    loop {
        let frame = tokio::select! {
            frame = client.recv_frame() => frame?,
            _ = tokio::signal::ctrl_c() => break,
        };
        // Replies cannot arrive here — nothing else was sent — but a push for
        // some other subscription could, so filter on the variant rather than
        // assuming.
        let Response::StateChanged { key, entry } = frame else {
            if !is_push(&frame) {
                return Err(unexpected_reply("StateWatch", &frame));
            }
            continue;
        };
        match (&entry, json_mode) {
            (Some(entry), true) => {
                println!("{}", json!({ "entry": entry_json(entry) }));
            }
            (Some(entry), false) => println!("{}", entry_line(entry)),
            (None, true) => println!("{}", json!({ "key": key, "deleted": true })),
            (None, false) => println!("{key}  (deleted)"),
        }
        let _ = std::io::stdout().flush();
    }
    Ok((json!({ "watched": prefix, "detached": true }), "detached".into()))
}
