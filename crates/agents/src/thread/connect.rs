//! Connection factory — turns a transport-tagged [`ConnectSpec`] into a live
//! `Box<dyn AgentConnection>` + its event receiver, so the app never names a
//! concrete connection type. The StreamJson arm drives Claude (today's path);
//! the ACP arm is a discoverable stub a later phase fills.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use anyhow::Result;

use super::claude_stream_json::ClaudeStreamJsonConnection;
use super::codex::CodexAppServerConnection;
use super::connection::AgentConnection;
use super::event::ThreadEvent;
use super::transport::Transport;

/// Everything the factory needs to open one chat connection. The `acp_*` fields
/// are only consulted by the `Acp` arm; the `StreamJson` (Claude) arm ignores
/// them.
#[derive(Debug, Clone)]
pub struct ConnectSpec {
    pub transport: Transport,
    pub cwd: PathBuf,
    /// `--model` selector (Claude alias / ACP model id). `None` = backend default.
    pub model: Option<String>,
    /// Resume a persisted session by id (`--resume` for Claude). `None` = fresh.
    pub resume_session_id: Option<String>,
    /// Permission/edit mode fixed at spawn (Claude `--permission-mode`).
    pub permission_mode: Option<String>,
    /// Reasoning effort fixed at spawn (Claude `--effort`).
    pub effort: Option<String>,
    /// The command that speaks ACP (only read by the `Acp` arm).
    pub acp_command: Option<String>,
    /// argv appended after `acp_command` (only read by the `Acp` arm).
    pub acp_args: Vec<String>,
}

impl ConnectSpec {
    /// Build a spec for Claude's native stream-json transport (the only
    /// transport wired today). Callers that already know the session's provider
    /// set `transport` themselves; this keeps the common Claude sites tidy.
    pub fn stream_json(
        cwd: PathBuf,
        model: Option<String>,
        resume_session_id: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
    ) -> Self {
        Self {
            transport: Transport::StreamJson,
            cwd,
            model,
            resume_session_id,
            permission_mode,
            effort,
            acp_command: None,
            acp_args: Vec::new(),
        }
    }
}

/// Open a chat connection for `spec.transport`, returning the boxed connection
/// and the `ThreadEvent` receiver the app drains. Semantics of the StreamJson
/// arm are identical to constructing `ClaudeStreamJsonConnection::spawn_resumed`
/// directly (the app's prior call).
pub fn connect(spec: ConnectSpec) -> Result<(Box<dyn AgentConnection>, Receiver<ThreadEvent>)> {
    match spec.transport {
        Transport::StreamJson => {
            let (conn, rx) = ClaudeStreamJsonConnection::spawn_resumed(
                &spec.cwd,
                spec.model.as_deref(),
                spec.resume_session_id.as_deref(),
                spec.permission_mode.as_deref(),
                spec.effort.as_deref(),
            )?;
            Ok((Box::new(conn), rx))
        }
        Transport::AppServer => {
            let (conn, rx) =
                CodexAppServerConnection::spawn(&spec.cwd, spec.model.as_deref())?;
            Ok((Box::new(conn), rx))
        }
        Transport::Acp => {
            anyhow::bail!("ACP transport not yet implemented")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_transport_is_a_clear_stub() {
        let spec = ConnectSpec {
            transport: Transport::Acp,
            cwd: PathBuf::from("."),
            model: None,
            resume_session_id: None,
            permission_mode: None,
            effort: None,
            acp_command: Some("gemini".into()),
            acp_args: vec!["--acp".into()],
        };
        // Can't `expect_err` — the Ok payload (`Box<dyn AgentConnection>`) isn't
        // `Debug`; match instead.
        match connect(spec) {
            Ok(_) => panic!("ACP arm must not connect yet"),
            Err(err) => assert!(err.to_string().contains("ACP"), "unexpected error: {err}"),
        }
    }
}
