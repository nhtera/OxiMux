//! `oximux config` — the host's own settings: the spawn caps, the delegation
//! switches, the stall clock.
//!
//! **Why this is an RPC and not a file this side writes.** A running host
//! reads these from a `watch` channel set when its core was built, so writing
//! the settings rows from here would change nothing until that host restarted
//! — while reporting success. The host validates, stores and applies, then
//! says what it enforces; everything below is printing.
//!
//! `get` prints the value alone, so a shell can read it. `list` prints a table
//! and marks the values nobody has set, because "8" and "8 because nobody
//! chose otherwise" are different answers to the only question being asked.

use oximux_remote_proto::host_config::HostConfigEntryWire;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

fn entry_json(entry: &HostConfigEntryWire) -> Value {
    json!({ "key": entry.key, "value": entry.value, "is_default": entry.is_default })
}

/// The entries a settings reply carries, or the refusal it is instead.
async fn ask(client: &Client, request: Request, verb: &str) -> Result<Vec<HostConfigEntryWire>, Failure> {
    match client.call(request).await? {
        Response::HostConfig(entries) => Ok(entries),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply(verb, &other)),
    }
}

/// One entry, for the verbs that name a key. A reply that carried none (or
/// several) for one key is a host disagreeing with its own protocol.
fn only(entries: Vec<HostConfigEntryWire>, verb: &str) -> Result<HostConfigEntryWire, Failure> {
    let n = entries.len();
    entries.into_iter().next().filter(|_| n == 1).ok_or_else(|| {
        Failure::new("protocol", crate::cli::exit::ERROR, format!("{verb} answered with {n} settings, not one"))
    })
}

pub async fn get(client: &Client, key: &str) -> Result<(Value, String), Failure> {
    let entry = only(ask(client, Request::HostConfigRead { key: Some(key.into()) }, "config get").await?, "config get")?;
    // The value alone: this is what a script reads.
    Ok((entry_json(&entry), entry.value.clone()))
}

pub async fn set(client: &Client, key: &str, value: &str) -> Result<(Value, String), Failure> {
    let asked = value.trim().to_string();
    let entry = only(
        ask(client, Request::HostConfigWrite { key: key.into(), value: value.into() }, "config set").await?,
        "config set",
    )?;
    // A cap is clamped to its ceiling, so what the host enforces is not always
    // what was asked for. Said plainly, because a limit someone believes they
    // have and does not is worse than a refusal.
    let human = if entry.value == asked {
        format!("{} = {}", entry.key, entry.value)
    } else {
        format!("{} = {} (asked for {asked}; this host's limit)", entry.key, entry.value)
    };
    Ok((entry_json(&entry), human))
}

pub async fn list(client: &Client) -> Result<(Value, String), Failure> {
    let entries = ask(client, Request::HostConfigRead { key: None }, "config list").await?;
    let width = entries.iter().map(|e| e.key.len()).max().unwrap_or(0);
    let human = entries
        .iter()
        .map(|e| {
            // A rule list is many lines; shown on one row it would break the
            // table, so it is summarised and `get` prints it in full.
            let value = match e.value.lines().count() {
                0 | 1 => e.value.clone(),
                n => format!("{n} rules"),
            };
            let note = if e.is_default { "  (default)" } else { "" };
            format!("{:width$}  {value}{note}", e.key)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok((json!({ "settings": entries.iter().map(entry_json).collect::<Vec<_>>() }), human))
}
