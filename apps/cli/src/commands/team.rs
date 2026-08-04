//! `oximux team` — open a multi-role run, report a role's outcome, read the
//! board.
//!
//! The run lives on the host, which is what makes `team status` meaningful
//! after the process that started the run is gone. Roles report from inside
//! their own sessions (`oximux team report …`), so the board converges without
//! anything polling the agents.

use oximux_remote_proto::messages::{
    TeamReportReq, TeamRoleStatusWire, TeamRoleWire, TeamRunCreateReq, TeamRoleSpecWire,
    TeamRunWire,
};
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// `name=prompt`, the repeatable `--role` form. Split on the FIRST `=` so a
/// prompt may contain them.
pub fn parse_role(spec: &str) -> Result<TeamRoleSpecWire, Failure> {
    let (name, prompt) = spec.split_once('=').ok_or_else(|| {
        Failure::new(
            "usage",
            exit::USAGE,
            format!("--role wants NAME=PROMPT; got `{spec}`"),
        )
        .with_steps(["e.g. --role backend=\"port the API to v2\"".into()])
    })?;
    if name.trim().is_empty() || prompt.trim().is_empty() {
        return Err(Failure::new(
            "usage",
            exit::USAGE,
            "--role wants a non-empty NAME and PROMPT".to_string(),
        ));
    }
    Ok(TeamRoleSpecWire { name: name.trim().to_string(), prompt: prompt.to_string() })
}

fn status_text(status: TeamRoleStatusWire) -> &'static str {
    match status {
        TeamRoleStatusWire::Running => "running",
        TeamRoleStatusWire::Done => "done",
        TeamRoleStatusWire::Failed => "failed",
    }
}

fn role_json(r: &TeamRoleWire) -> Value {
    json!({
        "name": r.name,
        "session_id": r.session_id,
        "status": status_text(r.status),
        "summary": r.summary,
        "updated_at": r.updated_at,
    })
}

fn run_json(run: &TeamRunWire) -> Value {
    json!({
        "id": run.id,
        "name": run.name,
        "cwd": run.cwd,
        "created_at": run.created_at,
        "closed": run.closed,
        "roles": run.roles.iter().map(role_json).collect::<Vec<_>>(),
    })
}

fn run_board(run: &TeamRunWire) -> String {
    let mut out = format!(
        "{}  {}  {}",
        run.id,
        run.name,
        if run.closed { "closed" } else { "open" }
    );
    for role in &run.roles {
        out.push_str(&format!("\n  {:<12} {}", role.name, status_text(role.status)));
        if let Some(session) = &role.session_id {
            out.push_str(&format!("  session {session}"));
        }
        if let Some(summary) = &role.summary {
            out.push_str(&format!("  — {summary}"));
        }
    }
    out
}

pub struct RunArgs {
    pub name: String,
    pub roles: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub agent: Option<String>,
    pub worktree_each: bool,
}

pub async fn run(client: &Client, args: RunArgs) -> Result<(Value, String), Failure> {
    let roles = args.roles.iter().map(|s| parse_role(s)).collect::<Result<Vec<_>, _>>()?;
    let cwd = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    let reply = client
        .call(Request::TeamRunCreate(TeamRunCreateReq {
            name: args.name,
            cwd: cwd.to_string_lossy().into_owned(),
            agent_id: args.agent,
            worktree_each: args.worktree_each,
            roles,
        }))
        .await?;
    match reply {
        Response::TeamRun(run) => {
            // Roles that failed to start are part of a normal reply, not an
            // error: the run exists, and the board says which roles are on it.
            let human = format!(
                "{}\nwatch it with `oximux team status --run {}`",
                run_board(&run),
                run.id
            );
            Ok((run_json(&run), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("TeamRunCreate", &other)),
    }
}

pub async fn report(
    client: &Client,
    run_id: &str,
    role: &str,
    ok: bool,
    summary: Option<String>,
) -> Result<(Value, String), Failure> {
    let reply = client
        .call(Request::TeamReport(TeamReportReq {
            run_id: run_id.into(),
            role: role.into(),
            ok,
            summary,
        }))
        .await?;
    match reply {
        Response::TeamRun(run) => Ok((run_json(&run), run_board(&run))),
        Response::Ack => Ok((json!({ "run_id": run_id, "role": role }), "reported".into())),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("TeamReport", &other)),
    }
}

pub async fn status(client: &Client, run_id: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::TeamStatus { run_id: run_id.into() }).await? {
        Response::TeamRun(run) => Ok((run_json(&run), run_board(&run))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("TeamStatus", &other)),
    }
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let runs = match client.call(Request::TeamList).await? {
        Response::TeamRuns(runs) => runs,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("TeamList", &other)),
    };
    let human = if runs.is_empty() {
        "no team runs".to_string()
    } else {
        runs.iter()
            .map(|run| {
                let open = run.roles.iter().filter(|r| r.status == TeamRoleStatusWire::Running).count();
                format!(
                    "{}  {}  {}/{} still running  {}",
                    run.id,
                    run.name,
                    open,
                    run.roles.len(),
                    run.created_at
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(runs.iter().map(run_json).collect::<Vec<_>>()), human))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_splits_on_the_first_equals() {
        let role = parse_role("backend=port the API to v2 (x=y)").expect("parses");
        assert_eq!(role.name, "backend");
        assert_eq!(role.prompt, "port the API to v2 (x=y)", "later `=` stay in the prompt");
    }

    #[test]
    fn a_role_without_an_equals_is_a_usage_error() {
        assert_eq!(parse_role("backend").unwrap_err().exit, exit::USAGE);
    }

    #[test]
    fn an_empty_half_is_a_usage_error() {
        assert_eq!(parse_role("=prompt").unwrap_err().exit, exit::USAGE);
        assert_eq!(parse_role("backend=").unwrap_err().exit, exit::USAGE);
    }
}
