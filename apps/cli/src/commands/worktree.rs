//! `oximux worktree` — create, list, and remove project worktrees. The host
//! derives every on-disk location; this side only names a project (by the root
//! path it was handed) and a slug, and removes by listed id, never by path.

use std::path::{Path, PathBuf};

use oximux_remote_proto::messages::WorktreeWire;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// The project root the host knows for `dir`: `dir` itself when listed, else
/// the **deepest** listed project root that is an ancestor of it — so running
/// from `proj/src/module` names `proj`, and a project nested inside another
/// resolves to the inner one. Falls back to `dir` verbatim when nothing
/// matches or the listing is unavailable, leaving the host's own exact-match
/// validation to answer — that check is the security boundary and stays
/// unchanged; this walk is UX on the client's side of it.
pub(crate) async fn resolve_project_root(client: &Client, dir: PathBuf) -> String {
    let roots: Vec<String> = match client.call(Request::ListProjects).await {
        Ok(Response::Projects(rows)) => rows.into_iter().map(|p| p.path).collect(),
        // Any refusal or transport failure: behave exactly as before this walk
        // existed. The verb's own RPC will surface the real error.
        _ => Vec::new(),
    };
    if let Some(root) = deepest_ancestor(&dir, &roots) {
        return root.to_string();
    }
    // Symlinked paths (macOS's /tmp → /private/tmp) and relative invocations:
    // one retry against the canonical form before giving up.
    if let Ok(real) = dir.canonicalize()
        && real != dir
        && let Some(root) = deepest_ancestor(&real, &roots)
    {
        return root.to_string();
    }
    dir.to_string_lossy().into_owned()
}

/// The longest (deepest) root in `roots` that is a path-component ancestor of
/// `dir` — `Path::starts_with`, never a string prefix, so `/work/proj` does
/// not claim `/work/proj-two`.
fn deepest_ancestor<'a>(dir: &Path, roots: &'a [String]) -> Option<&'a str> {
    roots
        .iter()
        .map(String::as_str)
        .filter(|root| dir.starts_with(root))
        .max_by_key(|root| Path::new(root).components().count())
}

/// `--project`, else the invoker's own directory — the natural "this project"
/// — resolved to the project root the host actually knows.
async fn resolve_project(client: &Client, project: Option<PathBuf>) -> Result<String, Failure> {
    let dir = match project {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    Ok(resolve_project_root(client, dir).await)
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
    let project_path = resolve_project(client, project).await?;
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
    // `--project` narrows (resolved to the root the host knows, so a
    // subdirectory works); its absence lists every project's worktrees.
    let project_path = match project {
        Some(p) => Some(resolve_project_root(client, p).await),
        None => None,
    };
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
