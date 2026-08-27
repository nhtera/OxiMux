//! Connection factory — turns a transport-tagged [`ConnectSpec`] into a live
//! `Box<dyn AgentConnection>` + its event receiver, so the app never names a
//! concrete connection type. The StreamJson arm drives Claude (today's path);
//! the ACP arm is a discoverable stub a later phase fills.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::acp::AcpConnection;
use super::claude_stream_json::{ClaudeStreamJsonConnection, HostInjection};
use super::codex::CodexAppServerConnection;
use super::connection::{AgentConnection, ModelChoice};
use super::event::ThreadEvent;
use super::mcp_server_spec::McpServerSpec;
use super::omp::posture::OmpPosture;
use super::omp::OmpRpcConnection;
use super::pi::posture::PiPosture;
use super::pi::PiRpcConnection;
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
    /// Environment overrides the user configured for this adapter (and launch
    /// profile), layered onto the spawn by every transport. This is how a chat
    /// reaches an alternate base URL or a second account without a source
    /// patch.
    ///
    /// It rides on the backend rather than being set per call site because the
    /// backend is already the one thing resolved from settings, so every chat
    /// entry point — a fresh launch, a restore, a respawn, the remote bridge,
    /// `serve` — picks it up from [`ConnectSpec::for_backend`] instead of each
    /// remembering to. Empty for every adapter with no configured env, which
    /// leaves those launches byte-identical.
    ///
    /// Not secret storage: these come from a plaintext settings file.
    pub env: Vec<(String, String)>,
    /// The settings key this backend was resolved from, and the named launch
    /// profile within it. `None`/`None` for a backend built without settings
    /// (a bare `Transport::into()`, a test fixture).
    ///
    /// These are carried so a chat can be **re-resolved** later rather than
    /// only replayed: a restore persists this pair and reads `env` back out of
    /// the current settings, which keeps a corrected base URL effective on the
    /// next open and keeps env values out of a second on-disk file. `env`
    /// itself is the already-resolved answer for the live connection.
    pub adapter_id: Option<String>,
    /// See [`Self::adapter_id`]. `None` = the adapter's plain entry.
    pub profile: Option<String>,
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
        Self {
            transport,
            acp_command: None,
            acp_args: Vec::new(),
            env: Vec::new(),
            adapter_id: None,
            profile: None,
        }
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
    /// Extra environment overrides for the spawned agent, applied on top of the
    /// inherited environment by **every** transport.
    ///
    /// Two producers today: the EnvVar-auth respawn puts the credentials the
    /// user typed here so the relaunched agent reads them (ACP only, since that
    /// is the only protocol with an EnvVar auth method), and a host that
    /// confines its agents puts this child's local-control credential here.
    /// The second is why this is no longer ACP-only — an agent's confinement
    /// cannot depend on which adapter it happens to run under.
    ///
    /// Held only in-flight — never written to the persisted chat blob. Empty
    /// for a launch that declares neither.
    pub env: Vec<(String, String)>,
    /// An auth method to `authenticate` once, automatically, right after an
    /// env-carrying respawn still reports `AuthRequired` — the "set env, then
    /// authenticate" EnvVar flow, so the user isn't re-prompted. `None` otherwise.
    pub auth_method: Option<String>,
    /// The Codex posture `(approval_policy, sandbox)` to seed at spawn (only read
    /// by the `AppServer` arm). Set from a restored chat's persisted posture so a
    /// reopened Codex session keeps its Approvals/Sandbox choice; `None` starts at
    /// the default posture (on-request / workspace-write).
    pub codex_posture: Option<(String, String)>,
    /// An explicit path to the `pi` binary (only read by the `Rpc` arm). `None`
    /// falls back to PATH, then a login-shell probe — needed because a
    /// Finder-launched app inherits no shell PATH and pi usually lives under a
    /// version manager. Deliberately its own field rather than reusing
    /// `acp_command`: these are per-adapter launch details that happen to be
    /// strings, not a shared abstraction.
    pub pi_command: Option<String>,
    /// Pi's session-level tool posture (only read by the `Rpc` arm). `None` uses
    /// the deliberate default (Standard — see `PiPosture::default`). Its own
    /// field rather than a second meaning for `codex_posture`: that one is
    /// documented Codex-specific, and this struct already carries one launch
    /// field per transport (`permission_mode`, `effort`, `codex_posture`) — a
    /// convention, not a shared abstraction.
    pub pi_posture: Option<PiPosture>,
    /// An explicit path to the `omp` binary (only read by the `OmpRpc` arm) —
    /// same GUI-has-no-PATH rationale as `pi_command`, its own field for the
    /// same per-adapter-launch-detail reason.
    pub omp_command: Option<String>,
    /// omp's approval posture (only read by the `OmpRpc` arm). `None` uses
    /// the deliberate OxiMux default (`Write` — NOT omp's own `yolo` default;
    /// the flag is always passed explicitly either way). Never `PiPosture`:
    /// the domains differ (a tool allowlist vs an approval mode).
    pub omp_posture: Option<OmpPosture>,
    /// MCP servers the *host* declares for this session — a sidecar OxiMux
    /// spawns and supervises, on top of whatever the user's own config provides.
    ///
    /// Unlike the per-transport fields above this one is deliberately shared:
    /// MCP is a cross-agent protocol, so the same declaration is meant to reach
    /// Claude (`--mcp-config`) and ACP (`mcpServers`) alike. Empty for every
    /// launch that declares none, which keeps those invocations unchanged.
    pub mcp_servers: Vec<McpServerSpec>,
    /// Inline settings JSON for this session (Claude's `--settings`), carrying
    /// the hooks OxiMux uses to police what it declared above.
    ///
    /// Claude-only, like `codex_posture` is Codex-only: hooks are that CLI's
    /// mechanism and no other transport here has an equivalent. That asymmetry
    /// is a real constraint on the caller rather than a gap to paper over — a
    /// capability whose enforcement rides this field must not be declared to a
    /// transport that drops it.
    pub settings_json: Option<String>,
    /// Tool names to strip from the agent's surface (`--disallowedTools`).
    /// Claude-only for the same reason.
    pub disallowed_tools: Vec<String>,
    /// An id the caller picked for this new session rather than waiting to be
    /// told one (Claude's `--session-id`). `None` leaves the agent to name its
    /// own, which is what every interactive launch does.
    ///
    /// Set by a headless host, which has no one to wait for — see
    /// [`HostInjection::fresh_session_id`] for why waiting deadlocks. Read only
    /// by the `StreamJson` arm; a transport that cannot be told an id ignores
    /// this and announces its own as before.
    ///
    /// [`HostInjection::fresh_session_id`]: super::claude_stream_json::HostInjection::fresh_session_id
    pub fresh_session_id: Option<String>,
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
            // The adapter/profile env the user configured. A respawn that also
            // carries EnvVar-auth credentials appends them AFTER these, so the
            // credentials win on a key collision (see `respawn_spec`).
            env: backend.env.clone(),
            auth_method: None,
            // Set only when restoring a Codex chat with a persisted posture; a
            // fresh launch starts at the default posture.
            codex_posture: None,
            pi_command: None,
            pi_posture: None,
            omp_command: None,
            omp_posture: None,
            // Set by the caller after construction when the host has something
            // to declare; a plain launch declares nothing.
            mcp_servers: Vec::new(),
            settings_json: None,
            disallowed_tools: Vec::new(),
            fresh_session_id: None,
        }
    }
}

