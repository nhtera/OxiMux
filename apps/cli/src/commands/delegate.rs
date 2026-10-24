//! `oximux delegate spawn|children|cancel` and `oximux whoami`: an agent
//! starting helper agents within the host's caps, and seeing where they stand.
//!
//! Online verbs (they route like `run`), unlike the offline `agent` group. The
//! host decides everything that matters — whether the caller may spawn, under
//! which parent, how deep and wide the tree may grow — and its refusals end
//! with what to do instead, so they are printed as written.

use oximux_remote_proto::lineage::{
    LineageStateWire, LineageWire, NotifyParentWire, SpawnAgentReq, SpawnCwdWire, SpawnParentWire,
    SpawnPostureWire, WhoamiWire,
};
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::{NotifyArg, PostureArg, exit};
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// What `delegate spawn` was given.
pub struct SpawnArgs {
    pub agent: String,
    pub model: Option<String>,
    pub posture: PostureArg,
    pub notify: NotifyArg,
    pub allow_parent_permits: bool,
    pub cwd: String,
    pub parent: Option<String>,
    pub request_id: Option<String>,
    pub prompt: Vec<String>,
}

/// `--cwd` as the wire takes it: `inherit`, `worktree`, `worktree:NAME`, or a
/// path (checked by the host against the parent's directory).
pub fn parse_cwd(text: &str) -> SpawnCwdWire {
    match text {
        "inherit" => SpawnCwdWire::Inherit,
        "worktree" => SpawnCwdWire::NewWorktree { name: None },
        _ => match text.strip_prefix("worktree:") {
            Some(name) if !name.trim().is_empty() => {
                SpawnCwdWire::NewWorktree { name: Some(name.trim().to_string()) }
            }
            _ => SpawnCwdWire::Path(text.to_string()),
        },
    }
}

