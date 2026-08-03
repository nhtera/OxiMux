//! `oximux ls` and `oximux projects ls` — the host's sessions and projects.

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure};
use crate::output::Failure;

fn unexpected(what: &str, got: &Response) -> Failure {
    Failure::new(
        "protocol",
        crate::cli::exit::ERROR,
        format!("unexpected reply to {what}: {got:?}"),
    )
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListSessions).await? {
        Response::Sessions(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected("ListSessions", &other)),
    };
    let data = json!(rows
        .iter()
        .map(|s| json!({
            "session_id": s.session_id,
            "title": s.title,
            "model": s.model,
            "last_seq": s.last_seq,
            "awaiting_permission": s.awaiting_permission,
        }))
        .collect::<Vec<_>>());
    let human = if rows.is_empty() {
        "no sessions".to_string()
    } else {
        rows.iter()
            .map(|s| {
                let flag = if s.awaiting_permission { "  [awaiting permission]" } else { "" };
                let model = s.model.as_deref().unwrap_or("-");
                format!("{}  {}  ({model}){flag}", s.session_id, s.title)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((data, human))
}

pub async fn projects_ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListProjects).await? {
        Response::Projects(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected("ListProjects", &other)),
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
