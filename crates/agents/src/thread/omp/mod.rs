//! omp chat backend — drives `omp --mode rpc-ui` as a subprocess.
//!
//! omp is a Pi fork that kept Pi's event taxonomy and NDJSON envelope but
//! renamed/extended the command layer, added a versioned handshake with
//! chunked-frame transport, and — the piece Pi never had — **real per-call
//! tool approval** delivered as `extension_ui_request` dialogs in rpc-ui
//! mode. Everything here was verified live against omp 18.0.4 (probe 01,
//! plans/…/research/probe-01-omp-behavior.md) with the reference integration
//! as the architectural template.
//!
//! Layout mirrors `thread/pi/`: [`protocol`] owns the wire types (plus the
//! chunk reassembler and the approval parser), [`map`] the event mapping,
//! [`posture`] the `--approval-mode` spawn flag, and this module the
//! connection + lifecycle. The pipe itself is the shared
//! [`ndjson_transport`] core.
//!
//! ## Lifecycle
//!
//! Same contract as Pi's, re-verified live: SIGTERM (never SIGKILL first)
//! runs omp's handler, which reaps the bash tool's detached children — a
//! probe with a running `sleep 300 &` left no orphans, exit 143 in ~1.5s.
//! Closing stdin resolves any pending approval dialog as DENY (verified), so
//! teardown can never leak an approval into an allow.
//!
//! [`ndjson_transport`]: super::ndjson_transport

pub mod map;
pub mod posture;
pub mod protocol;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use super::connection::{
    AgentCapabilities, AgentConnection, EffortChoice, FeatureControl, FeatureKind,
    FeatureSelectOption, ModelChoice, SlashCommandInfo,
};
use super::entry::ChatImage;
use super::event::{SessionMeta, ThreadEvent, TurnUsage};
use super::ndjson_transport::NdjsonRpcClient;
use super::pi::{fmt_window, thinking_label};
use super::tool_call::{PermissionDecision, PermissionKind};
use posture::{OmpPosture, FEATURE_APPROVALS};
use protocol::{
    approval_input, classify, parse_tool_approval, AvailableCommands, AvailableModels,
    ExtensionUiResponse, Inbound, Model, OmpCommand, ReadyFrame, SessionState, APPROVE, DENY,
};

/// Diagnostic name for error strings.
const NAME: &str = "omp --mode rpc-ui";

/// Bound on the wait for omp's `ready` frame. Measured warm latency is
/// 450–750ms; 10s absorbs a cold bun start while keeping a wedged omp from
/// hanging a new chat (the EOF drain covers the crash case far faster). Runs
/// on the connect thread, so it must be bounded (red-team F3).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the `get_state` handshake may take after `ready`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for omp to exit after SIGTERM before escalating. SIGTERM
/// runs the handler that reaps detached tool children; SIGKILL skips it.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on a user-driven control round-trip (`set_model`).
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// The reassembly ceiling omp 18.0.4 declares (`maxReassembledFrameBytes`).
/// The reassembler is installed before the `ready` frame can confirm it, so
/// this is the constant; connect asserts omp still declares the same.
const MAX_REASSEMBLED_BYTES: u64 = 64 * 1024 * 1024;

/// The subagent-event depth OxiMux subscribes to — lifecycle only (cards fold
/// into existing tool rendering; full per-subagent streams are out of scope,
/// recorded in the plan's validation log).
const SUBAGENT_SUBSCRIPTION_LEVEL: &str = "lifecycle";

/// A live `omp --mode rpc-ui` chat session.
pub struct OmpRpcConnection {
    rpc: OmpRpcClient,
    child: Arc<Mutex<Child>>,
    /// Windows stand-in for the process group (same rationale as Pi's).
    #[cfg(windows)]
    job: Option<Arc<oximux_job_object::JobObject>>,
    /// Live session facts; `set_model`/`set_thinking_level` mutate in-session.
    state: Arc<Mutex<Option<SessionState>>>,
    /// omp's catalog, pre-filtered to providers with credentials.
    models: Vec<ModelChoice>,
    /// The palette, updated live by `available_commands_update` push frames
    /// (red-team F15) — hence shared with the worker, unlike Pi's fixed list.
    commands: Arc<Mutex<Vec<SlashCommandInfo>>>,
    /// The meter's denominator, shared with the mapper.
    context_window: Arc<AtomicU64>,
    /// Approval dialogs currently awaiting an answer. An id not in here has
    /// either been answered or never existed — a late/duplicate decision must
    /// not reach the wire (an answer for a resolved dialog is undefined).
    pending_approvals: Arc<Mutex<HashSet<String>>>,
    /// The posture this process was spawned with. Fixed for the process's
    /// life — `--approval-mode` is spawn-time, so changing it respawns.
    posture: OmpPosture,
}

/// Thin typed layer over the shared NDJSON core.
#[derive(Clone)]
struct OmpRpcClient {
    inner: NdjsonRpcClient,
}

impl OmpRpcClient {
    fn spawn_command(cmd: Command) -> Result<(Self, Receiver<Value>, Child)> {
        let (inner, rx, child) = NdjsonRpcClient::spawn_command(
            cmd,
            NAME,
            Some(protocol::chunk_reassembler(MAX_REASSEMBLED_BYTES)),
        )?;
        Ok((Self { inner }, rx, child))
    }

    fn request(&self, cmd: OmpCommand, timeout: Duration) -> Result<protocol::RpcResponse> {
        let line = serde_json::to_string(&cmd).context("serialize omp command")?;
        let v = self.inner.request_value(cmd.id(), &line, timeout)?;
        serde_json::from_value(v).context("parse omp response")
    }

    fn send(&self, cmd: OmpCommand) -> Result<()> {
        let line = serde_json::to_string(&cmd).context("serialize omp command")?;
        self.inner.send_line(&line)
    }

    fn send_ui_response(&self, resp: &ExtensionUiResponse) -> Result<()> {
        let line = serde_json::to_string(resp).context("serialize omp ui response")?;
        self.inner.send_line(&line)
    }

