//! The lazy host client: nothing here runs until a verb actually needs the
//! host, so `--help`, `version` and `agent-context` never touch the socket.
//!
//! Every connection claims a scope: `OXIMUX_SESSION_ID` in the environment
//! (injected into agent-spawned processes) narrows the connection to that one
//! session; its absence is an operator at their own keyboard. The host
//! enforces the claim — this side merely never omits it.

use std::path::PathBuf;
use std::time::Duration;

use oximux_remote_local::{
    DialError, LocalSocketTransport, SESSION_ENV_VAR, SESSION_TOKEN_ENV_VAR, dial,
};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::HelloReq;
use oximux_remote_proto::proto::{
    MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Request, Response, is_compatible,
};

use crate::cli::exit;
use crate::output::Failure;

pub struct Client {
    transport: std::sync::Arc<LocalSocketTransport>,
    timeout: Duration,
    /// The host's announced protocol version, from the `Hello` exchange.
    pub host_version: u32,
    pub host_min_compatible: u32,
}

/// The runtime dir a host would be using on this machine, unless overridden.
pub fn runtime_dir(dir: Option<PathBuf>) -> Result<PathBuf, Failure> {
    dir.map_or_else(
        || {
            oximux_remote_local::default_runtime_dir().ok_or_else(|| {
                Failure::new(
                    "no-data-dir",
                    exit::ERROR,
                    "this platform reports no local data directory",
                )
                .with_steps(["pass --dir <DIR> explicitly".into()])
            })
        },
        Ok,
    )
}

/// Whether this invocation is running inside an agent session — used only to
/// word errors helpfully. The credential itself (and therefore the scope) is
/// chosen inside `remote-local`, from what this process can actually prove.
fn looks_agent_scoped() -> bool {
    std::env::var(SESSION_ENV_VAR).is_ok_and(|v| !v.trim().is_empty())
}

impl Client {
    /// Dial, authenticate, and exchange versions. Every failure carries its
    /// exit class and a concrete next step.
    pub async fn connect(dir: Option<PathBuf>, timeout_secs: u64) -> Result<Self, Failure> {
        let runtime_dir = runtime_dir(dir)?;
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let dialed = tokio::time::timeout(timeout, dial(&runtime_dir))
            .await
            .map_err(|_| timed_out("connecting to the host"))?;
        let transport = dialed.map_err(|e| match e {
            DialError::Unreachable { .. } if looks_agent_scoped() => {
                Failure::new("unreachable", exit::UNREACHABLE, e.to_string()).with_steps([
                    format!(
                        "this looks like an agent session but ${SESSION_TOKEN_ENV_VAR} is unset — \
                         only the host that spawned the agent can supply it"
                    ),
                ])
            }
            DialError::Unreachable { .. } => Failure::new("unreachable", exit::UNREACHABLE, e.to_string())
                .with_steps([
                    "open the OxiMux desktop app and enable local CLI access (Settings → Remote)"
                        .into(),
                    "if it is enabled, check the app is running".into(),
                ]),
            DialError::Denied(_) => Failure::new("denied", exit::DENIED, e.to_string()).with_steps([
                "the control credential rotated — toggle local CLI access off and on, then retry"
                    .into(),
            ]),
            DialError::Handshake(_) => Failure::new("handshake", exit::UNREACHABLE, e.to_string())
                .with_steps(["retry; if it persists, restart the desktop app".into()]),
        })?;

        let mut client = Self {
            transport,
            timeout,
            host_version: 0,
            host_min_compatible: 0,
        };
        let ack = client
            .call(Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION }))
            .await?;
        let Response::HelloAck(ack) = ack else {
            return Err(Failure::new(
                "protocol",
                exit::ERROR,
                "the host answered Hello with something else",
            ));
        };
        // Both directions, like the phone: refuse a host too old for us, and
        // surface a host that will refuse us.
        if !is_compatible(ack.protocol_version) || PROTOCOL_VERSION < ack.min_compatible {
            return Err(Failure::new(
                "incompatible",
                exit::ERROR,
                format!(
                    "host speaks protocol v{} (min v{}); this CLI speaks v{} (min v{})",
                    ack.protocol_version,
                    ack.min_compatible,
                    PROTOCOL_VERSION,
                    MIN_COMPATIBLE_VERSION
                ),
            )
            .with_steps(["update the older side so both speak a compatible protocol".into()]));
        }
        client.host_version = ack.protocol_version;
        client.host_min_compatible = ack.min_compatible;
        Ok(client)
    }

    /// One request → one response, bounded by the global timeout.
    pub async fn call(&self, req: Request) -> Result<Response, Failure> {
        let bytes = req.to_bytes().map_err(|e| {
            Failure::new("encode", exit::ERROR, format!("could not encode request: {e}"))
        })?;
        let exchange = async {
            self.transport
                .send(bytes)
                .await
                .map_err(|e| Failure::new("transport", exit::UNREACHABLE, e.to_string()))?;
            let frame = self
                .transport
                .recv()
                .await
                .map_err(|e| Failure::new("transport", exit::UNREACHABLE, e.to_string()))?
                .ok_or_else(|| {
                    Failure::new("closed", exit::UNREACHABLE, "the host closed the connection")
                })?;
            Response::from_bytes(&frame).map_err(|e| {
                Failure::new("decode", exit::ERROR, format!("undecodable host reply: {e}"))
            })
        };
        tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| timed_out("waiting for the host's reply"))?
    }
}

fn timed_out(what: &str) -> Failure {
    Failure::new("timeout", exit::TIMEOUT, format!("timed out {what}")).with_steps([
        "the host may be busy — retry, or raise --timeout".into(),
    ])
}

/// Map a protocol-level error into the exit-code contract. Shared by every
/// verb so `Unauthorized` is always exit 5 and never a generic failure.
pub fn rpc_failure(err: oximux_remote_proto::proto::RpcError) -> Failure {
    use oximux_remote_proto::proto::RpcError;
    match err {
        RpcError::Unauthorized => {
            Failure::new("denied", exit::DENIED, "the host refused this call").with_steps([
                format!(
                    "agent-scoped invocations (${SESSION_ENV_VAR} set) reach only their own session"
                ),
                "operator access covers everything — run without the session variable".into(),
            ])
        }
        RpcError::UnknownSession => {
            Failure::new("unknown-session", exit::ERROR, "no such session on this host")
                .with_steps(["run `oximux ls` to list sessions".into()])
        }
        other => Failure::new("rpc", exit::ERROR, format!("host error: {other:?}")),
    }
}
