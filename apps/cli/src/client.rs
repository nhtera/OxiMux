//! The lazy host client: nothing here runs until a verb actually needs the
//! host, so `--help`, `version` and `agent-context` never touch the socket.
//!
//! Every connection claims a scope: `OXIMUX_SESSION_ID` in the environment
//! names the per-session credential this process holds, narrowing it to that
//! one session; its absence is an operator at their own keyboard. The host
//! enforces the claim — this side merely never omits it.
//!
//! `oximux serve` injects those variables into every agent it spawns; the
//! desktop app does not yet, so an agent there runs as the operator. See
//! `oximux-remote-local`'s threat-boundary docs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oximux_remote_local::{DialError, SESSION_ENV_VAR, dial};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::HelloReq;
use oximux_remote_proto::proto::{
    MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Request, Response, is_compatible,
};

use crate::cli::exit;
use crate::hosts_store::{HOST_ENV_VAR, HostEntry, HostsFile};
use crate::output::Failure;

/// Which host a client is talking to — the one fact a verb may need beyond the
/// transport itself, since a remote host's name belongs in `--json` output and
/// in the fleet table's host column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTarget {
    /// The owner-only socket on this machine.
    Local,
    /// A paired host, by the name the hosts file records.
    Remote(String),
}

impl HostTarget {
    pub fn label(&self) -> &str {
        match self {
            HostTarget::Local => "local",
            HostTarget::Remote(name) => name,
        }
    }
}

pub struct Client {
    /// `dyn Transport`, not the local socket type: every verb is written
    /// against `call`/`recv_frame`, so making this the abstract seam is what
    /// lets the whole existing verb surface work over iroh without touching a
    /// single one of them.
    transport: Arc<dyn Transport>,
    timeout: Duration,
    /// The host's announced protocol version, from the `Hello` exchange.
    pub host_version: u32,
    pub host_min_compatible: u32,
    pub target: HostTarget,
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

/// A reply that does not answer the request that was sent. One helper so every
/// verb reports a protocol fault under the same code and exit class — `ls` and
/// `status` used to build this separately and could drift apart.
pub fn unexpected_reply(what: &str, got: &Response) -> Failure {
    Failure::new(
        "protocol",
        exit::ERROR,
        format!("unexpected reply to {what}: {got:?}"),
    )
}

/// How a verb was told which host to reach.
pub struct HostSelection {
    /// The `--host` flag, if given.
    pub flag: Option<String>,
    /// The runtime dir override, for the local path only.
    pub dir: Option<PathBuf>,
}

impl Client {
    /// Connect to whichever host the resolution order picks: `--host` →
    /// `OXIMUX_HOST` → the recorded default → the local socket.
    ///
    /// Bare local use stays zero-config: a machine that has never paired has no
    /// hosts file, which resolves to `None` and takes the local path exactly as
    /// before.
    pub async fn resolve_and_connect(
        selection: HostSelection,
        timeout_secs: u64,
    ) -> Result<Self, Failure> {
        let env = std::env::var(HOST_ENV_VAR).ok();
        let config = crate::hosts_store::config_dir()?;
        let hosts = HostsFile::load(&config)?;
        match hosts.resolve(selection.flag.as_deref(), env.as_deref())? {
            Some(entry) => Self::connect_remote(&config, entry, timeout_secs).await,
            None => Self::connect(selection.dir, timeout_secs).await,
        }
    }

