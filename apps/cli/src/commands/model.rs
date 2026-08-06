//! `oximux model` / `oximux mode` — see and switch a session's model and
//! permission mode. A backend that fixes these at spawn may refuse the switch
//! when no desktop view can respawn the child; that refusal is surfaced, not
//! masked.

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

pub async fn ls(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    let choices = match client.call(Request::ListChoices { session_id: session.into() }).await? {
        Response::Choices(c) => c,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListChoices", &other)),
    };
    let mark = |current: &Option<String>, id: &str| {
        if current.as_deref() == Some(id) { "* " } else { "  " }
    };
    let mut lines = Vec::new();
    if choices.models.is_empty() {
        lines.push("no models advertised (the backend may not have reported yet)".to_string());
    } else {
        lines.push("models:".into());
        for m in &choices.models {
            lines.push(format!("{}{}  {}", mark(&choices.current_model, &m.id), m.id, m.label));
        }
    }
    if !choices.modes.is_empty() {
        lines.push("modes:".into());
        for m in &choices.modes {
            lines.push(format!("{}{}  {}", mark(&choices.current_mode, &m.id), m.id, m.label));
        }
    }
    let data = json!({
        "models": choices.models.iter().map(|m| json!({
            "id": m.id, "label": m.label, "description": m.description,
        })).collect::<Vec<_>>(),
        "modes": choices.modes.iter().map(|m| json!({
            "id": m.id, "label": m.label,
        })).collect::<Vec<_>>(),
        "current_model": choices.current_model,
        "current_mode": choices.current_mode,
    });
    Ok((data, lines.join("\n")))
}

pub async fn set_model(
    client: &Client,
    session: &str,
    model: &str,
) -> Result<(Value, String), Failure> {
    match client
        .call(Request::SetModel { session_id: session.into(), model: model.into() })
        .await?
    {
        Response::Ack => {
            Ok((json!({ "model": model }), format!("model set to {model}")))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("SetModel", &other)),
    }
}

pub async fn set_mode(
    client: &Client,
    session: &str,
    mode: &str,
) -> Result<(Value, String), Failure> {
    match client
        .call(Request::SetPermissionMode { session_id: session.into(), mode: mode.into() })
        .await?
    {
        Response::Ack => Ok((json!({ "mode": mode }), format!("mode set to {mode}"))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("SetPermissionMode", &other)),
    }
}
