//! Client-side `terminal/*` request handlers for the ACP worker.
//!
//! ACP lets an agent create and drive a real terminal on the client (per the
//! client's advertised `terminal` capability): the agent asks the client to
//! spawn a command, polls its output, waits for exit, and can kill/release it —
//! then embeds the live terminal inline in a tool card (`ToolCallContent::Terminal`).
//!
//! OxiMux serves the five methods by delegating to an app-provided
//! [`AcpTerminalHost`]. The **app** owns the real PTY (reusing its existing
//! relay / portable-pty terminal machinery) and the inline `TerminalView`, so
//! this domain crate stays free of the UI/relay stack — dependency inversion,
//! mirroring the `AgentConnection`/`TerminalBackend` seams. The host is installed
//! once at app boot via [`super::install_terminal_host`]; when absent (tests,
//! headless) the client advertises `terminal:false` and these handlers reject
//! (an honest agent never calls them).

use std::path::PathBuf;

use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalExitStatus, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};
use futures::channel::oneshot;

/// JSON-RPC "internal error" — a failed spawn / output read on the host.
const INTERNAL_ERROR: i32 = -32603;
/// JSON-RPC "method not found" — returned when no terminal host is installed
/// (the client advertised `terminal:false`, so this path is only reachable via a
/// misbehaving agent).
const METHOD_NOT_FOUND: i32 = -32601;

/// What `terminal/create` needs to spawn a command. Deliberately gpui/pty-free
/// so [`AcpTerminalHost`] can be implemented in the app crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalSpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    /// Absolute working directory the agent requested (ACP requires it absolute);
    /// `None` = the host's session cwd.
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Cap on retained output bytes (ACP truncates from the front when exceeded);
    /// `None` = the host's default.
    pub output_byte_limit: Option<u64>,
}

/// A terminal's exit result crossing the host boundary → ACP `TerminalExitStatus`.
/// `exit_code` is `None` when the process was terminated by a signal (then
/// `signal` names it), matching the ACP contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalExitLite {
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

/// The current captured output of a terminal → ACP `TerminalOutputResponse`.
/// `exit_status` is `Some` only once the command has exited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalOutputLite {
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<TerminalExitLite>,
}

/// The app-provided backing for ACP embedded terminals. Implemented in the app
/// crate against its PTY/relay stack and injected once via
/// [`super::install_terminal_host`]. Every method is keyed by the client-minted
/// terminal id returned from [`create`](AcpTerminalHost::create).
pub trait AcpTerminalHost: Send + Sync {
    /// Spawn a command; return the client-minted terminal id (globally unique, so
    /// a single process-wide registry has no cross-session collisions).
    fn create(&self, spec: TerminalSpawnSpec) -> Result<String, String>;
    /// The output captured so far plus the exit status once the command has
    /// completed. Cheap + synchronous (reads the host's ring/grid).
    fn output(&self, terminal_id: &str) -> Result<TerminalOutputLite, String>;
    /// Resolve when the command exits. The host fires the sender on the PTY exit
    /// event, or immediately if the command already exited. A dropped sender (the
    /// terminal was released / the app is closing) resolves the receiver to
    /// `Err`, which the handler degrades to a default exit status.
    fn wait_for_exit(&self, terminal_id: &str) -> oneshot::Receiver<TerminalExitLite>;
    /// Kill the running command but keep the terminal valid — its output stays
    /// readable via [`output`](AcpTerminalHost::output) until `release`.
    fn kill(&self, terminal_id: &str) -> Result<(), String>;
    /// Kill (if still running) and free the terminal's resources. After this the
    /// id is invalid; the agent must have already embedded it if it wants the
    /// final output to keep showing.
    fn release(&self, terminal_id: &str) -> Result<(), String>;
}

/// Serve `terminal/create`: spawn the agent's command via the host.
pub(crate) fn create(
    host: &dyn AcpTerminalHost,
    req: &CreateTerminalRequest,
) -> Result<CreateTerminalResponse, Error> {
    let spec = TerminalSpawnSpec {
        command: req.command.clone(),
        args: req.args.clone(),
        cwd: req.cwd.clone(),
        env: req.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect(),
        output_byte_limit: req.output_byte_limit,
    };
    let id = host
        .create(spec)
        .map_err(|e| Error::new(INTERNAL_ERROR, format!("terminal/create: {e}")))?;
    Ok(CreateTerminalResponse::new(id))
}

/// Serve `terminal/output`: the output captured so far + exit status (if exited).
pub(crate) fn output(
    host: &dyn AcpTerminalHost,
    req: &TerminalOutputRequest,
) -> Result<TerminalOutputResponse, Error> {
    let out = host
        .output(req.terminal_id.0.as_ref())
        .map_err(|e| Error::new(INTERNAL_ERROR, format!("terminal/output: {e}")))?;
    let mut resp = TerminalOutputResponse::new(out.output, out.truncated);
    if let Some(exit) = out.exit_status {
        resp = resp.exit_status(exit_status_to_acp(&exit));
    }
    Ok(resp)
}

/// Serve `terminal/wait_for_exit`: block until the command exits. Async because
/// it awaits the host's exit signal; a dropped signal degrades to a default
/// (empty) exit status rather than hanging the agent's turn.
pub(crate) async fn wait_for_exit(
    host: &dyn AcpTerminalHost,
    req: &WaitForTerminalExitRequest,
) -> Result<WaitForTerminalExitResponse, Error> {
    let rx = host.wait_for_exit(req.terminal_id.0.as_ref());
    let exit = rx.await.unwrap_or_default();
    Ok(WaitForTerminalExitResponse::new(exit_status_to_acp(&exit)))
}

