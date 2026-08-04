//! `oximux run` — create a session (optionally inside a fresh worktree), send
//! the prompt, and either stay attached to the turn or background out with the
//! session id.

use std::path::PathBuf;

use oximux_remote_proto::messages::SendPromptReq;
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use super::attach::{Stop, StreamEnd, stream_session};
use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// Resolve the session's working directory: `--cwd`, else the invoker's own.
fn resolve_cwd(cwd: Option<PathBuf>) -> Result<String, Failure> {
    let dir = match cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    Ok(dir.to_string_lossy().into_owned())
}

/// A correlation id for the prompt. Uniqueness within one process's sends is
/// all the field promises; the pid seed keeps two concurrent CLIs distinct.
fn corr_id() -> u64 {
    u64::from(std::process::id()) << 16
}

pub struct RunArgs {
    pub prompt: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub worktree: Option<String>,
    /// A JSON Schema the final answer must satisfy (clap refuses it with
    /// `--bg`, which has no final answer to hold).
    pub output_schema: Option<String>,
    pub bg: bool,
}

pub async fn run(client: &Client, args: RunArgs, json_mode: bool) -> Result<(Value, String), Failure> {
    // Compiled BEFORE anything is spawned: a bad schema is the caller's typo,
    // and finding it after an agent has already started work would leave a
    // live session behind for a usage error.
    let schema = args
        .output_schema
        .as_deref()
        .map(crate::output_schema::OutputSchema::load)
        .transpose()?;
    let project_cwd = resolve_cwd(args.cwd)?;

    // `--worktree`: mint the worktree first; the session then opens inside it.
    // The cwd names the project — resolved to the root the host knows, so
    // invoking from a subdirectory works; the host derives the worktree's
    // real path and still validates the root itself.
    let (cwd, worktree) = match &args.worktree {
        Some(slug) => {
            let project_path = super::worktree::resolve_project_root(
                client,
                std::path::PathBuf::from(&project_cwd),
            )
            .await;
            let reply = client
                .call(Request::CreateWorktree { project_path, slug: slug.clone() })
                .await?;
            match reply {
                Response::WorktreeCreated(row) => (row.path.clone(), Some(row)),
                Response::Error(e) => return Err(rpc_failure(e)),
                other => return Err(unexpected_reply("CreateWorktree", &other)),
            }
        }
        None => (project_cwd, None),
    };

    let reply = client
        .call(Request::CreateSession { cwd: cwd.clone(), agent_id: args.agent.clone() })
        .await?;
    let session_id = match reply {
        Response::SessionCreated { session_id } => session_id,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("CreateSession", &other)),
    };

    // The model switch happens before the prompt so the turn runs on the asked-
    // for model. A refusal fails the verb loudly (the session exists — say so)
    // rather than running the prompt on a model the user did not pick.
    if let Some(model) = &args.model {
        match client
            .call(Request::SetModel { session_id: session_id.clone(), model: model.clone() })
            .await?
        {
            Response::Ack => {}
            Response::Error(e) => {
                return Err(rpc_failure(e).with_steps([
                    format!("the session {session_id} was created but keeps its default model"),
                    format!("send into it anyway with `oximux send {session_id} …`"),
                ]));
            }
            other => return Err(unexpected_reply("SetModel", &other)),
        }
    }

    match client
        .call(Request::SendPrompt(SendPromptReq {
            session_id: session_id.clone(),
            text: args.prompt.clone(),
            images: vec![],
            corr_id: corr_id(),
        }))
        .await?
    {
        Response::Ack => {}
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("SendPrompt", &other)),
    }

    let mut base = json!({
        "session_id": session_id,
        "cwd": cwd,
        "worktree": worktree.as_ref().map(|w| json!({ "id": w.id, "path": w.path, "branch": w.branch })),
    });
    if args.bg {
        // Accepted ≠ finished: the id is the handle for wait/attach.
        return Ok((
            base,
            format!("{session_id}\naccepted — watch with `oximux attach {session_id}` or `oximux wait {session_id} --until done`"),
        ));
    }

    if !json_mode {
        println!("session {session_id} — streaming (Ctrl+C detaches; the agent keeps running)");
    }
    // Follow from seq 0: the session is brand new, so the backlog IS the whole
    // history and the synthesized user bubble comes through too.
    match stream_session(client, &session_id, Some(0), json_mode, false, Stop::TurnEnded, None)
        .await?
    {
        StreamEnd::TurnEnded { is_error: false } => match schema {
            Some(schema) => {
                let value =
                    crate::output_schema::enforce(client, &session_id, &schema, json_mode).await?;
                let human = serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| value.to_string());
                base["output"] = value;
                Ok((base, human))
            }
            None => Ok((base, "✓ done".into())),
        },
        StreamEnd::TurnEnded { is_error: true } => Err(Failure::new(
            "turn-error",
            exit::ERROR,
            format!("the turn ended with an error (session {session_id})"),
        )),
        StreamEnd::Detached => Ok((
            base,
            format!("detached — the agent keeps running (session {session_id})"),
        )),
        _ => Err(Failure::new("protocol", exit::ERROR, "the stream ended unexpectedly")),
    }
}