    /// Connect to one paired host over iroh, authenticating with the key minted
    /// for it.
    pub async fn connect_remote(
        config_dir: &std::path::Path,
        entry: &HostEntry,
        timeout_secs: u64,
    ) -> Result<Self, Failure> {
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let endpoint_id = crate::hosts_store::parse_endpoint_id(&entry.endpoint_id)?;
        let signer = crate::client_identity::load_or_generate(config_dir, &entry.name)?;
        // One deadline for the whole connect — dial, versions, and auth — so a
        // host that accepts the connection and then stalls cannot cost several
        // multiples of what `--timeout` promised.
        let deadline = crate::remote_client::deadline_in(timeout);
        let transport = crate::remote_client::dial(endpoint_id, deadline).await?;
        // Versions before credentials, exactly as the phone does it: an
        // unusable pairing must fail with "update one side" rather than a
        // mystery refusal several RPCs later.
        let ack = crate::remote_client::hello(transport.as_ref(), deadline).await?;
        crate::remote_client::authenticate(transport.as_ref(), &signer, deadline).await?;
        Ok(Self {
            transport,
            timeout,
            host_version: ack.protocol_version,
            host_min_compatible: ack.min_compatible,
            target: HostTarget::Remote(entry.name.clone()),
        })
    }

    /// Dial the local socket, authenticate, and exchange versions. Every
    /// failure carries its exit class and a concrete next step.
    pub async fn connect(dir: Option<PathBuf>, timeout_secs: u64) -> Result<Self, Failure> {
        let runtime_dir = runtime_dir(dir)?;
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let dialed = tokio::time::timeout(timeout, dial(&runtime_dir))
            .await
            .map_err(|_| timed_out("connecting to the host"))?;
        let transport = dialed.map_err(|e| match e {
            // An access failure, so exit 5 like every other refusal — the host
            // may be running perfectly well. Reporting it as `unreachable` sent
            // a retrying wrapper into a loop no retry could resolve.
            DialError::NoCredential(_) => Failure::new("denied", exit::DENIED, e.to_string())
                .with_steps([
                    "only the host that spawned this agent can supply its credential".into(),
                    "to run as the operator instead, clear the session variables from \
                     the environment"
                        .into(),
                ]),
            // Both hosts are named, headless first. This is the error a server
            // operator hits most, and advice to open a GUI that is not installed
            // is worse than no advice — it sends them looking for the wrong
            // thing. The remote path (`remote_client.rs`) has always said both.
            DialError::Unreachable { .. } => Failure::new("unreachable", exit::UNREACHABLE, e.to_string())
                .with_steps([
                    "start a host here with `oximux serve` (see docs/server-install.md)".into(),
                    "or, on a desktop: open the OxiMux app and enable local CLI \
                     access (Settings → Remote)"
                        .into(),
                    "if a host should already be running, check it is still up".into(),
                ]),
            DialError::Denied(_) => Failure::new("denied", exit::DENIED, e.to_string()).with_steps([
                "the control credential rotated — restart `oximux serve`, or toggle \
                 local CLI access off and on in the desktop app, then retry"
                    .into(),
            ]),
            DialError::Handshake(_) => Failure::new("handshake", exit::UNREACHABLE, e.to_string())
                .with_steps([
                    "retry; if it persists, restart the host (`oximux serve`, or the \
                     desktop app)"
                        .into(),
                ]),
        })?;

        let mut client = Self {
            transport,
            timeout,
            host_version: 0,
            host_min_compatible: 0,
            target: HostTarget::Local,
        };
        client.exchange_versions().await?;
        Ok(client)
    }

