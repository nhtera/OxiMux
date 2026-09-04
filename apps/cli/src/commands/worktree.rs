//! `oximux worktree` — create, list, and remove project worktrees. The host
//! derives every on-disk location; this side only names a project (by the root
//! path it was handed) and a slug, and removes by listed id, never by path.

use std::path::{Path, PathBuf};

use oximux_core::WorkPhase;
use oximux_remote_proto::messages::{WorktreeProgressWire, WorktreeWire};
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
    let reply = client.call(Request::ListWorktrees { project_path: project_path.clone() }).await?;
    let rows = match reply {
        Response::Worktrees(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListWorktrees", &other)),
    };
    let progress = list_progress(client, project_path).await;
    let human = if rows.is_empty() {
        "no worktrees".to_string()
    } else {
        rows.iter()
            .map(|r| {
                let p = progress.iter().find(|s| s.id == r.id);
                let phase = p
                    .and_then(|s| WorkPhase::parse(&s.phase))
                    .map(|p| format!("  [{}]", p.as_str()))
                    .unwrap_or_default();
                let comment = match p.map(|s| s.comment.as_str()).unwrap_or("") {
                    "" => String::new(),
                    c => format!("  {c}"),
                };
                format!("{}  {}  ({}){phase}  {}{comment}", r.id, r.slug, r.branch, r.path)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let json_rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = wire_json(r);
            let p = progress.iter().find(|s| s.id == r.id);
            // Always present in JSON, even when unset: a consumer testing
            // `.comment` should not have to distinguish "absent key" from
            // "nothing said". The human listing omits them; a script cannot.
            v["comment"] = json!(p.map(|s| s.comment.as_str()).unwrap_or(""));
            v["phase"] = json!(p.map(|s| s.phase.as_str()).unwrap_or(""));
            v
        })
        .collect();
    Ok((json!(json_rows), human))
}

/// The progress sidecar for a listing, or empty if the host cannot serve it.
///
/// **Degrades rather than failing.** `ListWorktreeProgress` is a v21 verb; a
/// v16–v20 host answers it with an error, and an older `worktree ls` that
/// still works is a far better outcome than one that stops working against a
/// host it has always been able to list. The columns simply do not appear.
async fn list_progress(client: &Client, project_path: Option<String>) -> Vec<WorktreeProgressWire> {
    match client.call(Request::ListWorktreeProgress { project_path }).await {
        Ok(Response::WorktreeProgress(rows)) => rows,
        _ => Vec::new(),
    }
}

/// Set a worktree's progress line and/or phase.
///
/// Refuses a call that sets nothing: `worktree set <id>` with no flags almost
/// certainly means the flags were forgotten, and answering "ok" to it would
/// report a write that never happened.
///
/// The phase is validated here as well as host-side. The host's check is the
/// trust boundary (it serves callers that are not this CLI); this one exists
/// so a typo fails immediately, with the vocabulary in the message, instead of
/// after a connection attempt. Both read the same `WorkPhase` list.
pub async fn set(
    client: &Client,
    id: &str,
    comment: Option<String>,
    phase: Option<String>,
) -> Result<(Value, String), Failure> {
    if comment.is_none() && phase.is_none() {
        return Err(Failure::new(
            "usage",
            exit::USAGE,
            "nothing to set — pass --comment, --phase, or both".to_string(),
        ));
    }
    if let Some(raw) = phase.as_deref()
        && !raw.is_empty()
        && WorkPhase::parse(raw).is_none()
    {
        let known: Vec<&str> = WorkPhase::ALL.iter().map(|p| p.as_str()).collect();
        return Err(Failure::new(
            "usage",
            exit::USAGE,
            format!("unknown phase `{raw}` — expected one of: {}", known.join(", ")),
        ));
    }
    // Normalise before sending so the stored value is the canonical spelling
    // whatever case it was typed in — a listing should not show `In-Progress`
    // for one worktree and `in-progress` for the next.
    let phase = phase.map(|raw| match WorkPhase::parse(&raw) {
        Some(p) => p.as_str().to_string(),
        None => raw,
    });
    let reply = client
        .call(Request::SetWorktreeProgress {
            id: id.into(),
            comment: comment.clone(),
            phase: phase.clone(),
        })
        .await?;
    match reply {
        Response::Ack => {
            let mut parts = Vec::new();
            if let Some(c) = &comment {
                parts.push(if c.is_empty() {
                    "comment cleared".to_string()
                } else {
                    format!("comment: {c}")
                });
            }
            if let Some(p) = &phase {
                parts.push(if p.is_empty() {
                    "phase cleared".to_string()
                } else {
                    format!("phase: {p}")
                });
            }
            Ok((
                json!({ "id": id, "comment": comment, "phase": phase }),
                format!("{id}  {}", parts.join("  ")),
            ))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("SetWorktreeProgress", &other)),
    }
}

/// Removal is idempotent by design — the service treats an id that is already
/// gone as the caller's goal state reached — and the `Ack` is the same either
/// way, so this cannot report *whether* anything was there. It therefore states
/// the postcondition rather than claiming an action: `removed {id}` was an
/// affirmative claim about a worktree that may never have existed, which made a
/// typo'd slug indistinguishable from a real cleanup for anything scripting it.
pub async fn rm(client: &Client, id: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::RemoveWorktree { id: id.into() }).await? {
        Response::Ack => Ok((
            json!({ "id": id, "state": "absent" }),
            format!("no worktree `{id}` remains"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("RemoveWorktree", &other)),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The canonicalisation `set` applies before sending, isolated so it can be
    /// tested without a host. A listing must not show `In-Progress` for one
    /// worktree and `in-progress` for the next.
    fn canonical(raw: &str) -> String {
        match WorkPhase::parse(raw) {
            Some(p) => p.as_str().to_string(),
            None => raw.to_string(),
        }
    }

    #[test]
    fn a_phase_is_stored_in_one_spelling_however_it_was_typed() {
        assert_eq!(canonical("In-Progress"), "in-progress");
        assert_eq!(canonical("DONE"), "done");
        assert_eq!(canonical(" todo "), "todo");
    }

    /// Every spelling the CLI accepts is one the host also accepts — the two
    /// validators read the same `WorkPhase::ALL`, and this pins that they
    /// cannot drift into a state where the CLI passes something the host
    /// refuses.
    #[test]
    fn every_accepted_phase_is_one_the_host_accepts() {
        for phase in WorkPhase::ALL {
            let sent = canonical(phase.as_str());
            assert_eq!(
                WorkPhase::parse(&sent),
                Some(phase),
                "the CLI must not send a spelling the host cannot parse"
            );
        }
    }
}