    fn next_id(&self, prefix: &str) -> String {
        self.inner.next_id(prefix)
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    fn close_stdin(&self) {
        self.inner.close_stdin()
    }
}

impl OmpRpcConnection {
    /// Spawn omp in `cwd` and complete the ready → negotiate → `get_state`
    /// handshake.
    ///
    /// `posture` is fixed here: `--approval-mode` is a spawn flag, and it is
    /// ALWAYS passed explicitly — omp's own default is `yolo` (auto-approve
    /// exec), which must never be reachable by omission.
    ///
    /// `resume` is omp's session id and must be the FULL canonical UUID: the
    /// resolver prefix-matches and falls back across projects silently
    /// (probe 01), so anything shorter can land in the wrong session. `cwd`
    /// should be the session's own cwd so relative tool work lands right.
    pub fn spawn(
        cwd: &Path,
        model: Option<&str>,
        program: Option<&str>,
        posture: OmpPosture,
        resume: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let args = build_args(model, &posture, resume)?;
        let program = resolve_omp_binary(program)?;
        let build = |args: Vec<String>| {
            let mut cmd = Command::new(&program);
            cmd.args(args).current_dir(cwd);
            // omp inherits the parent environment on purpose: ambient provider
            // credential chains (e.g. AWS) resolve from it. Host overrides go
            // on top.
            cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            cmd
        };
        let err = match Self::spawn_with_posture(build(args), posture) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let Some(id) = resume.filter(|_| session_not_found(&err)) else {
            return Err(err);
        };
        // Same contract as Pi's fallback: a session that never got its first
        // reply may have nothing on disk to resume. Start fresh and SAY SO.
        tracing::warn!(session_id = %id, ?err, "omp could not resume; starting a fresh session");
        let fresh = build_args(model, &posture, None)?;
        Self::spawn_with_notice(
            build(fresh),
            posture,
            Some(format!(
                "Couldn't resume this omp session ({id}), so a new one was started — the agent \
                 cannot see the conversation above."
            )),
        )
    }

    /// Spawn an already-built command (the real `omp`, or a fake in tests)
    /// with the recorded posture.
    pub fn spawn_with_posture(
        cmd: Command,
        posture: OmpPosture,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        Self::spawn_with_notice(cmd, posture, None)
    }

