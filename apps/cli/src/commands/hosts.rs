//! `oximux hosts` and `oximux pair` — the client half of pairing, and the
//! named-host book the resolution order reads.
//!
//! `pair` and `hosts add` are one operation with two ergonomics: paste a link
//! and let the name be derived, or name it up front. Both enroll against the
//! host and write the same row.
//!
//! Nothing in here prints a ticket, a seed, or a session token. The only
//! credential-shaped value that reaches the terminal is the endpoint id, which
//! is a public key.

use std::time::Duration;

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, HostTarget};
use crate::client_identity;
use crate::hosts_store::{HostEntry, HostsFile, config_dir, endpoint_id_hex};
use crate::output::Failure;
use crate::remote_client;

/// How long `hosts ls --probe` gives each host to answer a `Ping`.
///
/// Short: this is a liveness column in a table, not a connection the user is
/// waiting to use. A host that needs longer than this to answer is, for the
/// purpose of "is it up right now", down.
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

fn host_json(h: &HostEntry, default: bool) -> Value {
    json!({
        "name": h.name,
        "endpoint_id": h.endpoint_id,
        "read_only": h.read_only,
        "protocol_version": h.protocol_version,
        "default": default,
    })
}

/// Enroll against a host and remember it.
///
/// The name defaults to the endpoint id's first eight hex characters — stable,
/// unique enough to distinguish a handful of hosts, and never a guess at what
/// the machine is called.
pub async fn pair(
    ticket_raw: &str,
    name: Option<String>,
    make_default: bool,
    timeout_secs: u64,
) -> Result<(Value, String), Failure> {
    let ticket = remote_client::parse_ticket(ticket_raw)?;
    let endpoint_hex = endpoint_id_hex(&ticket.endpoint_id);
    let name = name.unwrap_or_else(|| endpoint_hex[..8].to_string());
    if name.trim().is_empty() {
        return Err(Failure::new("usage", exit::USAGE, "a host name cannot be empty"));
    }

    let config = config_dir()?;
    // The key is minted BEFORE the dial: its public half is what the proof
    // commits to, so it has to exist to build the request at all. A dial that
    // then fails leaves an unused key, which the next attempt reuses — harmless
    // and better than minting a fresh identity per retry.
    let signer = client_identity::load_or_generate(&config, &name)?;
    let deadline = remote_client::deadline_in(Duration::from_secs(timeout_secs.max(1)));
    let transport = remote_client::dial(ticket.endpoint_id, deadline).await?;

    // Versions BEFORE the credential. A host this CLI cannot speak to must say
    // "update the older side" rather than refuse a proof it could not have
    // understood — and the ticket is one-time, so a proof spent against an
    // incompatible host is a ticket the user has to re-mint for nothing.
    // The ack doubles as the version `hosts ls` shows, so this costs no extra
    // round trip.
    let ack = remote_client::hello(transport.as_ref(), deadline).await?;

    let device_name = device_name();
    remote_client::register(transport.as_ref(), &signer, &ticket, &device_name, deadline).await?;

    let protocol_version = Some(ack.protocol_version);
    let mut hosts = HostsFile::load(&config)?;
    let first = hosts.entries.is_empty();
    hosts.upsert(HostEntry {
        name: name.clone(),
        endpoint_id: endpoint_hex.clone(),
        read_only: false,
        protocol_version,
    });
    // The first host paired becomes the default: a laptop with exactly one
    // remote host should not need `--host` on every call. Later ones do not
    // silently steal it.
    if make_default || first {
        hosts.default = Some(name.clone());
    }
    hosts.save(&config)?;

    let is_default = hosts.default.as_deref() == Some(name.as_str());
    let human = format!(
        "paired with {name} as \"{device_name}\"{}\ntry it with `oximux --host {name} ls`",
        if is_default { " (now the default host)" } else { "" }
    );
    Ok((host_json(hosts.get(&name).expect("just written"), is_default), human))
}

/// The label this machine appears under in the host's paired-devices list.
/// The hostname, because that is what a person scanning the list will
/// recognise; a generic "cli" would make three laptops indistinguishable.
fn device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            hostname_from_uname().filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "unknown".into());
    format!("{host} (cli)")
}

