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
        // `Ack` carries no did-anything-happen bit — a SQL DELETE of zero rows
        // is `Ok` — so report the postcondition, never the action. Claiming
        // `deleted <key>` for a key that was never set is an assertion this
        // reply cannot support. The idempotence itself is deliberate host-side
        // design (`dispatcher/state.rs`), and stays.
        Response::Ack => Ok((
            json!({ "key": key, "state": "absent" }),
            format!("no `{key}` remains"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("StateDelete", &other)),
    }
}

/// Stream changes until Ctrl+C, carrying a cursor so a reconnect can say
/// whether it missed anything.
///
/// Every line carries the `seq` it was delivered at; passing the last one back
/// as `--since` resumes there. The host replays the gap when its ring still
/// covers it and otherwise resyncs with a fresh baseline — and says which, so a
/// watcher is never quietly stale. That distinction is the whole reason the
/// cursor exists, and it is why `resynced` is printed rather than inferred.
pub async fn watch(
    client: &Client,
    prefix: Option<String>,
    since: Option<u64>,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    use std::io::Write as _;

    let reply = client
        .call(Request::StateWatchFrom { prefix: prefix.clone(), since_seq: since })
        .await?;
    let started = match reply {
        Response::StateWatchStarted(started) => started,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("StateWatchFrom", &other)),
    };
    let mut cursor = started.seq;

    // A resync after an explicit `--since` is the one thing a watcher must not
    // miss: it means transitions happened that it will never see. Marked on
    // stdout in both modes — in JSON because a consumer has to branch on it,
    // and in human form because a person reading a board wants to know their
    // history has a hole in it. Not marked for a fresh watch: there is no gap
    // to have missed when you asked for none.
    let resynced = started.baseline.is_some() && since.is_some();
    if resynced {
        if json_mode {
            println!("{}", json!({ "resynced": true, "since": since, "seq": cursor }));
        } else {
            println!("— resynced: the host could not replay from {} —", since.unwrap_or(0));
        }
    }
    if let Some(baseline) = &started.baseline {
        for entry in baseline {
            if json_mode {
                println!(
                    "{}",
                    json!({ "baseline": true, "seq": cursor, "entry": entry_json(entry) })
                );
            } else {
                println!("{}", entry_line(entry));
            }
        }
    }
    for change in &started.replay {
        cursor = cursor.max(change.seq);
        emit_change(change.seq, &change.key, change.entry.as_ref(), json_mode, true);
    }
    let _ = std::io::stdout().flush();

    loop {
        let frame = tokio::select! {
            frame = client.recv_frame() => frame?,
            _ = tokio::signal::ctrl_c() => break,
        };
        // Replies cannot arrive here — nothing else was sent — but a push for
        // some other subscription could, so filter on the variant rather than
        // assuming. Only the cursor-bearing push is expected: this subscribed
        // with `StateWatchFrom`, and the host answers that with `StateChangedAt`.
        let Response::StateChangedAt(change) = frame else {
            if !is_push(&frame) {
                return Err(unexpected_reply("StateWatchFrom", &frame));
            }
            continue;
        };
        cursor = cursor.max(change.seq);
        emit_change(change.seq, &change.key, change.entry.as_ref(), json_mode, false);
        let _ = std::io::stdout().flush();
    }
    // The cursor comes back in the result so a script can `--since` it next
    // time without having to parse the stream it just printed.
    Ok((
        json!({ "watched": prefix, "detached": true, "seq": cursor, "resynced": resynced }),
        format!("detached at seq {cursor}"),
    ))
}

/// One change line, in either mode. `replayed` marks the catch-up ones so a
/// reader can tell what happened while they were away from what is happening
/// now.
fn emit_change(
    seq: u64,
    key: &str,
    entry: Option<&oximux_remote_proto::messages::StateEntryWire>,
    json_mode: bool,
    replayed: bool,
) {
    match (entry, json_mode) {
        (Some(entry), true) => println!(
            "{}",
            json!({ "seq": seq, "replayed": replayed, "entry": entry_json(entry) })
        ),
        (Some(entry), false) => println!("{}", entry_line(entry)),
        (None, true) => {
            println!("{}", json!({ "seq": seq, "replayed": replayed, "key": key, "deleted": true }))
        }
        (None, false) => println!("{key}  (deleted)"),
    }
}