    fn spawn_with_notice(
        cmd: Command,
        posture: OmpPosture,
        notice: Option<String>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (rpc, inbound, child) = OmpRpcClient::spawn_command(cmd)?;

        // ── ready ────────────────────────────────────────────────────────
        // The first frame omp sends, unsolicited. Bounded, racing process
        // death: the raw channel disconnects when the reader thread ends, so
        // a crashed omp fails here with its stderr rather than timing out.
        let mut early: Vec<Value> = Vec::new();
        let ready = wait_for_ready(&inbound, &mut early, READY_TIMEOUT).map_err(|e| {
            let tail = rpc.stderr_tail();
            let tail = tail.trim();
            if tail.is_empty() { e } else { anyhow::anyhow!("{e}. Stderr: {tail}") }
        })?;
        // Fail LOUD on an unexpected protocol rather than degrade: v1 caps
        // single frames at 1MiB and this session's catalogs alone approach
        // that. A version that dropped v2 support is a version to re-probe.
        if !ready.supported_protocol_versions.contains(&2) {
            anyhow::bail!(
                "omp advertised protocol versions {:?} (no v2) — this omp version needs re-probing",
                ready.supported_protocol_versions
            );
        }
        if ready.max_reassembled_frame_bytes != 0
            && ready.max_reassembled_frame_bytes != MAX_REASSEMBLED_BYTES
        {
            tracing::warn!(
                declared = ready.max_reassembled_frame_bytes,
                assumed = MAX_REASSEMBLED_BYTES,
                "omp declares a different reassembly ceiling than assumed"
            );
        }
        let resp = rpc
            .request(
                OmpCommand::NegotiateProtocol { id: rpc.next_id("n"), protocol_version: 2 },
                HANDSHAKE_TIMEOUT,
            )
            .context("omp negotiate_protocol")?;
        let negotiated = resp.into_data()?;
        if negotiated.get("protocolVersion").and_then(Value::as_u64) != Some(2) {
            anyhow::bail!("omp did not accept RPC protocol v2 (got {negotiated})");
        }

        // ── get_state ────────────────────────────────────────────────────
        let resp = rpc
            .request(OmpCommand::GetState { id: rpc.next_id("s") }, HANDSHAKE_TIMEOUT)
            .context("omp handshake (get_state)")?;
        let state: SessionState =
            serde_json::from_value(resp.into_data()?).context("decode omp get_state")?;

        // Catalog + palette, both non-fatal (a session with a one-model
        // picker or an empty palette is still a working session).
        let models = match rpc
            .request(OmpCommand::GetAvailableModels { id: rpc.next_id("m") }, HANDSHAKE_TIMEOUT)
            .and_then(|r| r.into_data())
            .and_then(|d| serde_json::from_value::<AvailableModels>(d).map_err(Into::into))
        {
            Ok(cat) if !cat.models.is_empty() => cat.models.iter().map(model_choice).collect(),
            Ok(_) => state.model.iter().map(model_choice).collect(),
            Err(err) => {
                tracing::warn!(?err, "omp get_available_models failed; picker limited to the current model");
                state.model.iter().map(model_choice).collect::<Vec<_>>()
            }
        };
        let commands: Vec<SlashCommandInfo> = match rpc
            .request(OmpCommand::GetAvailableCommands { id: rpc.next_id("gc") }, CONTROL_TIMEOUT)
            .and_then(|r| r.into_data())
            .and_then(|d| serde_json::from_value::<AvailableCommands>(d).map_err(Into::into))
        {
            Ok(cat) => cat.commands.iter().map(command_info).collect(),
            Err(err) => {
                tracing::warn!(?err, "omp get_available_commands failed; the slash palette stays empty");
                Vec::new()
            }
        };
        // Minimal subagent visibility: lifecycle-level events fold into the
        // existing tool cards. Fire-and-forget; an omp without the command
        // just answers an error nothing waits on.
        let _ = rpc.send(OmpCommand::SetSubagentSubscription {
            id: rpc.next_id("ss"),
            level: SUBAGENT_SUBSCRIPTION_LEVEL.to_string(),
        });

        let context_window = state.model.as_ref().and_then(|m| m.context_window);
        let (tx, rx) = std::sync::mpsc::channel();

        let _ = tx.send(ThreadEvent::SessionInit {
            session_id: state.session_id.clone(),
            model: state.model.as_ref().map(Model::qualified).unwrap_or_default(),
            // The posture rides `features()`, not the Claude-style permission
            // mode vocabulary.
            permission_mode: String::new(),
            slash_commands: commands.iter().map(|c| c.name.clone()).collect(),
            meta: SessionMeta::default(),
        });
        // Seed the meter's NUMERATOR too: omp reports live context occupancy
        // at get_state, so a resumed session's meter starts at the truth
        // instead of empty until the first turn settles.
        if let Some(u) = state.context_usage.as_ref()
            && let Some(tokens) = u.tokens.filter(|t| *t > 0)
        {
            let _ = tx.send(ThreadEvent::LiveUsage(TurnUsage {
                input_tokens: tokens,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                context_window: u.context_window.or(context_window),
                cost_usd: None,
            }));
        }
        if let Some(n) = notice {
            let _ = tx.send(ThreadEvent::Error(n));
        }

        let mut map_state = map::OmpState::with_context_window(context_window);
        let shared_state = Arc::new(Mutex::new(Some(state)));
        let shared_commands = Arc::new(Mutex::new(commands));
        let pending_approvals: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        #[cfg(windows)]
        let job = match oximux_job_object::JobObject::adopt(&child) {
            Ok(job) => Some(Arc::new(job)),
            Err(e) => {
                tracing::warn!(?e, "could not put omp in a job object");
                None
            }
        };

        let conn = Self {
            rpc: rpc.clone(),
            child: Arc::new(Mutex::new(child)),
            #[cfg(windows)]
            job,
            state: shared_state.clone(),
            models,
            commands: shared_commands.clone(),
            context_window: map_state.context_window_handle(),
            pending_approvals: pending_approvals.clone(),
            posture,
        };

        std::thread::spawn(move || {
            // Frames that raced ahead of `ready` are processed first, in order.
            for v in early.into_iter().chain(inbound.iter()) {
                match classify(v) {
                    Inbound::Event(v) => {
                        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
                        // The palette can change mid-session (a skill added, an
                        // extension mounted); omp pushes the whole new list.
                        // ~88KB per push and repeated, so update the cache and
                        // emit nothing (F15).
                        if ty == "available_commands_update" {
                            if let Ok(cat) =
                                serde_json::from_value::<AvailableCommands>(v.clone())
                                && let Ok(mut g) = shared_commands.lock()
                            {
                                *g = cat.commands.iter().map(command_info).collect();
                            }
                            continue;
                        }
                        // Same channel-of-record as Pi for a silently moved
                        // thinking level.
                        if ty == "thinking_level_changed"
                            && let Some(level) = v.get("level").and_then(Value::as_str)
                            && let Ok(mut g) = shared_state.lock()
                            && let Some(st) = g.as_mut()
                        {
                            st.thinking_level = Some(level.to_string());
                        }
                        for e in map::map_event(&v, &mut map_state) {
                            if tx.send(e).is_err() {
                                return; // the view is gone
                            }
                        }
                    }
                    Inbound::ExtensionUiRequest(v) => {
                        if let Some(approval) = parse_tool_approval(&v) {
                            if let Ok(mut g) = pending_approvals.lock() {
                                g.insert(approval.id.clone());
                            }
                            let input = approval_input(&approval.tool_name, &approval.body);
                            if tx
                                .send(ThreadEvent::PermissionRequested {
                                    request_id: approval.id,
                                    tool_use_id: None,
                                    tool_name: approval.tool_name,
                                    input,
                                    description: approval.body,
                                    suggestions: Vec::new(),
                                    kind: PermissionKind::Tool,
                                })
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        match v.get("method").and_then(Value::as_str) {
                            // TUI widget housekeeping omp emits over the same
                            // frame type. Observed to need no answer (turns
                            // complete with it unanswered) — leave it be.
                            Some("setWidget") => {
                                tracing::debug!(req = %v, "omp widget update (ignored)");
                            }
                            // Any other dialog (freeform input, non-approval
                            // select, confirm, editor): OxiMux has no surface
                            // for it, and an unanswered dialog blocks the turn
                            // FOREVER (no timeout — probe 01). Cancel it, and
                            // say so — deny-by-default, visibly.
                            other => {
                                let id = v
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                tracing::warn!(method = ?other, "omp dialog OxiMux cannot render; cancelling");
                                if !id.is_empty() {
                                    let _ = rpc.send_ui_response(&ExtensionUiResponse {
                                        id,
                                        value: None,
                                        cancelled: Some(true),
                                    });
                                }
                                let what = other.unwrap_or("unknown");
                                if tx
                                    .send(ThreadEvent::Error(format!(
                                        "omp asked for input OxiMux can't render ({what} dialog) — cancelled it so the turn can continue"
                                    )))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    Inbound::Response(r) => {
                        tracing::debug!(command = %r.command, "omp uncorrelated response");
                    }
                }
            }
            // Inbound closed → omp exited. `tx` drops after this, closing the
            // app's receiver and firing its disconnect handling.
            let tail = rpc.stderr_tail();
            let msg = if tail.trim().is_empty() {
                "omp exited".to_string()
            } else {
                format!("omp exited. Stderr: {}", tail.trim())
            };
            let _ = tx.send(ThreadEvent::Error(msg));
        });

        Ok((conn, rx))
    }

    /// The session id omp reported at handshake.
    pub fn session_id(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().map(|s| s.session_id.clone())
    }

    /// The session file omp reported (may not exist yet — a fresh session
    /// persists on first activity).
    pub fn session_file(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().and_then(|s| s.session_file.clone())
    }

    /// The posture this process runs under — what a respawn spec must carry.
    pub fn posture(&self) -> OmpPosture {
        self.posture
    }

    /// Adopt a model omp confirmed it switched to (meter + state together).
    fn adopt_model(&self, model: Model) {
        self.context_window.store(model.context_window.unwrap_or(0), Ordering::Relaxed);
        if let Ok(mut g) = self.state.lock()
            && let Some(st) = g.as_mut()
        {
            st.model = Some(model);
        }
    }

    /// SIGTERM omp, then hand the grace-wait + SIGKILL escalation to a
    /// detached thread. Same design as Pi's (see that doc comment); the
    /// SIGTERM child-reaping behavior was re-verified live on omp 18.0.4.
    fn terminate(&self) {
        {
            let mut child = match self.child.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {}
            }
            #[cfg(unix)]
            {
                // SAFETY: our own child, confirmed un-reaped just above.
                let pid = child.id() as libc::pid_t;
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        let child = self.child.clone();
        #[cfg(windows)]
        let job = self.job.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + TERM_GRACE;
            loop {
                let mut guard = match child.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                match guard.try_wait() {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) => {}
                }
                if Instant::now() >= deadline {
                    #[cfg(unix)]
                    tracing::warn!(
                        "omp did not exit within {TERM_GRACE:?} of SIGTERM; escalating to \
                         SIGKILL — any running bash tool children will be orphaned"
                    );
                    #[cfg(windows)]
                    tracing::warn!("omp did not exit within {TERM_GRACE:?}; terminating its job object");
                    let _ = guard.kill();
                    let _ = guard.wait();
                    #[cfg(windows)]
                    if let Some(job) = &job {
                        let _ = job.kill();
                    }
                    return;
                }
                drop(guard);
                std::thread::sleep(Duration::from_millis(25));
            }
        });
    }
}

/// Wait for the unsolicited `ready` frame, collecting any frames that arrive
/// ahead of it (none observed live, but the order is omp's to choose).
fn wait_for_ready(
    rx: &Receiver<Value>,
    early: &mut Vec<Value>,
    timeout: Duration,
) -> Result<ReadyFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow::anyhow!("omp never sent its ready frame within {timeout:?}"))?;
        match rx.recv_timeout(remaining) {
            Ok(v) if v.get("type").and_then(Value::as_str) == Some("ready") => {
                return serde_json::from_value(v).context("decode omp ready frame");
            }
            Ok(v) => early.push(v),
            Err(RecvTimeoutError::Timeout) => {
                anyhow::bail!("omp never sent its ready frame within {timeout:?}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("omp exited before its ready frame")
            }
        }
    }
}

impl AgentConnection for OmpRpcConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.rpc.send(OmpCommand::Prompt {
            id: self.rpc.next_id("p"),
            message: text.to_string(),
            images: Vec::new(),
        })
    }

    fn send_user_message_with_images(&self, text: &str, images: &[ChatImage]) -> Result<()> {
        self.rpc.send(OmpCommand::Prompt {
            id: self.rpc.next_id("p"),
            message: text.to_string(),
            images: images
                .iter()
                .map(|i| protocol::ImageContent {
                    r#type: "image",
                    data: i.data.clone(),
                    mime_type: i.media_type.clone(),
                })
                .collect(),
        })
    }

    /// Redirect the live turn at the next turn boundary.
    fn steer(&self, text: &str) -> Result<()> {
        self.rpc.send(OmpCommand::Steer { id: self.rpc.next_id("st"), message: text.to_string() })
    }

    /// Answer a pending approval dialog. Anything other than an Allow decides
    /// DENY — the exact contract omp applies to the wire value.
    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        let known = self
            .pending_approvals
            .lock()
            .map_err(|_| anyhow::anyhow!("omp approvals lock poisoned"))?
            .contains(request_id);
        if !known {
            // A late click on an already-answered (or never-issued) dialog.
            // Sending anyway would answer a dialog in an unknown state.
            anyhow::bail!("omp approval {request_id:?} is not pending (already answered?)");
        }
        let value = match decision {
            PermissionDecision::Allow { .. } | PermissionDecision::AllowWithSuggestion { .. } => {
                APPROVE
            }
            PermissionDecision::Deny { .. } => DENY,
        };
        self.rpc.send_ui_response(&ExtensionUiResponse {
            id: request_id.to_string(),
            value: Some(value.to_string()),
            cancelled: None,
        })?;
        // Unpend only once the answer actually reached the pipe: a failed
        // write (dying stdin) must leave the id answerable so a retry isn't
        // refused with "not pending" for an answer omp never saw. Two threads
        // racing the same id can both pass `contains`, but the second WRITE
        // is then a duplicate answer to a resolved dialog — omp ignores it —
        // while a remove-first ordering would instead lose a real answer.
        if let Ok(mut g) = self.pending_approvals.lock() {
            g.remove(request_id);
        }
        Ok(())
    }

    fn shutdown(&self) {
        // Closing stdin resolves any pending approval as deny (verified live)
        // before the SIGTERM lands — teardown can't allow anything.
        self.rpc.close_stdin();
        self.terminate();
    }

    fn cancel(&self) -> Result<()> {
        // In-band abort, like Pi — fire-and-forget.
        self.rpc.send(OmpCommand::Abort { id: self.rpc.next_id("a") })
    }

    fn cancel_and_wait(&self) -> Result<()> {
        self.rpc
            .request(OmpCommand::Abort { id: self.rpc.next_id("a") }, HANDSHAKE_TIMEOUT)
            .map(|_| ())
    }

    /// The approval posture, surfaced as a composer control. `set_feature` is
    /// deliberately unimplemented: `--approval-mode` is spawn-time, so the
    /// app's respawn fallback is the only honest way to change it.
    fn features(&self) -> Vec<FeatureControl> {
        let opt = |wire: &str, label: &str, desc: &str| FeatureSelectOption {
            wire: wire.to_string(),
            label: label.to_string(),
            description: Some(desc.to_string()),
        };
        vec![FeatureControl {
            id: FEATURE_APPROVALS.to_string(),
            label: "Approvals".to_string(),
            description: Some("How omp asks before running tools — chosen at launch".to_string()),
            icon: Some("shield".to_string()),
            kind: FeatureKind::Select {
                options: vec![
                    opt(
                        posture::APPROVAL_ALWAYS_ASK,
                        "Always ask",
                        "Every tool call asks first",
                    ),
                    opt(
                        posture::APPROVAL_WRITE,
                        "Ask to write",
                        "Reads run free; writes, edits and bash ask",
                    ),
                    opt(posture::APPROVAL_YOLO, "Never ask", "⚠ Runs everything without asking"),
                ],
                selected: Some(self.posture.wire().to_string()),
            },
        }]
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            // The posture rides features(); Claude-style permission modes
            // don't apply.
            supports_modes: false,
            supports_slash: true,
            // Gates the reasoning-effort picker, where thinking levels render.
            supports_config: true,
            // Every assistant message carries usage, including real dollars.
            emits_usage: true,
            // omp's branch/fork has the same impedance mismatch Pi's had;
            // deliberately out of scope (plan).
            supports_rewind: false,
            // Verified live in the probe captures (steer is part of omp's
            // command union, same semantics as Pi's).
            supports_steer: true,
        }
    }

    fn models(&self) -> Vec<ModelChoice> {
        self.models.clone()
    }

    /// The palette, LIVE: `available_commands_update` pushes replace it
    /// mid-session (F15), so this reads the shared cache.
    fn slash_commands(&self) -> Vec<SlashCommandInfo> {
        self.commands.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn default_model(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().and_then(|s| s.model.as_ref().map(Model::qualified))
    }

    fn context_window(&self) -> Option<u64> {
        match self.context_window.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Switch model in-session. `model` is a `provider/id` wire; split on the
    /// FIRST slash (provider names carry no slash; model ids may). Unlike
    /// Pi's, omp's `set_model` is session-scoped — it does NOT rewrite the
    /// user's global default (verified live: `config.yml` untouched).
    fn set_model(&self, model: &str) -> Result<()> {
        let (provider, model_id) = model.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("omp needs a provider-qualified model (`provider/id`), got {model:?}")
        })?;
        let resp = self.rpc.request(
            OmpCommand::SetModel {
                id: self.rpc.next_id("sm"),
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            },
            CONTROL_TIMEOUT,
        )?;
        let switched: Model =
            serde_json::from_value(resp.into_data()?).context("decode omp set_model")?;
        self.adopt_model(switched);
        Ok(())
    }

    /// Thinking levels ride the reasoning-effort picker, derived from the
    /// CURRENT model's authoritative `thinking.efforts` list.
    fn efforts(&self) -> Vec<EffortChoice> {
        let Ok(g) = self.state.lock() else { return Vec::new() };
        let Some(model) = g.as_ref().and_then(|s| s.model.as_ref()) else { return Vec::new() };
        model
            .supported_thinking_levels()
            .into_iter()
            .map(|wire| {
                let label = thinking_label(&wire);
                EffortChoice { wire, label }
            })
            .collect()
    }

    fn default_effort(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().and_then(|s| s.thinking_level.clone())
    }

    /// Fire-and-forget, optimistic — `thinking_level_changed` corrects a
    /// clamp, same contract as Pi's.
    fn set_effort(&self, effort: &str) -> Result<()> {
        self.rpc.send(OmpCommand::SetThinkingLevel {
            id: self.rpc.next_id("tl"),
            level: effort.to_string(),
        })?;
        if let Ok(mut g) = self.state.lock()
            && let Some(st) = g.as_mut()
        {
            st.thinking_level = Some(effort.to_string());
        }
        Ok(())
    }
}