#[cfg(unix)]
fn hostname_from_uname() -> Option<String> {
    // `uname -n` rather than a crate: one syscall's worth of value, and the
    // CLI's dependency list is a shipped artifact.
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(not(unix))]
fn hostname_from_uname() -> Option<String> {
    None
}

pub async fn ls(probe: bool, timeout_secs: u64) -> Result<(Value, String), Failure> {
    let config = config_dir()?;
    let hosts = HostsFile::load(&config)?;
    if hosts.entries.is_empty() {
        return Ok((
            json!([]),
            "no paired hosts — `oximux pair <ticket>` adds one\n\
             (with none, every verb talks to this machine's own host)"
                .to_string(),
        ));
    }

    // Probed concurrently, like the fleet fan-out: a sequential loop costs
    // N × the timeout, which on a fleet of any size turns a status table into
    // a wait.
    let mut probes = std::collections::HashMap::new();
    if probe {
        let mut tasks = Vec::new();
        for entry in hosts.entries.clone() {
            let config = config.clone();
            tasks.push(tokio::spawn(async move {
                let name = entry.name.clone();
                (name, reachable(&config, &entry, timeout_secs).await)
            }));
        }
        for task in tasks {
            if let Ok((name, state)) = task.await {
                probes.insert(name, state);
            }
        }
    }

    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for entry in &hosts.entries {
        let is_default = hosts.default.as_deref() == Some(entry.name.as_str());
        let reach = if probe {
            // A missing entry means the probe task itself died — reported as
            // unknown rather than as "down", which would be a claim about the
            // host that this run has no evidence for.
            match probes.get(&entry.name).copied().flatten() {
                Some(true) => "up",
                Some(false) => "down",
                None => "?",
            }
        } else {
            "-"
        };
        let mut row = host_json(entry, is_default);
        if probe {
            row["reachable"] = json!(reach == "up");
        }
        rows.push(row);
        lines.push(format!(
            "{}{}  {}  {}  v{}  {}",
            if is_default { "*" } else { " " },
            entry.name,
            if entry.read_only { "read-only" } else { "read-write" },
            reach,
            entry.protocol_version.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            &entry.endpoint_id[..8],
        ));
    }
    Ok((json!(rows), lines.join("\n")))
}

/// Whether a host answers right now. `None` means the check could not be made
/// (a malformed row), which is deliberately distinct from "it said nothing".
async fn reachable(
    config: &std::path::Path,
    entry: &HostEntry,
    timeout_secs: u64,
) -> Option<bool> {
    let secs = timeout_secs.min(PROBE_TIMEOUT.as_secs()).max(1);
    match Client::connect_remote(config, entry, secs).await {
        Ok(client) => Some(matches!(client.call(Request::Ping).await, Ok(Response::Pong))),
        Err(f) if f.code == "endpoint-id" => None,
        Err(_) => Some(false),
    }
}

/// Forget a host: unpair host-side first (best effort), then drop the row and
/// its key.
///
/// Order matters. Unpairing needs the key, so the local erase has to come
/// second — and it happens even when the host cannot be reached, because a
/// laptop that has finished with a host must be able to say so without the
/// host's cooperation. The consequence is stated to the user rather than
/// hidden: a host that was down still lists this device until someone clears
/// it there.
pub async fn rm(name: &str, timeout_secs: u64) -> Result<(Value, String), Failure> {
    let config = config_dir()?;
    let mut hosts = HostsFile::load(&config)?;
    let Some(entry) = hosts.get(name).cloned() else {
        return Err(Failure::new("unknown-host", exit::USAGE, format!("no host named `{name}`"))
            .with_steps(["`oximux hosts ls` shows what is paired".into()]));
    };

    let unpaired = match Client::connect_remote(&config, &entry, timeout_secs).await {
        Ok(client) => matches!(client.call(Request::Unpair).await, Ok(Response::Ack)),
        Err(_) => false,
    };

    hosts.remove(name);
    hosts.save(&config)?;
    client_identity::forget(&config, name);

    let human = if unpaired {
        format!("removed {name}; the host cleared this device too")
    } else {
        format!(
            "removed {name} locally — the host could not be reached, so it may still list this \
             device\nclear it there with `oximux pair-ls` / `pair-rm` when it is back"
        )
    };
    Ok((json!({ "removed": name, "unpaired_host_side": unpaired }), human))
}

