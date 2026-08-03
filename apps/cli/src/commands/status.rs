//! `oximux status` — is a host there, and what is it running?

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure};
use crate::output::Failure;

pub async fn run(client: &Client) -> Result<(Value, String), Failure> {
    let sessions = match client.call(Request::ListSessions).await? {
        Response::Sessions(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => {
            return Err(Failure::new(
                "protocol",
                crate::cli::exit::ERROR,
                format!("unexpected reply to ListSessions: {other:?}"),
            ));
        }
    };
    let awaiting = sessions.iter().filter(|s| s.awaiting_permission).count();
    let data = json!({
        "reachable": true,
        "host": {
            "protocol_version": client.host_version,
            "min_compatible": client.host_min_compatible,
        },
        "sessions": { "total": sessions.len(), "awaiting_permission": awaiting },
    });
    let human = format!(
        "host reachable (protocol v{})\nsessions: {} total, {} awaiting permission",
        client.host_version,
        sessions.len(),
        awaiting
    );
    Ok((data, human))
}
