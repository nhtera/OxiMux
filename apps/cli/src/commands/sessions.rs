//! `oximux ls` and `oximux projects ls` — the host's sessions and projects.

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// One host's sessions as JSON rows — **the** definition of a session row.
///
/// Both `ls` and the fleet view render from this, so the two cannot disagree
/// about what a row contains; the fleet view only stamps a `host` key on top.
pub async fn rows(client: &Client) -> Result<Vec<Value>, Failure> {
    let rows = match client.call(Request::ListSessions).await? {
        Response::Sessions(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListSessions", &other)),
    };
    Ok(rows
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id,
                "title": s.title,
                "model": s.model,
                "last_seq": s.last_seq,
                "awaiting_permission": s.awaiting_permission,
            })
        })
        .collect())
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    // Through `rows`, not a second copy of the same `json!` — the fleet view
    // and this one must not be able to disagree about what a session row is.
    let rows = rows(client).await?;
    let human = if rows.is_empty() {
        "no sessions".to_string()
    } else {
        rows.iter()
            .map(|s| {
                let flag = if s["awaiting_permission"] == json!(true) {
                    "  [awaiting permission]"
                } else {
                    ""
                };
                let model = s["model"].as_str().unwrap_or("-");
                format!(
                    "{}  {}  ({model}){flag}",
                    s["session_id"].as_str().unwrap_or("?"),
                    s["title"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(rows), human))
}

pub async fn projects_ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListProjects).await? {
        Response::Projects(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListProjects", &other)),
    };
    let data = json!(rows
        .iter()
        .map(|p| json!({ "name": p.name, "path": p.path }))
        .collect::<Vec<_>>());
    let human = if rows.is_empty() {
        "no projects".to_string()
    } else {
        rows.iter().map(|p| format!("{}  {}", p.name, p.path)).collect::<Vec<_>>().join("\n")
    };
    Ok((data, human))
}