pub fn set_default(name: &str) -> Result<(Value, String), Failure> {
    let config = config_dir()?;
    let mut hosts = HostsFile::load(&config)?;
    if hosts.get(name).is_none() {
        return Err(Failure::new("unknown-host", exit::USAGE, format!("no host named `{name}`"))
            .with_steps(["`oximux hosts ls` shows what is paired".into()]));
    }
    hosts.default = Some(name.to_string());
    hosts.save(&config)?;
    Ok((json!({ "default": name }), format!("{name} is now the default host")))
}

/// `ls --all-hosts`: every paired host plus this machine, in one table.
///
/// **Never silently drops a host.** An unreachable one becomes a warning row
/// carrying the reason, and the verb still exits 0 with the partial results —
/// a fleet view that failed whenever any host was down would be unusable on a
/// fleet of more than about three. `--strict` inverts that for scripts, which
/// generally do want the failure.
pub async fn fleet_ls(
    strict: bool,
    timeout_secs: u64,
    dir: Option<std::path::PathBuf>,
) -> Result<(Value, String), Failure> {
    let config = config_dir()?;
    let hosts = HostsFile::load(&config)?;

    // Bounded concurrency: a fleet is small, and one round trip each means the
    // whole view costs about as long as its slowest host rather than their sum.
    let mut tasks = Vec::new();
    for entry in hosts.entries.clone() {
        let config = config.clone();
        // The name is captured OUT here as well as inside: a task that panics
        // yields a `JoinError` carrying nothing of the host, and a row labelled
        // "?" in a view whose contract is "never silently drop a host" is only
        // half of keeping that promise.
        let name = entry.name.clone();
        let handle = tokio::spawn(async move {
            let client = Client::connect_remote(&config, &entry, timeout_secs).await?;
            crate::commands::sessions::rows(&client).await
        });
        tasks.push((name, handle));
    }
    // The local host too — a fleet view that omitted the machine you are on
    // would be a strange kind of "all".
    let local = async {
        let client = Client::connect(dir, timeout_secs).await?;
        crate::commands::sessions::rows(&client).await
    }
    .await;

    // Gathered first, rendered second. A closure that borrowed all three
    // accumulators would conflict with the loop that also pushes warnings, and
    // splitting the two phases is clearer than threading them through.
    let mut gathered: Vec<(String, Result<Vec<Value>, Failure>)> =
        vec![(HostTarget::Local.label().to_string(), local)];
    for (name, handle) in tasks {
        let result = match handle.await {
            Ok(result) => result,
            // A panicked task is a bug, not an unreachable host — but it still
            // must not vanish from a view whose contract is "never silently
            // drop a host", and it must still say *which* host.
            Err(e) => Err(Failure::new("task", exit::ERROR, format!("host task failed: {e}"))),
        };
        gathered.push((name, result));
    }

    let mut all = Vec::new();
    let mut lines = Vec::new();
    let mut warnings = Vec::new();
    for (host, result) in gathered {
        match result {
            Ok(rows) => {
                for mut row in rows {
                    row["host"] = json!(host);
                    lines.push(format!(
                        "{host:<12} {}  {}",
                        row["session_id"].as_str().unwrap_or("?"),
                        row["title"].as_str().unwrap_or("")
                    ));
                    all.push(row);
                }
            }
            Err(f) => {
                warnings.push(format!("! {host:<12} unreachable — {}", f.message));
                all.push(json!({ "host": host, "unreachable": true, "error": f.message }));
            }
        }
    }

    let unreachable = warnings.len();
    if strict && unreachable > 0 {
        return Err(Failure::new(
            "partial",
            exit::UNREACHABLE,
            format!("{unreachable} host(s) unreachable"),
        )
        .with_steps(warnings));
    }
    let mut human = lines.join("\n");
    if !warnings.is_empty() {
        if !human.is_empty() {
            human.push('\n');
        }
        human.push_str(&warnings.join("\n"));
    }
    if human.is_empty() {
        human = "no sessions on any host".into();
    }
    Ok((json!(all), human))
}