    /// The `Hello` exchange, run before anything else on both the local and the
    /// remote path.
    ///
    /// Checked in **both** directions, like the phone: refuse a host too old
    /// for this CLI to understand, and surface a host whose floor would refuse
    /// this CLI. Either way the answer names both versions, because "update"
    /// is useless advice without saying which side.
    async fn exchange_versions(&mut self) -> Result<(), Failure> {
        let ack = self.call(Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION })).await?;
        let Response::HelloAck(ack) = ack else {
            return Err(Failure::new(
                "protocol",
                exit::ERROR,
                "the host answered Hello with something else",
            ));
        };
        if !is_compatible(ack.protocol_version) || PROTOCOL_VERSION < ack.min_compatible {
            return Err(skew_failure(ack.protocol_version, ack.min_compatible));
        }
        self.host_version = ack.protocol_version;
        self.host_min_compatible = ack.min_compatible;
        Ok(())
    }

    /// Refuse a call this host is too old to serve, before it is sent.
    ///
    /// The compat gate the fleet needs: `Hello` already rejects a host outside
    /// the mutually-compatible range, but a host inside it can still predate a
    /// specific verb — and postcard would answer an appended ordinal with a
    /// dropped connection rather than a clean error. So a verb that needs a
    /// minimum version says so, and gets a sentence naming the host's version
    /// and the one it needs instead of "the host closed the connection".
    pub fn require_version(&self, needed: u32, verb: &str) -> Result<(), Failure> {
        if self.host_version >= needed {
            return Ok(());
        }
        Err(Failure::new(
            "incompatible",
            exit::UNREACHABLE,
            format!(
                "`{verb}` needs protocol v{needed}; {} speaks v{}",
                self.target.label(),
                self.host_version
            ),
        )
        .with_steps([
            format!("update the host so it speaks at least v{needed}"),
            "`oximux hosts ls` shows every host's protocol version".into(),
        ]))
    }

    /// One request → one response, bounded by the global timeout. Any pushed
    /// frame that interleaves (a live event on a subscribed connection) is
    /// silently dropped — verbs that subscribe must use
    /// [`call_streaming`](Self::call_streaming) instead, or lose events.
    pub async fn call(&self, req: Request) -> Result<Response, Failure> {
        self.call_streaming(req, |_| {}).await
    }

    /// One request → one response, feeding every pushed frame that interleaves
    /// to `on_push` instead of dropping it. The per-frame wait is bounded by
    /// the global timeout; a chatty stream cannot starve the reply, because
    /// every frame either IS the reply or goes to `on_push`.
    pub async fn call_streaming(
        &self,
        req: Request,
        mut on_push: impl FnMut(Response),
    ) -> Result<Response, Failure> {
        let bytes = req.to_bytes().map_err(|e| {
            Failure::new("encode", exit::ERROR, format!("could not encode request: {e}"))
        })?;
        tokio::time::timeout(self.timeout, self.transport.send(bytes))
            .await
            .map_err(|_| timed_out("sending to the host"))?
            .map_err(|e| Failure::new("transport", exit::UNREACHABLE, e.to_string()))?;
        loop {
            let frame = tokio::time::timeout(self.timeout, self.recv_frame())
                .await
                .map_err(|_| timed_out("waiting for the host's reply"))??;
            if is_push(&frame) {
                on_push(frame);
                continue;
            }
            return Ok(frame);
        }
    }

    /// The next frame from the host — push or reply, undistinguished. No
    /// timeout: an idle live stream is normal, and liveness is the socket
    /// itself. Streaming loops (attach/wait/term) sit on this.
    pub async fn recv_frame(&self) -> Result<Response, Failure> {
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
    }
}

/// Whether a frame is an unsolicited push (live stream traffic) rather than
/// the reply to an outstanding request. The variant IS the demux key — the
/// protocol deliberately uses distinct variants for pushes (`Event` vs the
/// `Events` reply, `SessionsChanged` vs `Sessions`) for exactly this test.
pub fn is_push(resp: &Response) -> bool {
    matches!(
        resp,
        Response::Event(_)
            | Response::SessionsChanged(_)
            | Response::ScheduleRunsChanged(_)
            | Response::TermOutput { .. }
            | Response::TermGapped { .. }
            | Response::TermExited { .. }
    )
}