pub async fn spawn(client: &Client, args: SpawnArgs) -> Result<(Value, String), Failure> {
    let prompt = args.prompt.join(" ");
    if prompt.trim().is_empty() {
        return Err(Failure::new("usage", exit::USAGE, "the child needs a prompt"));
    }
    let req = SpawnAgentReq {
        request_id: args.request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        parent: match args.parent {
            Some(id) => SpawnParentWire::Session(id),
            None => SpawnParentWire::Caller,
        },
        agent_id: args.agent,
        model: args.model,
        prompt,
        cwd: parse_cwd(&args.cwd),
        output_schema: None,
        notify_parent: match args.notify {
            NotifyArg::Auto => NotifyParentWire::Auto,
            NotifyArg::Stuck => NotifyParentWire::Stuck,
            NotifyArg::Off => NotifyParentWire::Off,
        },
        allow_parent_permits: args.allow_parent_permits,
        posture: posture_to_wire(args.posture),
    };
    match client.call(Request::SpawnAgent(req)).await? {
        Response::AgentSpawned(r) => {
            let data = json!({
                "session_id": r.session_id,
                "parent_id": r.parent_id,
                "root_id": r.root_id,
                "depth": r.depth,
                "replayed": r.replayed,
            });
            let human = if r.replayed {
                format!("already spawned {} (same request id)", r.session_id)
            } else {
                format!("spawned {} under {} (depth {})", r.session_id, r.parent_id, r.depth)
            };
            Ok((data, human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("SpawnAgent", &other)),
    }
}

pub async fn children(client: &Client, of: Option<String>, recursive: bool) -> Result<(Value, String), Failure> {
    let of = match of {
        Some(of) => of,
        None => match whoami_wire(client).await?.session {
            Some(own) => own,
            None => {
                return Err(Failure::new(
                    "usage",
                    exit::USAGE,
                    "name whose children to list with --of (this caller has no session of its own)",
                ));
            }
        },
    };
    match client.call(Request::ListChildren { of: of.clone(), recursive }).await? {
        Response::Children(rows) => {
            let human = if rows.is_empty() {
                format!("{of} has no children")
            } else {
                rows.iter().map(row_line).collect::<Vec<_>>().join("\n")
            };
            Ok((Value::Array(rows.iter().map(row_json).collect()), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("ListChildren", &other)),
    }
}

pub async fn cancel(client: &Client, id: &str, cascade: bool) -> Result<(Value, String), Failure> {
    match client.call(Request::CancelAgent { id: id.to_string(), cascade }).await? {
        Response::AgentsCancelled { ended } => {
            let human = if ended.is_empty() {
                format!("{id} was already ended")
            } else {
                format!("ended {}", ended.join(", "))
            };
            Ok((json!({ "ended": ended }), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("CancelAgent", &other)),
    }
}

pub async fn whoami(client: &Client) -> Result<(Value, String), Failure> {
    let me = whoami_wire(client).await?;
    let mut human = match &me.session {
        Some(session) => format!("{}({session})", capitalize(&me.scope)),
        None => capitalize(&me.scope),
    };
    if me.session.is_some() {
        human.push_str(&format!("\ndepth {}", me.depth));
    }
    match (&me.spawn_allowed, &me.reason) {
        (true, _) => human.push_str("\nmay spawn agents"),
        (false, Some(reason)) => human.push_str(&format!("\nmay not spawn: {reason}")),
        (false, None) => human.push_str("\nmay not spawn"),
    }
    let installed: Vec<&str> = me.agents.iter().filter(|a| a.installed).map(|a| a.agent_id.as_str()).collect();
    if !installed.is_empty() {
        human.push_str(&format!("\nagents: {}", installed.join(", ")));
    }
    let data = json!({
        "scope": me.scope,
        "session": me.session,
        "depth": me.depth,
        "root_id": me.root_id,
        "spawn_allowed": me.spawn_allowed,
        "reason": me.reason,
        "agents": me.agents.iter().map(|a| json!({
            "agent_id": a.agent_id,
            "display": a.display,
            "default_model": a.default_model,
            "models": a.models,
            "postures": a.postures.iter().map(|p| posture_text(*p)).collect::<Vec<_>>(),
            "installed": a.installed,
        })).collect::<Vec<_>>(),
    });
    Ok((data, human))
}

async fn whoami_wire(client: &Client) -> Result<WhoamiWire, Failure> {
    match client.call(Request::Whoami).await? {
        Response::Whoami(me) => Ok(me),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("Whoami", &other)),
    }
}

fn posture_to_wire(p: PostureArg) -> SpawnPostureWire {
    match p {
        PostureArg::Default => SpawnPostureWire::Default,
        PostureArg::Review => SpawnPostureWire::Review,
        PostureArg::Verify => SpawnPostureWire::Verify,
    }
}

fn posture_text(p: SpawnPostureWire) -> &'static str {
    match p {
        SpawnPostureWire::Default => "default",
        SpawnPostureWire::Review => "review",
        SpawnPostureWire::Verify => "verify",
    }
}

fn state_text(s: LineageStateWire) -> &'static str {
    match s {
        LineageStateWire::Reserved => "starting",
        LineageStateWire::Active => "working",
        LineageStateWire::Parked => "idle",
        LineageStateWire::Ended => "ended",
    }
}

fn row_line(row: &LineageWire) -> String {
    let indent = "  ".repeat(row.depth.saturating_sub(1) as usize);
    format!("{indent}{}  {}  {}", row.session_id, state_text(row.state), posture_text(row.posture))
}

fn row_json(row: &LineageWire) -> Value {
    json!({
        "session_id": row.session_id,
        "parent_id": row.parent_id,
        "root_id": row.root_id,
        "depth": row.depth,
        "state": state_text(row.state),
        "posture": posture_text(row.posture),
        "created_at": row.created_at,
    })
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_forms_parse() {
        assert_eq!(parse_cwd("inherit"), SpawnCwdWire::Inherit);
        assert_eq!(parse_cwd("worktree"), SpawnCwdWire::NewWorktree { name: None });
        assert_eq!(parse_cwd("worktree:review"), SpawnCwdWire::NewWorktree { name: Some("review".into()) });
        assert_eq!(parse_cwd("worktree:"), SpawnCwdWire::Path("worktree:".into()));
        assert_eq!(parse_cwd("crates/agents"), SpawnCwdWire::Path("crates/agents".into()));
    }
}
