//! `oximux worktree` — create, list, and remove project worktrees. The host
//! derives every on-disk location; this side only names a project (by the root
//! path it was handed) and a slug, and removes by listed id, never by path.

use std::path::PathBuf;

use oximux_remote_proto::messages::WorktreeWire;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// `--project`, else the invoker's own directory — the natural "this project".
fn resolve_project(project: Option<PathBuf>) -> Result<String, Failure> {
    let dir = match project {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    Ok(dir.to_string_lossy().into_owned())
}

fn wire_json(row: &WorktreeWire) -> Value {
    json!({
        "id": row.id,
        "project_path": row.project_path,
        "name": row.name,
        "slug": row.slug,
        "branch": row.branch,
        "path": row.path,
    })
}

pub async fn create(
    client: &Client,
    slug: &str,
    project: Option<PathBuf>,
) -> Result<(Value, String), Failure> {
    let project_path = resolve_project(project)?;
    let reply = client
        .call(Request::CreateWorktree { project_path, slug: slug.into() })
        .await?;
    match reply {
        Response::WorktreeCreated(row) => {
            let human = format!(
                "created {} on branch {}\n{}\nstart an agent there with `oximux run --cwd {} \"…\"`",
                row.slug, row.branch, row.path, row.path
            );
            Ok((wire_json(&row), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("CreateWorktree", &other)),
    }
}

pub async fn ls(client: &Client, project: Option<PathBuf>) -> Result<(Value, String), Failure> {
    // `--project` narrows; its absence lists every project's worktrees.
    let project_path = project.map(|p| p.to_string_lossy().into_owned());
    let reply = client.call(Request::ListWorktrees { project_path }).await?;
    let rows = match reply {
        Response::Worktrees(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListWorktrees", &other)),
    };
    let human = if rows.is_empty() {
        "no worktrees".to_string()
    } else {
        rows.iter()
            .map(|r| format!("{}  {}  ({})  {}", r.id, r.slug, r.branch, r.path))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(rows.iter().map(wire_json).collect::<Vec<_>>()), human))
}

pub async fn rm(client: &Client, id: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::RemoveWorktree { id: id.into() }).await? {
        Response::Ack => Ok((json!({ "removed": id }), format!("removed {id}"))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("RemoveWorktree", &other)),
    }
}