/// A version mismatch either side cannot bridge, named from both ends.
fn skew_failure(host_version: u32, host_min: u32) -> Failure {
    Failure::new(
        "incompatible",
        exit::ERROR,
        format!(
            "host speaks protocol v{host_version} (min v{host_min}); \
             this CLI speaks v{PROTOCOL_VERSION} (min v{MIN_COMPATIBLE_VERSION})"
        ),
    )
    .with_steps(["update the older side so both speak a compatible protocol".into()])
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
            let mut steps = vec!["run `oximux ls` to list sessions".to_string()];
            // The one wrong guess worth naming: an agent reaching for its own
            // id finds the credential handle in its environment, which is not
            // a session id and never resolves to one.
            if std::env::var_os(SESSION_ENV_VAR).is_some() {
                steps.push(format!(
                    "${SESSION_ENV_VAR} names this process's credential, not a session — \
                     `oximux ls` shows the one session it can reach"
                ));
            }
            Failure::new("unknown-session", exit::ERROR, "no such session on this host")
                .with_steps(steps)
        }
        RpcError::Unsupported => Failure::new(
            "unsupported",
            exit::ERROR,
            "this host does not offer that capability",
        ),
        // The host authored this string for a person to read — "no such
        // permission mode for this session; it offers: …" — so print it, not a
        // Debug rendering of the variant wrapping it. Through the catch-all
        // below it surfaced as `host error: BadRequest("no such permission
        // mode …")`, which buries the sentence that actually helps inside Rust
        // syntax the reader did not ask for.
        RpcError::BadRequest(message) => Failure::new("bad-request", exit::ERROR, message),
        other => Failure::new("rpc", exit::ERROR, format!("host error: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_at(version: u32, target: HostTarget) -> Client {
        struct Dead;
        #[async_trait::async_trait]
        impl Transport for Dead {
            async fn send(&self, _: Vec<u8>) -> Result<(), oximux_remote_proto::TransportError> {
                Ok(())
            }
            async fn recv(
                &self,
            ) -> Result<Option<Vec<u8>>, oximux_remote_proto::TransportError> {
                Ok(None)
            }
        }
        Client {
            transport: Arc::new(Dead),
            timeout: Duration::from_secs(1),
            host_version: version,
            host_min_compatible: 1,
            target,
        }
    }

    /// The gate lets a current host through and refuses an older one — with a
    /// message naming both the host and the version it would need, because
    /// "update something" is not an actionable next step.
    #[test]
    fn the_version_gate_names_the_host_and_the_version_it_needs() {
        let current = client_at(PROTOCOL_VERSION, HostTarget::Local);
        assert!(current.require_version(18, "heartbeat").is_ok());

        let old = client_at(15, HostTarget::Remote("server".into()));
        let err = old.require_version(18, "heartbeat").expect_err("too old");
        assert_eq!(err.exit, exit::UNREACHABLE);
        assert!(err.message.contains("server"), "names the host: {}", err.message);
        assert!(err.message.contains("v18"), "names what it needs: {}", err.message);
        assert!(err.message.contains("v15"), "and what it has: {}", err.message);
        assert!(!err.next_steps.is_empty(), "and says what to do");
    }

    /// A host at exactly the required version is served — the boundary, since
    /// an off-by-one here would refuse every host the moment it caught up.
    #[test]
    fn a_host_at_exactly_the_required_version_passes() {
        assert!(client_at(18, HostTarget::Local).require_version(18, "state").is_ok());
    }

    /// A `BadRequest` reaches the user as the sentence the host wrote.
    ///
    /// These carry the host's own actionable text — the list of ids a session
    /// actually offers, for instance — and the catch-all arm used to wrap them
    /// in `host error: BadRequest("…")`, putting Rust syntax around the only
    /// part worth reading.
    #[test]
    fn a_bad_request_surfaces_the_hosts_own_words() {
        use oximux_remote_proto::proto::RpcError;
        let failure = rpc_failure(RpcError::BadRequest(
            "no such permission mode for this session; it offers: default, plan".into(),
        ));
        assert_eq!(failure.exit, exit::ERROR);
        assert_eq!(failure.code, "bad-request");
        assert_eq!(
            failure.message,
            "no such permission mode for this session; it offers: default, plan",
            "verbatim — no Debug wrapper, no prefix"
        );
        assert!(!failure.message.contains("BadRequest"), "{}", failure.message);
    }

    #[test]
    fn the_local_target_labels_itself() {
        assert_eq!(HostTarget::Local.label(), "local");
        assert_eq!(HostTarget::Remote("prod".into()).label(), "prod");
    }
}