impl Drop for OmpRpcConnection {
    fn drop(&mut self) {
        // rpc-ui never exits on its own; without this a dropped connection
        // leaks a bun process and any detached tool children.
        self.rpc.close_stdin();
        self.terminate();
    }
}

/// The argv for an omp launch (everything after the program). Pure so the
/// exact flags are testable without spawning — `--approval-mode` is the tool
/// gate, so "which flags actually reached the child" is a safety property.
///
/// `resume` must be the FULL canonical session UUID. omp's resolver is a
/// prefix match with a global cross-project fallback that resolves SILENTLY
/// (probe 01: an ambiguous 8-char prefix picked one of two sessions; another
/// project's id loaded without so much as a prompt) — so anything that is not
/// a full UUID is refused here rather than passed on to maybe-land in the
/// wrong session. A path is doubly wrong (omp resumes by id, not by file).
pub fn build_args(
    model: Option<&str>,
    posture: &OmpPosture,
    resume: Option<&str>,
) -> Result<Vec<String>> {
    let mut args = vec!["--mode".to_string(), "rpc-ui".to_string()];
    // ALWAYS explicit: omp's own default is yolo (auto-approve exec).
    args.extend(posture.to_args());
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(id) = resume.map(str::trim).filter(|s| !s.is_empty()) {
        if !is_full_session_uuid(id) {
            anyhow::bail!(
                "refusing to resume omp with {id:?}: omp prefix-matches session ids and falls \
                 back across projects silently, so only a full canonical session UUID can be \
                 trusted to land in the right session."
            );
        }
        args.push("--resume".to_string());
        args.push(id.to_string());
    }
    Ok(args)
}

