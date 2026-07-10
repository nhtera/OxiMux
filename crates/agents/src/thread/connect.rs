//! Connection factory — turns a transport-tagged [`ConnectSpec`] into a live
//! `Box<dyn AgentConnection>` + its event receiver, so the app never names a
//! concrete connection type. The StreamJson arm drives Claude (today's path);
//! the ACP arm is a discoverable stub a later phase fills.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::acp::AcpConnection;
use super::claude_stream_json::ClaudeStreamJsonConnection;
use super::codex::CodexAppServerConnection;
use super::connection::{AgentConnection, ModelChoice};
use super::event::ThreadEvent;
use super::transport::Transport;

/// Which backend a chat tab runs over, plus — for ACP — the external command to
/// spawn. Bundled so the launch/restore call-chain threads one value through its
/// many hops instead of three loose params, and so the view stores/persists one
/// field. The `acp_*` fields are meaningful only when `transport == Acp`
/// (Claude/Codex leave them empty). Gpui-free so it lives beside [`ConnectSpec`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatBackend {
    pub transport: Transport,
    /// The command that speaks ACP (e.g. `gemini`); `None` for non-ACP backends.
    pub acp_command: Option<String>,
    /// argv appended after `acp_command`; empty for non-ACP backends.
    pub acp_args: Vec<String>,
}

impl ChatBackend {
    /// The native Claude backend (stream-json, no ACP command) — the default a
    /// provider-agnostic entry point (e.g. the "New Agent Chat" action) opens.
    pub fn stream_json() -> Self {
        Self::default()
    }

    /// Human-facing provider name ("Claude"/"Codex"/"Agent") for captions.
    pub fn provider_display_name(&self) -> &'static str {
        self.transport.provider_display_name()
    }
}

impl From<Transport> for ChatBackend {
    /// A backend that carries only a transport (Claude/Codex — no ACP command).
    fn from(transport: Transport) -> Self {
        Self { transport, acp_command: None, acp_args: Vec::new() }
    }
}

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
    /// Build a spec from a resolved [`ChatBackend`] plus the per-session bits
    /// (cwd, model, resume id, permission mode, effort). One place carries the
    /// backend's transport + `acp_*` into the spec, so the fresh-launch and
    /// respawn call sites stay a single line each.
    pub fn for_backend(
        backend: &ChatBackend,
        cwd: PathBuf,
        model: Option<String>,
        resume_session_id: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
    ) -> Self {
        Self {
            transport: backend.transport,
            cwd,
            model,
            resume_session_id,
            permission_mode,
            effort,
            acp_command: backend.acp_command.clone(),
            acp_args: backend.acp_args.clone(),
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
            let (conn, rx) = CodexAppServerConnection::spawn(
                &spec.cwd,
                spec.model.as_deref(),
                spec.resume_session_id.as_deref(),
                spec.effort.as_deref(),
            )?;
            Ok((Box::new(conn), rx))
        }
        Transport::Acp => {
            let command = spec
                .acp_command
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| anyhow::anyhow!("ACP transport requires an acp_command"))?;
            let (conn, rx) = AcpConnection::spawn(command, &spec.acp_args, &spec.cwd)?;
            Ok((Box::new(conn), rx))
        }
    }
}

/// The model vocabulary a short-lived *catalog probe* pulls from an agent whose
/// models are only known after it spawns (Codex, ACP) — so the unbound *New
/// Agent* draft can offer a real model picker before the user commits. Claude's
/// models are static (declared in the roster) and never probed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbedCatalog {
    pub models: Vec<ModelChoice>,
    /// The backend's default/current model wire, so the picker preselects it.
    pub default_model: Option<String>,
}

/// Upper bound on how long a probe waits for the agent's handshake. Generous —
/// a cold `codex app-server` or an ACP agent behind an `npx` download / auth can
/// take a while — but bounded so a wedged agent can't hang the probe thread.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Open `spec`'s connection, wait for its `SessionInit` (which the transports
/// emit once the handshake has populated the model catalog — no prompt needed),
/// read the models + default, then drop the connection so its process is reaped
/// (Codex `reap()` / ACP worker shutdown). The live chat still spawns fresh on
/// first send; this is a throwaway catalog fetch.
///
/// Blocking (it waits on the handshake) — run it off the UI thread. Errors when
/// the agent can't spawn, the handshake fails, the process exits early, or the
/// probe times out; the caller renders those as "no models" rather than a picker.
pub fn probe_catalog(spec: ConnectSpec) -> Result<ProbedCatalog> {
    let (conn, rx) = connect(spec)?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("catalog probe timed out"))?;
        match rx.recv_timeout(remaining) {
            Ok(ThreadEvent::SessionInit { .. }) => {
                return Ok(ProbedCatalog { models: conn.models(), default_model: conn.default_model() });
            }
            Ok(ThreadEvent::Error(e)) => return Err(anyhow::anyhow!(e)),
            // Pre-init chatter is rare but harmless — keep waiting for init.
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => return Err(anyhow::anyhow!("catalog probe timed out")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("agent exited before session init"))
            }
        }
    }
    // `conn` drops here → the transport tears its subprocess down.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_transport_requires_a_command() {
        // The ACP arm needs an `acp_command` to know what to spawn; without one it
        // fails fast rather than spawning nothing. (A configured command would
        // spawn a real subprocess, so the happy path is covered by a live smoke
        // test, not a unit test.)
        let spec = ConnectSpec {
            transport: Transport::Acp,
            cwd: PathBuf::from("."),
            model: None,
            resume_session_id: None,
            permission_mode: None,
            effort: None,
            acp_command: None,
            acp_args: vec![],
        };
        // Can't `expect_err` — the Ok payload (`Box<dyn AgentConnection>`) isn't
        // `Debug`; match instead.
        match connect(spec) {
            Ok(_) => panic!("ACP arm must not connect without a command"),
            Err(err) => {
                assert!(err.to_string().contains("acp_command"), "unexpected error: {err}")
            }
        }
    }

    #[test]
    fn probe_catalog_fails_fast_without_a_command() {
        // `probe_catalog` opens a connection first, so an ACP spec with no command
        // surfaces the same fast failure — no process is spawned, nothing to wait
        // on. (The happy path spawns a real agent, so it's a live smoke test.)
        let spec = ConnectSpec {
            transport: Transport::Acp,
            cwd: PathBuf::from("."),
            model: None,
            resume_session_id: None,
            permission_mode: None,
            effort: None,
            acp_command: None,
            acp_args: vec![],
        };
        let err = probe_catalog(spec).expect_err("probe must fail without a command");
        assert!(err.to_string().contains("acp_command"), "unexpected error: {err}");
    }
}