/// Open a chat connection for `spec.transport`, returning the boxed connection
/// and the `ThreadEvent` receiver the app drains. Semantics of the StreamJson
/// arm are identical to constructing `ClaudeStreamJsonConnection::spawn_resumed`
/// directly (the app's prior call).
pub fn connect(spec: ConnectSpec) -> Result<(Arc<dyn AgentConnection>, Receiver<ThreadEvent>)> {
    match spec.transport {
        Transport::StreamJson => {
            let (conn, rx) = ClaudeStreamJsonConnection::spawn_resumed(
                &spec.cwd,
                spec.model.as_deref(),
                spec.resume_session_id.as_deref(),
                spec.permission_mode.as_deref(),
                spec.effort.as_deref(),
                &HostInjection {
                    mcp_servers: &spec.mcp_servers,
                    settings: spec.settings_json.as_deref(),
                    disallowed_tools: &spec.disallowed_tools,
                    fresh_session_id: spec.fresh_session_id.as_deref(),
                },
                &spec.env,
            )?;
            Ok((Arc::new(conn) as Arc<dyn AgentConnection>, rx))
        }
        Transport::AppServer => {
            let posture = spec
                .codex_posture
                .as_ref()
                .map(|(a, s)| (a.as_str(), s.as_str()));
            let (conn, rx) = CodexAppServerConnection::spawn(
                &spec.cwd,
                spec.model.as_deref(),
                spec.resume_session_id.as_deref(),
                spec.effort.as_deref(),
                posture,
                &spec.env,
            )?;
            Ok((Arc::new(conn) as Arc<dyn AgentConnection>, rx))
        }
        Transport::Acp => {
            let command = spec
                .acp_command
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| anyhow::anyhow!("ACP transport requires an acp_command"))?;
            let (conn, rx) = AcpConnection::spawn_with_env(
                command,
                &spec.acp_args,
                &spec.cwd,
                spec.resume_session_id.clone(),
                spec.env.clone(),
                spec.auth_method.clone(),
                spec.mcp_servers.clone(),
            )?;
            Ok((Arc::new(conn) as Arc<dyn AgentConnection>, rx))
        }
        Transport::Rpc => {
            // `None` → the deliberate default, never an accidental "no flags".
            let posture = spec.pi_posture.clone().unwrap_or_default();
            // Pi resumes by session id (`--session`), scoped to the store for
            // `cwd`'s project — so the caller must pass the session's own cwd.
            // A path here is refused rather than resumed: pi would silently mint
            // an empty session at it (see `pi::build_args`).
            let (conn, rx) = PiRpcConnection::spawn(
                &spec.cwd,
                spec.model.as_deref(),
                spec.pi_command.as_deref(),
                posture,
                spec.resume_session_id.as_deref(),
                &spec.env,
            )?;
            Ok((Arc::new(conn) as Arc<dyn AgentConnection>, rx))
        }
        Transport::OmpRpc => {
            // `None` → the deliberate OxiMux default (Write). The flag itself
            // is ALWAYS emitted — omp's own default is yolo (see `build_args`).
            let posture = spec.omp_posture.unwrap_or_default();
            // omp resumes by full session UUID only; `build_args` refuses
            // anything else (its resolver prefix-matches silently). Spawn in
            // the session's own cwd so tool work lands in the right project.
            let (conn, rx) = OmpRpcConnection::spawn(
                &spec.cwd,
                spec.model.as_deref(),
                spec.omp_command.as_deref(),
                posture,
                spec.resume_session_id.as_deref(),
                &spec.env,
            )?;
            Ok((Arc::new(conn) as Arc<dyn AgentConnection>, rx))
        }
    }
}

/// The model vocabulary a short-lived *catalog probe* pulls from an agent whose
/// models are only known after it spawns (Codex, ACP) — so the unbound *New
/// Agent* draft can offer a real model picker before the user commits. Claude's
/// models are static (declared in the roster) and never probed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
            env: vec![],
            auth_method: None,
            codex_posture: None,
            pi_command: None,
            pi_posture: None,
            omp_command: None,
            omp_posture: None,
            mcp_servers: vec![],
            settings_json: None,
            disallowed_tools: vec![],
            fresh_session_id: None,
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
            env: vec![],
            auth_method: None,
            codex_posture: None,
            pi_command: None,
            pi_posture: None,
            omp_command: None,
            omp_posture: None,
            mcp_servers: vec![],
            settings_json: None,
            disallowed_tools: vec![],
            fresh_session_id: None,
        };
        let err = probe_catalog(spec).expect_err("probe must fail without a command");
        assert!(err.to_string().contains("acp_command"), "unexpected error: {err}");
    }
}