/// Serve `terminal/kill`: terminate the command, keep the terminal valid.
pub(crate) fn kill(
    host: &dyn AcpTerminalHost,
    req: &KillTerminalRequest,
) -> Result<KillTerminalResponse, Error> {
    host.kill(req.terminal_id.0.as_ref())
        .map_err(|e| Error::new(INTERNAL_ERROR, format!("terminal/kill: {e}")))?;
    Ok(KillTerminalResponse::new())
}

/// Serve `terminal/release`: kill (if running) and free the terminal.
pub(crate) fn release(
    host: &dyn AcpTerminalHost,
    req: &ReleaseTerminalRequest,
) -> Result<ReleaseTerminalResponse, Error> {
    host.release(req.terminal_id.0.as_ref())
        .map_err(|e| Error::new(INTERNAL_ERROR, format!("terminal/release: {e}")))?;
    Ok(ReleaseTerminalResponse::new())
}

/// The JSON-RPC error returned when a terminal method is invoked but no host is
/// installed (the client never advertised `terminal:true`, so this is defensive).
pub(crate) fn no_host_error() -> Error {
    Error::new(METHOD_NOT_FOUND, "terminal/* is not supported by this client")
}

/// Map the host's lite exit into the ACP `TerminalExitStatus` wire shape.
fn exit_status_to_acp(exit: &TerminalExitLite) -> TerminalExitStatus {
    let mut status = TerminalExitStatus::default();
    status.exit_code = exit.exit_code;
    status.signal = exit.signal.clone();
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A host that records calls and returns canned results, so the handlers'
    /// request→host→response translation is verified without a real PTY.
    #[derive(Default)]
    struct FakeHost {
        created: Mutex<Vec<TerminalSpawnSpec>>,
        killed: Mutex<Vec<String>>,
        released: Mutex<Vec<String>>,
        fail: bool,
    }

    impl AcpTerminalHost for FakeHost {
        fn create(&self, spec: TerminalSpawnSpec) -> Result<String, String> {
            if self.fail {
                return Err("spawn boom".into());
            }
            self.created.lock().unwrap().push(spec);
            Ok("term-1".into())
        }
        fn output(&self, _terminal_id: &str) -> Result<TerminalOutputLite, String> {
            if self.fail {
                return Err("read boom".into());
            }
            Ok(TerminalOutputLite {
                output: "hello\n".into(),
                truncated: true,
                exit_status: Some(TerminalExitLite { exit_code: Some(0), signal: None }),
            })
        }
        fn wait_for_exit(&self, _terminal_id: &str) -> oneshot::Receiver<TerminalExitLite> {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(TerminalExitLite { exit_code: None, signal: Some("SIGTERM".into()) });
            rx
        }
        fn kill(&self, terminal_id: &str) -> Result<(), String> {
            self.killed.lock().unwrap().push(terminal_id.to_string());
            Ok(())
        }
        fn release(&self, terminal_id: &str) -> Result<(), String> {
            self.released.lock().unwrap().push(terminal_id.to_string());
            Ok(())
        }
    }

    #[test]
    fn create_forwards_spec_and_returns_minted_id() {
        let host = FakeHost::default();
        let req = CreateTerminalRequest::new("sess", "cargo")
            .args(vec!["test".into()])
            .output_byte_limit(4096u64);
        let resp = create(&host, &req).expect("create ok");
        assert_eq!(resp.terminal_id.0.as_ref(), "term-1");
        let created = host.created.lock().unwrap();
        assert_eq!(created[0].command, "cargo");
        assert_eq!(created[0].args, vec!["test".to_string()]);
        assert_eq!(created[0].output_byte_limit, Some(4096));
    }

    #[test]
    fn output_maps_text_truncation_and_exit_status() {
        let host = FakeHost::default();
        let req = TerminalOutputRequest::new("sess", "term-1");
        let resp = output(&host, &req).expect("output ok");
        assert_eq!(resp.output, "hello\n");
        assert!(resp.truncated);
        let exit = resp.exit_status.expect("exited");
        assert_eq!(exit.exit_code, Some(0));
        assert!(exit.signal.is_none());
    }

    #[test]
    fn wait_for_exit_resolves_with_signal() {
        let host = FakeHost::default();
        let req = WaitForTerminalExitRequest::new("sess", "term-1");
        let resp = futures::executor::block_on(wait_for_exit(&host, &req)).expect("wait ok");
        assert_eq!(resp.exit_status.exit_code, None);
        assert_eq!(resp.exit_status.signal.as_deref(), Some("SIGTERM"));
    }

    #[test]
    fn kill_and_release_forward_the_terminal_id() {
        let host = FakeHost::default();
        kill(&host, &KillTerminalRequest::new("sess", "term-1")).expect("kill ok");
        release(&host, &ReleaseTerminalRequest::new("sess", "term-1")).expect("release ok");
        assert_eq!(host.killed.lock().unwrap().as_slice(), ["term-1"]);
        assert_eq!(host.released.lock().unwrap().as_slice(), ["term-1"]);
    }

    #[test]
    fn host_error_becomes_internal_json_rpc_error() {
        let host = FakeHost { fail: true, ..Default::default() };
        let err = create(&host, &CreateTerminalRequest::new("s", "x")).unwrap_err();
        assert_eq!(i32::from(err.code), INTERNAL_ERROR);
        let err = output(&host, &TerminalOutputRequest::new("s", "t")).unwrap_err();
        assert_eq!(i32::from(err.code), INTERNAL_ERROR);
    }
}