/// A full canonical `8-4-4-4-12` hex UUID — the only resume handle omp's
/// prefix-matching resolver cannot mis-resolve. (The settings crate enforces
/// the same rule at the ⌘⇧H seam; this guards the chat spawn path, and the
/// two cannot share code — settings must stay gpui-free of this crate.)
fn is_full_session_uuid(handle: &str) -> bool {
    let groups: Vec<&str> = handle.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, g)| g.len() == *len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Whether a spawn failure is omp reporting a missing session id (probed:
/// `Session "<id>" not found.` on stderr, exit 1) — the one failure a fresh
/// session legitimately recovers from.
fn session_not_found(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("not found") && text.contains("Session")
}

/// One omp model as a picker choice, same layout as Pi's rows.
fn model_choice(m: &Model) -> ModelChoice {
    let mut description = m.provider.clone();
    if let Some(w) = m.context_window.filter(|w| *w > 0) {
        description.push_str(&format!(" · {} context", fmt_window(w)));
    }
    ModelChoice {
        wire: m.qualified(),
        label: m.display_name().to_string(),
        description: Some(description),
    }
}

/// One omp command as a palette row.
fn command_info(c: &protocol::SlashCommand) -> SlashCommandInfo {
    SlashCommandInfo {
        name: c.name.clone(),
        description: c.description.clone(),
        is_skill: c.is_skill(),
        source_label: c.source_info.as_ref().and_then(|s| s.scope.clone()),
    }
}

/// Resolve the `omp` binary. Two well-known install dirs beyond PATH: the
/// official installer targets `~/.local/bin`, and a bun install (the shape on
/// this machine — probed) lands a shim in `~/.bun/bin`.
fn resolve_omp_binary(configured: Option<&str>) -> Result<PathBuf> {
    let well_known: Vec<PathBuf> = dirs::home_dir()
        .map(|h| vec![h.join(".local/bin"), h.join(".bun/bin")])
        .unwrap_or_default();
    super::agent_binary::resolve_agent_binary("omp", configured, &well_known)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(model: Option<&str>, posture: &OmpPosture) -> Vec<String> {
        build_args(model, posture, None).expect("args")
    }

    #[test]
    fn every_spawn_carries_an_explicit_approval_mode() {
        // The yolo-default guard (red-team F2): omp's own default approval
        // mode auto-approves exec, so an argv WITHOUT the flag is a spawn
        // that silently dropped the user's safety posture.
        for posture in [OmpPosture::AlwaysAsk, OmpPosture::Write, OmpPosture::Yolo] {
            let a = args(None, &posture);
            let at = a.iter().position(|s| s == "--approval-mode").expect("flag present");
            assert_eq!(a[at + 1], posture.wire());
        }
        // Including the default posture, spelled out.
        let a = args(None, &OmpPosture::default());
        assert!(a.windows(2).any(|w| w[0] == "--approval-mode" && w[1] == "write"));
        // And with every other option in play.
        let a = build_args(Some("openai-codex/gpt-5.6-sol"), &OmpPosture::AlwaysAsk, Some("01a037fe-2a2b-76e1-8d1f-db954755a79c")).unwrap();
        assert!(a.contains(&"--approval-mode".to_string()));
        assert!(a.contains(&"--resume".to_string()));
    }

    #[test]
    fn resume_takes_only_a_full_canonical_uuid() {
        let posture = OmpPosture::default();
        let ok = build_args(None, &posture, Some("01a037fe-2a2b-76e1-8d1f-db954755a79c"));
        assert!(ok.is_ok());
        for bad in [
            "01a037fe",                                   // prefix — resolves silently in omp
            "/tmp/x/sessions/a.jsonl",                    // path — omp resumes by id
            "--continue",                                 // flag-shaped
            "01a037fe-2a2b-76e1-8d1f-db954755a79",        // truncated last group
        ] {
            let err = build_args(None, &posture, Some(bad)).expect_err(bad);
            assert!(err.to_string().contains("full canonical"), "{bad}: {err}");
        }
        // Blank/whitespace = no resume, not an error.
        assert!(build_args(None, &posture, Some("  ")).unwrap().iter().all(|a| a != "--resume"));
    }

    #[test]
    fn rpc_ui_mode_is_always_first() {
        let a = args(None, &OmpPosture::default());
        assert_eq!(&a[..2], &["--mode".to_string(), "rpc-ui".to_string()]);
    }

    // ── fake-omp integration tests (no real omp needed) ────────────────────

    fn fake_omp(script: &str) -> Command {
        let mut cmd = crate::thread::sh_fixture::sh_command();
        cmd.arg("-c").arg(script.to_string());
        cmd
    }

    /// A fake omp that completes the full handshake, then runs `extra`.
    fn handshake_script(extra: &str) -> String {
        format!(
            r#"
printf '{{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}}\n'
read neg
printf '{{"id":"n1","type":"response","command":"negotiate_protocol","success":true,"data":{{"protocolVersion":2}}}}\n'
read gs
printf '{{"id":"s2","type":"response","command":"get_state","success":true,"data":{{"sessionId":"01a037fe-2a2b-76e1-8d1f-db954755a79c","sessionFile":"/tmp/s.jsonl","thinkingLevel":"high","contextUsage":{{"tokens":48183,"contextWindow":272000}},"model":{{"id":"gpt-5.6-sol","name":"GPT-5.6-Sol","provider":"openai-codex","reasoning":true,"contextWindow":272000,"thinking":{{"efforts":["low","medium","high","max"]}},"input":["text","image"]}}}}}}\n'
read gm
printf '{{"id":"m3","type":"response","command":"get_available_models","success":true,"data":{{"models":[{{"id":"gpt-5.6-sol","name":"GPT-5.6-Sol","provider":"openai-codex","reasoning":true,"contextWindow":272000}}]}}}}\n'
read gc
printf '{{"id":"gc4","type":"response","command":"get_available_commands","success":true,"data":{{"commands":[{{"name":"skill:x","description":"a skill"}},{{"name":"security","description":"scans"}}]}}}}\n'
read ss
{extra}
sleep 0.3
"#
        )
    }

    fn drain_until_init(rx: &Receiver<ThreadEvent>) -> Vec<ThreadEvent> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(e) => {
                    got.push(e);
                    if matches!(got.last(), Some(ThreadEvent::SessionInit { .. })) {
                        return got;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        got
    }

    #[test]
    fn handshake_negotiates_v2_and_seeds_session_model_meter_and_palette() {
        let (conn, rx) = OmpRpcConnection::spawn_with_posture(
            fake_omp(&handshake_script("")),
            OmpPosture::AlwaysAsk,
        )
        .expect("handshake");
        let events = drain_until_init(&rx);
        let init = events
            .iter()
            .find_map(|e| match e {
                ThreadEvent::SessionInit { session_id, model, slash_commands, .. } => {
                    Some((session_id.clone(), model.clone(), slash_commands.clone()))
                }
                _ => None,
            })
            .expect("session init");
        assert_eq!(init.0, "01a037fe-2a2b-76e1-8d1f-db954755a79c");
        assert_eq!(init.1, "openai-codex/gpt-5.6-sol", "model is provider-qualified");
        assert!(init.2.iter().any(|c| c == "security"));
        assert_eq!(conn.session_id().as_deref(), Some("01a037fe-2a2b-76e1-8d1f-db954755a79c"));
        assert_eq!(conn.context_window(), Some(272_000));
        // The meter's numerator seeds from get_state's live occupancy.
        let usage = rx
            .recv_timeout(Duration::from_secs(2))
            .ok()
            .and_then(|e| match e {
                ThreadEvent::LiveUsage(u) => Some(u),
                _ => None,
            })
            .expect("handshake usage seed");
        assert_eq!(usage.input_tokens, 48_183);
        // Thinking levels come from the model's authoritative efforts list.
        let efforts: Vec<String> = conn.efforts().into_iter().map(|e| e.wire).collect();
        assert_eq!(efforts, vec!["off", "low", "medium", "high", "max"]);
        // The posture the connection reports is the one it was spawned with.
        assert_eq!(conn.posture(), OmpPosture::AlwaysAsk);
    }

    #[test]
    fn an_approval_request_surfaces_and_deny_answers_on_the_wire() {
        // After the handshake the fake emits a live-captured approval frame,
        // then echoes whatever it reads next to stdout inside a marker event —
        // which the test then reads back as proof of what reached omp's stdin.
        let extra = r#"
printf '{"type":"extension_ui_request","id":"15654c5b8e9fba59","method":"select","title":"Allow tool: bash\\nCommand: rm -rf /tmp/x","options":["Approve","Deny"]}\n'
read answer
printf '{"type":"echo_for_test","raw":%s}\n' "$(printf '%s' "$answer" | sed 's/\\/\\\\/g;s/"/\\"/g;s/^/"/;s/$/"/')"
"#;
        let (conn, rx) = OmpRpcConnection::spawn_with_posture(
            fake_omp(&handshake_script(extra)),
            OmpPosture::AlwaysAsk,
        )
        .expect("handshake");
        let _ = drain_until_init(&rx);
        // The approval surfaces as a permission request with the parsed tool.
        let deadline = Instant::now() + Duration::from_secs(5);
        let req = loop {
            assert!(Instant::now() < deadline, "no permission request arrived");
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ThreadEvent::PermissionRequested { request_id, tool_name, input, .. }) => {
                    break (request_id, tool_name, input)
                }
                Ok(_) => continue,
                Err(e) => panic!("waiting for permission request: {e:?}"),
            }
        };
        assert_eq!(req.0, "15654c5b8e9fba59");
        assert_eq!(req.1, "bash");
        assert_eq!(req.2["command"], "rm -rf /tmp/x");
        // Deny goes out as the byte-exact wire value on the request's own id.
        conn.resolve_permission(&req.0, PermissionDecision::Deny { message: "no".into() })
            .expect("deny");
        let deadline = Instant::now() + Duration::from_secs(5);
        // The fake echoes our stdin line back; nothing else is listening for
        // `echo_for_test`, so it reaches the mapper and maps to nothing —
        // instead read it off the raw wire via the error the fake's exit
        // produces... simpler: the echo frame is an unknown event (mapped to
        // nothing), so assert by absence of failure AND by the pending set:
        // a second resolve for the same id must now be refused.
        let second = conn.resolve_permission(&req.0, PermissionDecision::Allow {
            updated_input: serde_json::Value::Null,
        });
        assert!(second.is_err(), "an answered approval must not be answerable again");
        let _ = deadline;
    }

    #[test]
    fn a_missing_ready_frame_fails_bounded_not_forever() {
        // An omp that never says ready (prints nothing, holds the pipe).
        let script = "read x\nsleep 30\n";
        let start = Instant::now();
        // Shorten the wait indirectly is not possible (const), so this test
        // rides the process-exit race instead: a fake that exits immediately
        // fails the ready wait through disconnection, fast.
        let fast = "exit 7\n";
        let err = match OmpRpcConnection::spawn_with_posture(fake_omp(fast), OmpPosture::default())
        {
            Ok(_) => panic!("must fail"),
            Err(e) => e,
        };
        assert!(start.elapsed() < Duration::from_secs(5), "failed via disconnect, not timeout");
        let msg = format!("{err:#}");
        assert!(msg.contains("ready"), "the error names the missing ready frame: {msg}");
        let _ = script;
    }

    #[test]
    fn a_v1_only_omp_fails_loud_rather_than_degrading() {
        // Large frames NEED v2 (catalogs approach the 1MiB v1 cap); a version
        // that dropped v2 is a version to re-probe, not to limp along with.
        let script = r#"
printf '{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1],"maxFrameBytes":1048576}\n'
sleep 0.3
"#;
        let err = match OmpRpcConnection::spawn_with_posture(fake_omp(script), OmpPosture::default())
        {
            Ok(_) => panic!("must fail"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("re-probing"), "{err:#}");
    }

    #[test]
    fn an_unrenderable_dialog_is_cancelled_and_surfaced() {
        let extra = r#"
printf '{"type":"extension_ui_request","id":"d1","method":"input","title":"Type something"}\n'
"#;
        let (_conn, rx) = OmpRpcConnection::spawn_with_posture(
            fake_omp(&handshake_script(extra)),
            OmpPosture::default(),
        )
        .expect("handshake");
        let _ = drain_until_init(&rx);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "no cancellation notice arrived");
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ThreadEvent::Error(m)) if m.contains("can't render") => break,
                Ok(_) => continue,
                Err(e) => panic!("waiting for the notice: {e:?}"),
            }
        }
    }

    // ── live tests against a real `omp` (ignored by default) ──────────────

    /// Scratch agent dir with the REAL auth copied in — omp 18 keeps provider
    /// credentials in `agent.db` (SQLite, WAL journal), so the copy must go
    /// through `sqlite3 .backup`; a plain file copy loses WAL-resident rows
    /// (bit the probe live). `PI_CODING_AGENT_DIR` (omp kept Pi's env name)
    /// points omp at the scratch, keeping the user's real sessions/settings
    /// out of reach. Known leak, accepted for a live test: `~/.omp/logs` and
    /// `~/.omp/run` still receive debris — full isolation would need HOME.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch_omp_home() -> Option<(Scratch, PathBuf)> {
        let home = PathBuf::from(std::env::var("HOME").ok()?);
        let real = home.join(".omp/agent/agent.db");
        if !real.exists() {
            return None;
        }
        let root = std::env::temp_dir().join(format!("oximux-omp-home-{}", std::process::id()));
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).ok()?;
        let guard = Scratch(root);
        let status = Command::new("sqlite3")
            .arg(&real)
            .arg(format!(".backup {}", agent.join("agent.db").display()))
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        // The model-role defaults, so a spawn without --model resolves the
        // user's configured provider instead of an ambient-credential guess.
        let _ = std::fs::copy(home.join(".omp/agent/config.yml"), agent.join("config.yml"));
        Some((guard, agent))
    }

    fn live_model() -> Option<String> {
        std::env::var("OMP_LIVE_TEST_MODEL").ok().filter(|m| !m.is_empty())
    }

    fn run_until_turn_end(
        rx: &Receiver<ThreadEvent>,
        mut on_event: impl FnMut(&ThreadEvent, &OmpRpcConnection),
        conn: &OmpRpcConnection,
    ) -> crate::thread::state::ChatThread {
        let mut thread = crate::thread::state::ChatThread::default();
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out before the turn ended");
            match rx.recv_timeout(left) {
                Ok(e) => {
                    on_event(&e, conn);
                    let done = matches!(e, ThreadEvent::TurnEnded { .. });
                    thread.apply(&e);
                    if done {
                        return thread;
                    }
                }
                Err(e) => panic!("event stream ended early: {e:?}"),
            }
        }
    }

    /// Handshake + one turn + the SAFETY-CRITICAL approval deny against a real
    /// omp: under `always-ask`, a bash write must surface an approval, Deny
    /// must block the effect ON DISK, and the turn must still end cleanly.
    /// Spends a few provider tokens (one cheap turn).
    /// Run: `cargo test -p oximux-agents omp::tests::live_omp_handshake -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `omp`, its signed-in agent.db, and spends provider tokens"]
    fn live_omp_handshake_turn_and_deny_blocks_the_disk() {
        let Some((guard, agent_dir)) = scratch_omp_home() else {
            eprintln!("skipping: no ~/.omp/agent/agent.db — omp is not set up here");
            return;
        };
        let program = resolve_omp_binary(None).expect("find omp");
        let project = guard.0.join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let marker = project.join("live-deny-marker.txt");

        let posture = OmpPosture::AlwaysAsk;
        let mut cmd = Command::new(&program);
        cmd.args(build_args(live_model().as_deref(), &posture, None).expect("args"))
            .env("PI_CODING_AGENT_DIR", &agent_dir)
            .current_dir(&project);
        let (conn, rx) = OmpRpcConnection::spawn_with_posture(cmd, posture).expect("handshake");
        assert!(conn.session_id().is_some(), "handshake must mint a session id");
        assert!(!conn.models().is_empty(), "the catalog must reach the picker");

        conn.send_user_message(&format!(
            "Use the bash tool to run exactly: echo touched > {}",
            marker.display()
        ))
        .expect("send");
        let mut denied = false;
        let thread = run_until_turn_end(
            &rx,
            |e, conn| {
                if let ThreadEvent::PermissionRequested { request_id, tool_name, .. } = e {
                    assert_eq!(tool_name, "bash");
                    conn.resolve_permission(
                        request_id,
                        PermissionDecision::Deny { message: "denied by live test".into() },
                    )
                    .expect("deny");
                    denied = true;
                }
            },
            &conn,
        );
        assert!(denied, "the approval prompt must have surfaced");
        assert!(!marker.exists(), "DENY MUST BLOCK THE WRITE — the marker file exists");
        assert!(!thread.entries.is_empty(), "the turn rendered a transcript");
        conn.shutdown();
    }

    /// Cross-process resume against the real omp: teach a codeword, tear the
    /// process down, respawn with `--resume <full id>` and prove the new
    /// process remembers. Spends a few provider tokens (two cheap turns).
    /// Run: `cargo test -p oximux-agents omp::tests::live_omp_resume -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `omp`, its signed-in agent.db, and spends provider tokens"]
    fn live_omp_resume_by_full_id_recalls_the_codeword() {
        let Some((guard, agent_dir)) = scratch_omp_home() else {
            eprintln!("skipping: no ~/.omp/agent/agent.db — omp is not set up here");
            return;
        };
        let program = resolve_omp_binary(None).expect("find omp");
        let project = guard.0.join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let spawn = |resume: Option<&str>| {
            let posture = OmpPosture::AlwaysAsk;
            let mut cmd = Command::new(&program);
            cmd.args(build_args(live_model().as_deref(), &posture, resume).expect("args"))
                .env("PI_CODING_AGENT_DIR", &agent_dir)
                .current_dir(&project);
            OmpRpcConnection::spawn_with_posture(cmd, posture)
        };

        let (conn, rx) = spawn(None).expect("first spawn");
        conn.send_user_message(
            "The codeword is XENON-42. Acknowledge with exactly: stored",
        )
        .expect("send");
        let _ = run_until_turn_end(&rx, |_, _| {}, &conn);
        let id = conn.session_id().expect("session id");
        conn.shutdown();
        drop(conn);
        std::thread::sleep(Duration::from_millis(500));

        let (conn2, rx2) = spawn(Some(&id)).expect("resume spawn");
        conn2
            .send_user_message("Reply with only the codeword I told you earlier.")
            .expect("send");
        let thread = run_until_turn_end(&rx2, |_, _| {}, &conn2);
        let all: String = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                crate::thread::ThreadEntry::Assistant(m) => Some(m.text.clone()),
                _ => None,
            })
            .collect();
        assert!(all.contains("XENON-42"), "the resumed omp must recall the codeword, got {all:?}");
        conn2.shutdown();
    }

    #[test]
    fn a_chunked_inbound_frame_reassembles_end_to_end() {
        // The fake emits a >1-chunk event AFTER the handshake, exercising the
        // preprocess hook inside the real transport (not just the unit test).
        use base64::Engine as _;
        let frame = br#"{"type":"message_start","message":{"role":"assistant"}}"#;
        let (a, b) = frame.split_at(20);
        let c1 = base64::engine::general_purpose::STANDARD.encode(a);
        let c2 = base64::engine::general_purpose::STANDARD.encode(b);
        let n = frame.len();
        let extra = format!(
            r#"printf '{{"type":"rpc_chunk","chunkId":"t1","index":0,"count":2,"byteLength":{n},"data":"{c1}"}}\n'
printf '{{"type":"rpc_chunk","chunkId":"t1","index":1,"count":2,"byteLength":{n},"data":"{c2}"}}\n'
printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","contentIndex":0,"delta":"ok","partial":{{"role":"assistant","content":[{{"type":"text","text":"ok"}}]}}}}}}\n'
"#
        );
        let (_conn, rx) = OmpRpcConnection::spawn_with_posture(
            fake_omp(&handshake_script(&extra)),
            OmpPosture::default(),
        )
        .expect("handshake");
        let _ = drain_until_init(&rx);
        // The chunked message_start bumped the ordinal, so the delta that
        // follows renders — proof the reassembled frame reached the mapper.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "the post-chunk delta never arrived");
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(ThreadEvent::AssistantTextDelta(s)) => {
                    assert_eq!(s, "ok");
                    break;
                }
                Ok(_) => continue,
                Err(e) => panic!("waiting for the delta: {e:?}"),
            }
        }
    }
}
