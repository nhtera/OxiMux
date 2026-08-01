//! Pi chat backend — drives `pi --mode rpc` as a subprocess.
//!
//! Pi speaks neither ACP nor app-server; `--mode rpc` is its own public,
//! purpose-built embedding protocol (a package export, `"./rpc-entry"`). The
//! Node SDK is deliberately NOT embedded: it is unavailable to Rust, and its one
//! extra capability (`beforeToolCall`) is unused even by pi's own GUI.
//!
//! Layout mirrors `thread/codex/`: [`transport`] owns the pipe, [`protocol`] the
//! wire types, and this module the connection + lifecycle.
//!
//! ## Lifecycle, and why SIGTERM specifically
//!
//! pi registers a handler for **SIGTERM/SIGHUP only** — there is no SIGINT
//! handler, so Claude's SIGINT-to-end-a-turn idiom does not transfer here and is
//! never used. Stopping a turn is the in-band `abort` command.
//!
//! Shutdown must SIGTERM rather than SIGKILL, but *not* to flush the session:
//! there is no shutdown-time session flush at all (`dispose()` aborts and
//! invalidates, persisting nothing). SIGTERM matters because it is the **only**
//! path that runs pi's `killTrackedDetachedChildren()`: pi's bash tool spawns
//! *detached* children and tracks their pids, so a SIGKILL leaves every running
//! tool tree (a dev server, a build) orphaned forever. pi's own orchestrator
//! stops its child the same way.

pub mod map;
pub mod posture;
pub mod protocol;
pub mod transport;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use super::connection::{
    AgentCapabilities, AgentConnection, EffortChoice, FeatureControl, FeatureKind,
    FeatureSelectOption, ModelChoice, SlashCommandInfo,
};
use super::event::{SessionMeta, ThreadEvent};
use super::tool_call::PermissionDecision;
use posture::{PiPosture, FEATURE_CONTEXT_FILES, FEATURE_TOOLS, TOOLS_NONE, TOOLS_READ_ONLY, TOOLS_STANDARD};
use protocol::{AvailableCommands, AvailableModels, Inbound, Model, PiCommand, SessionState};
use transport::PiRpcClient;

/// How long the `get_state` handshake may take. Generous (a cold Node start,
/// or pi resolving provider auth) but bounded so a wedged pi can't hang a new
/// chat forever. The EOF drain covers the crash case far faster than this.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for pi to exit after SIGTERM before escalating. SIGTERM runs
/// the handler that reaps pi's detached tool children; escalating to SIGKILL
/// skips that, so this is a real deadline, not a formality.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on the login-shell binary probe. It runs the user's full rc chain
/// on the UI thread, so it needs a hard ceiling: better an actionable "pi not
/// found" than a frozen app behind someone's `.zshrc`.
const LOGIN_SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound on a user-driven control round-trip (`set_model`). pi answers
/// these locally in milliseconds — no provider call — but the caller is the UI
/// thread, so it still needs a ceiling rather than the handshake's 30s.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// A live `pi --mode rpc` chat session.
pub struct PiRpcConnection {
    rpc: PiRpcClient,
    child: Arc<Mutex<Child>>,
    /// Windows stand-in for the process group. pi's whole shutdown design leans
    /// on SIGTERM reaching its handler so it can reap the *detached* children
    /// its bash tool spawns — there is no Windows equivalent of either half of
    /// that, so the job object is what makes those children accountable to
    /// something. `Arc` because `terminate` hands the escalation to a detached
    /// thread that outlives the caller.
    #[cfg(windows)]
    job: Option<Arc<oximux_job_object::JobObject>>,
    /// Live session facts: the current model (picker label, thinking levels,
    /// context window) and thinking level. Mutable — `set_model` and
    /// `set_thinking_level` both change it in-session, and pi re-clamps thinking
    /// when the model changes, so this cannot be handshake-only.
    state: Arc<Mutex<Option<SessionState>>>,
    /// pi's catalog, already filtered to providers the user has auth for.
    models: Vec<ModelChoice>,
    /// The slash commands pi advertised, with the description + attribution pi
    /// itself supplied. Read once at the handshake: `get_commands` is a request,
    /// answerable before any turn, so none of the persist-then-seed dance a
    /// backend that only advertises after the first message would force.
    commands: Vec<SlashCommandInfo>,
    /// The meter's denominator, shared with the reader thread's mapper so a
    /// model switch moves it. See [`map::PiState`].
    context_window: Arc<AtomicU64>,
    /// The posture this process was actually spawned with. Fixed for the
    /// process's life — pi's gating is spawn-time, so a change respawns.
    posture: PiPosture,
}

impl PiRpcConnection {
    /// Spawn pi in `cwd` and complete the `get_state` handshake.
    ///
    /// `posture` is fixed here because pi's tool gating is a spawn-time
    /// allowlist — there is no per-call approval to fall back on, so changing it
    /// later means a respawn.
    ///
    /// `resume` is pi's **session id**. It must be an id and not a path — see
    /// [`build_args`]. `cwd` must be the cwd the session was created under: pi
    /// scopes its store per project, and a session id from a different project
    /// is unreachable (see [`build_args`] again).
    pub fn spawn(
        cwd: &Path,
        model: Option<&str>,
        program: Option<&str>,
        posture: PiPosture,
        resume: Option<&str>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let args = self::build_args(model, &posture, resume)?;
        let program = resolve_pi_binary(program)?;
        let build = |args: Vec<String>| {
            let mut cmd = Command::new(&program);
            cmd.args(args).current_dir(cwd);
            // pi inherits the parent environment on purpose: it resolves provider
            // credentials from it. Upstream's own client spawns with `process.env`.
            cmd
        };
        let err = match Self::spawn_with_posture(build(args), posture.clone()) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let Some(id) = resume.filter(|_| session_not_found(&err)) else {
            return Err(err);
        };

        // pi holds a new session in MEMORY until its first assistant message, so
        // a chat whose prompt never got a reply (the app quit, pi crashed) leaves
        // an id that resolves to nothing. Failing here would strand that chat
        // permanently — dead on every reopen — over context that never existed:
        // an unpersisted session has nothing for pi to remember.
        //
        // So fall back to a fresh session, and SAY SO. The silent version of this
        // is exactly the failure this adapter refuses elsewhere (resuming by path
        // mints an empty session and says nothing), and the difference is only
        // that the user is told. OxiMux's own transcript still renders, so the
        // conversation is not lost from view — but the agent cannot see it, and
        // that must not be discovered by watching it answer as a stranger.
        tracing::warn!(session_id = %id, ?err, "pi could not resume; starting a fresh session");
        let fresh = self::build_args(model, &posture, None)?;
        Self::spawn_with_notice(
            build(fresh),
            posture,
            Some(format!(
                "Couldn't resume this Pi session ({id}), so a new one was started — the agent \
                 cannot see the conversation above. Pi keeps a session in memory until its first \
                 reply, so a chat that never got one leaves nothing on disk to resume."
            )),
        )
    }

    /// Spawn an already-built command (the real `pi`, or a fake in tests) with
    /// the default posture.
    pub fn spawn_command(cmd: Command) -> Result<(Self, Receiver<ThreadEvent>)> {
        Self::spawn_with_posture(cmd, PiPosture::default())
    }

    /// Spawn an already-built command, recording the posture it was built with
    /// so `features()` can report the truth rather than a guess.
    pub fn spawn_with_posture(
        cmd: Command,
        posture: PiPosture,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        Self::spawn_with_notice(cmd, posture, None)
    }

    /// As [`Self::spawn_with_posture`], plus a `notice` surfaced in the
    /// transcript right after the session opens — for telling the user something
    /// about this session that they could not otherwise observe (today: that a
    /// resume failed and this agent is starting empty).
    fn spawn_with_notice(
        cmd: Command,
        posture: PiPosture,
        notice: Option<String>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (rpc, inbound, child) = PiRpcClient::spawn_command(cmd)?;

        // The handshake is itself a request. If pi dies on spawn (bad auth, bad
        // flag), the EOF drain fails this immediately with pi's stderr attached
        // rather than leaving a new chat spinning until the timeout.
        let id = rpc.next_id("s");
        let resp = rpc
            .request(PiCommand::GetState { id }, HANDSHAKE_TIMEOUT)
            .context("pi handshake (get_state)")?;
        let state: SessionState =
            serde_json::from_value(resp.into_data()?).context("decode pi get_state")?;

        // pi's catalog is local (no network) and already filtered to providers
        // the user has auth for, so it is read once at connect rather than
        // joining the disk-backed catalog cache the network-probing backends
        // need. A failure here is not fatal: the session still runs on the model
        // it already has, with a picker that offers only that one.
        let models = match rpc.request(
            PiCommand::GetAvailableModels { id: rpc.next_id("m") },
            CONTROL_TIMEOUT,
        ) {
            Ok(resp) => match resp
                .into_data()
                .and_then(|d| serde_json::from_value::<AvailableModels>(d).map_err(Into::into))
            {
                Ok(cat) if !cat.models.is_empty() => cat.models.iter().map(model_choice).collect(),
                Ok(_) => state.model.iter().map(model_choice).collect(),
                Err(err) => {
                    tracing::warn!(?err, "pi get_available_models failed; picker limited to the current model");
                    state.model.iter().map(model_choice).collect()
                }
            },
            Err(err) => {
                tracing::warn!(?err, "pi get_available_models failed; picker limited to the current model");
                state.model.iter().map(model_choice).collect::<Vec<_>>()
            }
        };
        // The palette's commands, with pi's own descriptions and attribution.
        // Also non-fatal: a session with no palette is still a working session.
        let commands = match rpc
            .request(PiCommand::GetCommands { id: rpc.next_id("gc") }, CONTROL_TIMEOUT)
            .and_then(|r| r.into_data())
            .and_then(|d| serde_json::from_value::<AvailableCommands>(d).map_err(Into::into))
        {
            Ok(cat) => cat.commands.iter().map(command_info).collect::<Vec<_>>(),
            Err(err) => {
                tracing::warn!(?err, "pi get_commands failed; the slash palette stays empty");
                Vec::new()
            }
        };
        // Seeds the mapper so every usage event carries the context window — pi
        // reports it per model at the handshake, before any turn.
        let context_window = state.model.as_ref().and_then(|m| m.context_window);
        let (tx, rx) = std::sync::mpsc::channel();

        let _ = tx.send(ThreadEvent::SessionInit {
            session_id: state.session_id.clone(),
            // Qualified, so it matches a picker row's `wire` (and so a restore
            // that spawns on it resolves the model pi is actually running,
            // rather than whatever a bare id fuzzy-matches).
            model: state.model.as_ref().map(Model::qualified).unwrap_or_default(),
            // Pi has no per-call approval and no permission modes; its gating is
            // a session-level tool posture fixed at spawn. Reporting a mode here
            // would imply a control that does not exist.
            permission_mode: String::new(),
            slash_commands: commands.iter().map(|c| c.name.clone()).collect(),
            meta: SessionMeta::default(),
        });
        if let Some(n) = notice {
            let _ = tx.send(ThreadEvent::Error(n));
        }

        // Decode pi's stream into transcript events. The mapper owns the
        // snapshot→delta diffing, so everything downstream sees the same shape
        // Claude produces and the existing repaint throttle applies unchanged.
        let mut map_state = map::PiState::with_context_window(context_window);
        let shared_state = Arc::new(Mutex::new(Some(state)));

        #[cfg(windows)]
        let job = match oximux_job_object::JobObject::adopt(&child) {
            Ok(job) => Some(Arc::new(job)),
            Err(e) => {
                tracing::warn!(?e, "could not put pi in a job object");
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
            commands,
            context_window: map_state.context_window_handle(),
            posture,
        };

        std::thread::spawn(move || {
            for msg in inbound {
                match msg {
                    Inbound::Event(v) => {
                        // pi re-clamps the thinking level when the model changes,
                        // and announces the result here rather than in any
                        // response — so this is the only channel that can correct
                        // a level the session silently moved off.
                        if v.get("type").and_then(serde_json::Value::as_str)
                            == Some("thinking_level_changed")
                            && let Some(level) = v.get("level").and_then(serde_json::Value::as_str)
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
                        // Never observed firing (it needs an installed extension
                        // that raises UI), so nothing is built against its shape.
                        tracing::debug!(req = %v, "pi extension_ui_request (unhandled)");
                    }
                    Inbound::Response(r) => {
                        tracing::debug!(command = %r.command, "pi uncorrelated response");
                    }
                }
            }
            // Inbound closed → pi exited. Surface it; `tx` drops here, which
            // closes the app's receiver and fires its disconnect handling.
            let tail = rpc.stderr_tail();
            let msg = if tail.trim().is_empty() {
                "pi exited".to_string()
            } else {
                format!("pi exited. Stderr: {}", tail.trim())
            };
            let _ = tx.send(ThreadEvent::Error(msg));
        });

        Ok((conn, rx))
    }

    /// The session id pi reported at handshake.
    pub fn session_id(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().map(|s| s.session_id.clone())
    }

    /// The session file pi reported. **May not exist on disk**: a new session is
    /// held in memory until its first assistant message, so an absent file is
    /// normal. Never pre-create it — pi opens it with an exclusive-create flag
    /// and would throw.
    pub fn session_file(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().and_then(|s| s.session_file.clone())
    }

    /// Adopt a model pi has confirmed it switched to.
    ///
    /// The two writes belong together: the meter's denominator moves with the
    /// model (gpt-5.5 is 272K, gpt-5.3-codex-spark 128K), so updating the state
    /// without the atomic would leave the meter quietly measuring against the
    /// old model's window.
    fn adopt_model(&self, model: Model) {
        self.context_window.store(model.context_window.unwrap_or(0), Ordering::Relaxed);
        if let Ok(mut g) = self.state.lock()
            && let Some(st) = g.as_mut()
        {
            st.model = Some(model);
        }
    }

    /// SIGTERM pi, then hand the grace-wait + SIGKILL escalation to a detached
    /// thread.
    ///
    /// The signal itself is cheap and happens here, so pi starts reaping its
    /// detached tool children immediately. **The waiting must not happen on the
    /// caller's thread**: both call sites (`AgentConnection::shutdown` via a
    /// respawn, and `Drop`) run on the GPUI main thread, so a synchronous grace
    /// loop would freeze every window for as long as pi took to exit. That is a
    /// real path, not a hypothetical — pi survives `abort`, so a Stop-then-send
    /// respawn always signals a *live* process, unlike Claude (whose child is
    /// already dead by then) or Codex (which SIGKILLs outright).
    ///
    /// A SIGKILL escalation means pi never ran its handler, so its detached tool
    /// children are orphaned — a failure, not a routine path.
    fn terminate(&self) {
        {
            let mut child = match self.child.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            // Check before signalling: once reaped, the pid is free for the OS to
            // recycle, and a raw `kill(2)` would signal an unrelated process.
            // `Child::kill()` guards this; `libc::kill` does not. `shutdown()`
            // followed by `Drop` makes the second call the normal case.
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {}
            }
            #[cfg(unix)]
            {
                // SAFETY: `pid` is our own child, confirmed un-reaped just above,
                // so it still refers to that process. SIGTERM runs pi's handler,
                // which reaps the bash tool's detached children before exiting.
                let pid = child.id() as libc::pid_t;
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
        let child = self.child.clone();
        #[cfg(windows)]
        let job = self.job.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + TERM_GRACE;
            loop {
                let mut guard = match child.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                match guard.try_wait() {
                    Ok(Some(_)) | Err(_) => return, // exited via SIGTERM
                    Ok(None) => {}
                }
                if std::time::Instant::now() >= deadline {
                    // On unix this is the lossy path: pi never ran its handler,
                    // so its detached bash children are orphaned. On Windows the
                    // job object catches them instead — there was never a
                    // handler to run, but there is a tree to end.
                    #[cfg(unix)]
                    tracing::warn!(
                        "pi did not exit within {TERM_GRACE:?} of SIGTERM; escalating to \
                         SIGKILL — any running bash tool children will be orphaned"
                    );
                    #[cfg(windows)]
                    tracing::warn!(
                        "pi did not exit within {TERM_GRACE:?}; terminating its job object"
                    );
                    let _ = guard.kill();
                    let _ = guard.wait();
                    #[cfg(windows)]
                    if let Some(job) = &job {
                        let _ = job.kill();
                    }
                    return;
                }
                drop(guard); // never sleep holding the lock
                std::thread::sleep(Duration::from_millis(25));
            }
        });
    }
}

impl AgentConnection for PiRpcConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.rpc.send(PiCommand::Prompt {
            id: self.rpc.next_id("p"),
            message: text.to_string(),
            streaming_behavior: None,
        })
    }

    /// Redirect the live turn. pi delivers this at the next turn boundary — once
    /// the running tool finishes, before the next model call — as an ordinary
    /// user message.
    ///
    /// Fire-and-forget: pi's response only says the message was queued, which the
    /// caller learns anyway when pi drains it, and the caller is the UI thread.
    fn steer(&self, text: &str) -> Result<()> {
        self.rpc.send(PiCommand::Steer { id: self.rpc.next_id("st"), message: text.to_string() })
    }

    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        let _ = (request_id, decision);
        // Pi has no per-call approval anywhere in its protocol (verified against
        // the whole upstream source and live on the wire: bash executes with no
        // round-trip). Nothing can be resolved because nothing is ever asked.
        // Gating is the session-level tool posture fixed at spawn.
        anyhow::bail!("pi has no per-call tool approval; gating is a session-level tool posture")
    }

    fn shutdown(&self) {
        self.rpc.close_stdin();
        self.terminate();
    }

    fn cancel(&self) -> Result<()> {
        // In-band: pi has no SIGINT handler, so a signal would kill rather than
        // interrupt. Fire-and-forget — pi emits `response:abort` only AFTER the
        // turn settles, so awaiting it here would block the caller.
        self.rpc.send(PiCommand::Abort { id: self.rpc.next_id("a") })
    }

    fn cancel_and_wait(&self) -> Result<()> {
        // `response:abort` arrives after `agent_settled`, so awaiting the
        // response *is* waiting for the turn to finish unwinding.
        self.rpc
            .request(PiCommand::Abort { id: self.rpc.next_id("a") }, HANDSHAKE_TIMEOUT)
            .map(|_| ())
    }

    /// Pi's session-level posture, surfaced as composer controls.
    ///
    /// This is the round's load-bearing UI: pi cannot ask before running a tool,
    /// while its neighbours in the same cockpit can and do. A Pi chat must
    /// therefore *say* what it is allowed to do, because nothing will interrupt
    /// it later to ask. `set_feature` is deliberately not implemented — the app
    /// falls back to a respawn, which is the only way a spawn-time allowlist can
    /// change.
    fn features(&self) -> Vec<FeatureControl> {
        let opt = |wire: &str, label: &str, desc: &str| FeatureSelectOption {
            wire: wire.to_string(),
            label: label.to_string(),
            description: Some(desc.to_string()),
        };
        vec![
            FeatureControl {
                id: FEATURE_TOOLS.to_string(),
                label: "Tools".to_string(),
                description: Some(
                    "Pi has no per-call approval — this is chosen once, at launch".to_string(),
                ),
                icon: Some("shield".to_string()),
                kind: FeatureKind::Select {
                    options: vec![
                        opt(TOOLS_STANDARD, "Auto-run", "⚠ Runs bash & edits files without asking"),
                        opt(TOOLS_READ_ONLY, "Read-only", "Read, find, grep, ls — no bash or edits"),
                        opt(TOOLS_NONE, "No tools", "Chat only"),
                    ],
                    selected: Some(self.posture.tools.clone()),
                },
            },
            FeatureControl {
                id: FEATURE_CONTEXT_FILES.to_string(),
                label: "Context files".to_string(),
                description: Some(
                    "Load AGENTS.md / CLAUDE.md from the repo into context".to_string(),
                ),
                icon: Some("file".to_string()),
                kind: FeatureKind::Toggle { on: self.posture.context_files },
            },
        ]
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            // Pi has no permission modes at all — its gating is the spawn-time
            // posture, surfaced through `features()`.
            supports_modes: false,
            // `get_commands` at the handshake, and a `/cmd` forwarded as ordinary
            // text expands (verified live — see `slash_commands`).
            supports_slash: true,
            // The reasoning-effort picker, which is where pi's thinking level
            // renders. This is the gate for that control (`supports_effort` in
            // the composer), not a claim about arbitrary config — a `false` here
            // hides the picker no matter what `efforts()` returns.
            supports_config: true,
            // Every assistant message carries `usage`, including real dollars.
            emits_usage: true,
            // Rewind reads an on-disk session log. Pi keeps a new session in
            // memory until its first assistant message and rewrites the file
            // unlocked, so the file is not a safe fork source.
            supports_rewind: false,
            // `steer` — verified live: it lands at the next turn boundary and the
            // agent genuinely changes course.
            supports_steer: true,
        }
    }

    fn models(&self) -> Vec<ModelChoice> {
        self.models.clone()
    }

    /// The palette's rows, described by pi rather than reconstructed from disk.
    ///
    /// Safe to offer because a `/command` needs no special send path: pi expands
    /// one inside `prompt` itself (`agent-session.ts:809`), so OxiMux's existing
    /// "forward `/cmd` as ordinary text" contract already invokes it. Verified
    /// live — `/skill:gpui-action <args>` arrives at the model as the skill's body
    /// wrapped in a `<skill>` block with the arguments appended, and an
    /// *unrecognised* `/command` passes through untouched rather than erroring.
    fn slash_commands(&self) -> Vec<SlashCommandInfo> {
        self.commands.clone()
    }

    fn default_model(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().and_then(|s| s.model.as_ref().map(Model::qualified))
    }

    /// Known at connect and updated on every model switch — pi reports
    /// `contextWindow` per model, so the meter needs no turn to find a
    /// denominator.
    fn context_window(&self) -> Option<u64> {
        match self.context_window.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Switch model in-session — no respawn, so the conversation survives.
    ///
    /// `model` is a `provider/id` wire from [`Self::models`]; pi needs the two
    /// halves separately. Split on the FIRST slash, matching pi's own reference
    /// parsing (`model-resolver.ts:98`), so a provider whose model ids contain
    /// slashes still resolves.
    ///
    /// Side effect worth knowing: pi persists this as the user's **global**
    /// default model (`settingsManager.setDefaultModelAndProvider`), so it also
    /// changes what a bare `pi` picks up in the terminal. That is pi's own
    /// design — its interactive model-selector writes the same setting — and
    /// there is no in-session switch that avoids it; only `--model` at spawn is
    /// session-scoped, and reaching it would mean respawning.
    fn set_model(&self, model: &str) -> Result<()> {
        let (provider, model_id) = model.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("pi needs a provider-qualified model (`provider/id`), got {model:?}")
        })?;
        let resp = self.rpc.request(
            PiCommand::SetModel {
                id: self.rpc.next_id("sm"),
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            },
            CONTROL_TIMEOUT,
        )?;
        // pi echoes the model it actually switched to; adopting that rather than
        // the request keeps the meter and the thinking levels tied to what the
        // session is really running.
        let switched: Model =
            serde_json::from_value(resp.into_data()?).context("decode pi set_model")?;
        self.adopt_model(switched);
        Ok(())
    }

    /// Pi's thinking levels ride the existing reasoning-effort picker — the same
    /// control Codex's effort uses. Derived from the CURRENT model, never a fixed
    /// list: support is per-model, and pi answers an unsupported level with
    /// `success: true` after silently clamping it (see
    /// [`Model::supported_thinking_levels`]).
    fn efforts(&self) -> Vec<EffortChoice> {
        let Ok(g) = self.state.lock() else { return Vec::new() };
        let Some(model) = g.as_ref().and_then(|s| s.model.as_ref()) else { return Vec::new() };
        model
            .supported_thinking_levels()
            .into_iter()
            .map(|wire| EffortChoice { wire: wire.to_string(), label: thinking_label(wire) })
            .collect()
    }

    fn default_effort(&self) -> Option<String> {
        self.state.lock().ok()?.as_ref().and_then(|s| s.thinking_level.clone())
    }

    /// Set the thinking level in-session (returning `Ok` is what tells the app to
    /// skip a respawn).
    ///
    /// Fire-and-forget: pi acks locally and `efforts()` already excludes anything
    /// it would clamp, so there is nothing to wait for on the UI thread. If pi
    /// does clamp anyway — it re-clamps whenever the model changes — the
    /// `thinking_level_changed` event corrects this optimistic write.
    fn set_effort(&self, effort: &str) -> Result<()> {
        self.rpc.send(PiCommand::SetThinkingLevel {
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

/// pi's own name for a thinking level (`thinking-selector.ts`), so the picker
/// reads the way pi's docs and CLI do rather than title-casing `xhigh` into
/// something no one recognises.
fn thinking_label(wire: &str) -> String {
    match wire {
        "off" => "Off",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra-high",
        "max" => "Max",
        other => return other.to_string(),
    }
    .to_string()
}

impl Drop for PiRpcConnection {
    fn drop(&mut self) {
        // `pi --mode rpc` never exits on its own, so without this a connection
        // dropped without an explicit shutdown() leaks a Node process *and* any
        // detached tool children it spawned.
        self.rpc.close_stdin();
        self.terminate();
    }
}

/// The argv for a pi launch (everything after the program). Pure so the exact
/// flags a posture produces are testable without spawning — the posture IS the
/// tool gate, so "which flags actually reached the child" is a safety property,
/// not a detail. Resuming is a safety property for the same reason (below), so
/// it is built here too, and this returns `Result` rather than quietly dropping
/// a reference it cannot make safe.
///
/// ## Why `resume` must be an id, never a path
///
/// pi accepts `--session <path|id>` and resolves the two **completely
/// differently** (`resolveSessionPath`, `main.ts:163-188`):
///
/// - **id** → looked up in the current project's store. A miss is loud: pi
///   prints `No session found matching '<id>'` and exits 1. Our handshake then
///   fails with that text attached, and the user is told.
/// - **path** → anything containing a slash or ending `.jsonl` is taken as a
///   path **with no existence check**. A stale path therefore does not fail — pi
///   *creates* a new empty session there and starts normally (verified live:
///   `messageCount: 0`, a fresh session id). OxiMux would render the restored
///   transcript above an agent that remembers nothing, and neither the user nor
///   any assertion on the response could tell.
///
/// So a path-shaped reference is rejected here rather than passed on: the only
/// resume route whose failure is *observable* is the id. This is the same silent
/// class as `switch_session` on a missing path, and choosing the id closes it by
/// construction instead of guarding after the fact.
///
/// The caller must also spawn in the session's own cwd. pi scopes its store per
/// project; an id belonging to a different project takes a third branch that
/// asks `Fork this session into current directory? [y/N]` on **stdin** — which
/// in `--mode rpc` is the command pipe. Verified live: pi consumed the
/// handshake as the answer, printed `Aborted.` and exited **0**.
pub fn build_args(
    model: Option<&str>,
    posture: &PiPosture,
    resume: Option<&str>,
) -> Result<Vec<String>> {
    let mut args = vec!["--mode".to_string(), "rpc".to_string()];
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(id) = resume.map(str::trim).filter(|s| !s.is_empty()) {
        if is_path_like(id) {
            anyhow::bail!(
                "refusing to resume pi by path ({id:?}): pi does not check that a session path \
                 exists — it silently creates an empty session there and starts as if resumed. \
                 Resume by session id instead, so a stale reference fails loudly."
            );
        }
        args.push("--session".to_string());
        args.push(id.to_string());
    }
    args.extend(posture.to_args());
    Ok(args)
}

/// Exactly pi's own test for "this reference is a path, not an id"
/// (`main.ts:165`). Matched deliberately: the point is to predict which branch
/// pi will take, so it must agree with pi rather than be merely reasonable.
fn is_path_like(reference: &str) -> bool {
    reference.contains('/') || reference.contains('\\') || reference.ends_with(".jsonl")
}

/// Whether a spawn failure is pi reporting that the session id doesn't exist —
/// as opposed to a missing binary, bad auth, or a crash, none of which a fresh
/// session would fix.
///
/// Matches on pi's message because pi offers nothing better: it exits `1` for a
/// missing session, but also for other startup failures, so the exit code cannot
/// tell them apart. The string is pi's (`main.ts:292,316`); if a release changes
/// it, this stops matching and a failed resume goes back to surfacing as an
/// error — the safe direction.
fn session_not_found(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("No session found matching")
}

/// One pi model as a picker choice. Pi gives a real display name, a provider and
/// a context window, so the row reads `GPT-5.5` / `openai-codex · 272K context`
/// rather than a bare wire id.
///
/// `wire` is **provider-qualified** — see [`Model::qualified`] for why a bare id
/// silently loads a different model.
fn model_choice(m: &Model) -> ModelChoice {
    let mut description = m.provider.clone();
    if let Some(w) = m.context_window.filter(|w| *w > 0) {
        description.push_str(&format!(" · {} context", fmt_window(w)));
    }
    ModelChoice { wire: m.qualified(), label: m.display_name().to_string(), description: Some(description) }
}

/// One pi command as a palette row.
///
/// The name rides through verbatim: pi's own `skill:` prefix is part of what the
/// user must type for the expansion to fire, so stripping it for looks would
/// produce a row that doesn't work.
///
/// `scope` becomes the attribution tag — for pi's skills it is the useful
/// distinction (a repo-local `.agents/skills` entry vs a user-global one), which
/// `source` is not: it would read `skill` next to a row already named `skill:…`.
fn command_info(c: &protocol::SlashCommand) -> SlashCommandInfo {
    SlashCommandInfo {
        name: c.name.clone(),
        description: c.description.clone(),
        is_skill: c.is_skill(),
        source_label: c.source_info.as_ref().and_then(|s| s.scope.clone()),
    }
}

/// A context window as a picker blurb: `272000` → `272K`, `1050000` → `1.05M`.
fn fmt_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let m = tokens as f64 / 1_000_000.0;
        // Two models can differ by a fraction of a million (1.0M vs 1.05M), so
        // don't round that distinction away.
        format!("{m:.2}M").replace(".00M", "M")
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// Resolve the `pi` binary.
///
/// A macOS app launched from Finder does not inherit the shell's PATH, and pi is
/// typically installed under a version manager (nvm/volta/bun) whose bin dir is
/// only on an interactive shell's PATH. So a bare `Command::new("pi")` fails for
/// exactly the launch mode most users use.
///
/// Order: an explicitly configured path wins; then PATH; then a login-shell
/// probe, which is what actually recovers the version-manager case.
fn resolve_pi_binary(configured: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(p);
        if path.is_absolute() && !path.exists() {
            anyhow::bail!("configured pi binary not found: {p}");
        }
        return Ok(path);
    }
    if let Some(p) = which_on_path("pi") {
        return Ok(p);
    }
    // Cached: this spawns a login shell, which is far too expensive to repeat on
    // every connect/respawn, and connect runs on the UI thread. The answer only
    // changes if the user reinstalls pi, which warrants a restart anyway.
    static LOGIN_SHELL_PI: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    if let Some(p) = LOGIN_SHELL_PI.get_or_init(|| login_shell_which("pi")).clone() {
        return Ok(p);
    }
    anyhow::bail!(
        "could not find the `pi` binary. Install it, or set an absolute path in \
         Settings → Agents (a GUI launch does not inherit your shell's PATH, so a \
         version-manager install such as nvm is not visible by default)."
    )
}

/// First match for `bin` on the inherited PATH.
///
/// Synchronous on purpose — this runs inside `connect`, which is called from
/// the UI thread and has no runtime to await on. The `which` crate does not
/// spawn, so the cost is a handful of `stat`s either way; what it adds over the
/// hand-rolled walk this replaced is `PATHEXT` handling on Windows, where `pi`
/// installs as `pi.cmd`.
fn which_on_path(bin: &str) -> Option<PathBuf> {
    which::which(bin).ok()
}

/// The shell and argv that ask "where is `bin`?" with the user's own startup
/// files loaded. That is the whole point of the probe: the answer differs from
/// a plain PATH lookup exactly when a startup file edits PATH.
///
/// The two platforms get there differently. A POSIX login shell (`-l`) sources
/// the profile chain a version manager hooks into; PowerShell has no comparable
/// login mode on Windows (`-Login` is honored only on unix builds), but it
/// loads `$PROFILE` for `-Command` unless told not to — which is the same
/// recovery, since a Windows PATH edit that a GUI launch misses lives there.
#[cfg(unix)]
fn login_probe_command(bin: &str) -> (String, Vec<String>) {
    (
        oximux_shell_env::default_shell(),
        vec!["-lc".to_string(), format!("command -v {bin}")],
    )
}

#[cfg(windows)]
fn login_probe_command(bin: &str) -> (String, Vec<String>) {
    (
        oximux_shell_env::default_shell(),
        vec![
            "-NoLogo".to_string(),
            "-Command".to_string(),
            // `-ErrorAction SilentlyContinue` so a missing command prints
            // nothing instead of an error record the caller would parse as a
            // path. Empty stdout is already the "not found" signal.
            format!("(Get-Command {bin} -ErrorAction SilentlyContinue).Source"),
        ],
    )
}

/// Ask a login shell where `bin` is — recovers nvm/volta/bun installs that a
/// Finder-launched app can't see.
///
/// Bounded: a login shell runs the user's full rc chain, which can block
/// indefinitely (an nvm auto-use hook, a network-mounted home, a prompt). This
/// runs on the UI thread, so it must never wait forever — a slow shell degrades
/// to "pi not found" (an actionable error) rather than a frozen app.
fn login_shell_which(bin: &str) -> Option<PathBuf> {
    use oximux_no_window::NoWindow as _;
    let (shell, args) = login_probe_command(bin);
    let mut child = Command::new(shell)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .no_window()
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + LOGIN_SHELL_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            // Timed out or errored: kill the shell and give up rather than hang.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    "login-shell probe for `{bin}` exceeded {LOGIN_SHELL_PROBE_TIMEOUT:?}; \
                     treating it as not found"
                );
                return None;
            }
        }
    }

    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    // The LAST non-empty line: an rc file that prints banners ("Now using node
    // v20.11.0") puts them on stdout ahead of `command -v`'s answer, and
    // treating the whole blob as one path fails for exactly the version-manager
    // users this fallback exists to serve.
    let last = out.lines().rev().find(|l| !l.trim().is_empty())?.trim();
    let p = PathBuf::from(last);
    is_executable(&p).then_some(p)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write an executable fake `pi` into `dir` and return the path to spawn.
    ///
    /// These fakes have to be real programs — `PiRpcConnection::spawn` execs the
    /// configured path and talks JSON-RPC over its pipes — which makes them the
    /// one thing in this suite that cannot be written once for both platforms:
    ///
    /// - unix gets the `sh` script plus a `chmod`.
    /// - Windows cannot exec a shebang at all (`%1 is not a valid Win32
    ///   application`, os error 193) and `CreateProcess` infers no extension
    ///   from a bare `pi`. So the fake is a `.cmd`, which `CreateProcess` does
    ///   run through the command processor, shimmed onto a `.ps1` holding the
    ///   real logic — `cmd`'s own language is not up to a JSON-RPC loop.
    ///
    /// Both scripts are passed in rather than translated, because they are not
    /// mechanically translatable and pretending otherwise hides which behaviour
    /// each platform is actually asserting.
    fn write_fake_pi(dir: &Path, sh: &str, powershell: &str) -> PathBuf {
        #[cfg(unix)]
        {
            let _ = powershell;
            let path = dir.join("pi");
            std::fs::write(&path, sh).expect("write fake");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            path
        }
        #[cfg(windows)]
        {
            let _ = sh;
            std::fs::write(dir.join("pi.ps1"), powershell).expect("write fake");
            let path = dir.join("pi.cmd");
            // `-File` (not `-Command`) so the script is not re-parsed by cmd,
            // and `%*` forwards argv so a fake can branch on `--session`.
            std::fs::write(
                &path,
                "@echo off\r\npowershell -NoProfile -ExecutionPolicy Bypass \
                 -File \"%~dp0pi.ps1\" %*\r\n",
            )
            .expect("write shim");
            path
        }
    }

    /// Live handshake against the real `pi`. Ignored by default (needs pi
    /// installed); costs no model tokens — `get_state` never calls a provider.
    ///
    /// Pinned to a scratch `--session-dir`: without one, pi roots sessions under
    /// the user's real `~/.pi` store, and a test must not write there.
    ///
    /// Run: `cargo test -p oximux-agents pi:: -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `pi` on this machine"]
    fn live_pi_handshake() {
        let dir = std::env::temp_dir().join(format!("oximux-pi-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let program = resolve_pi_binary(None).expect("find pi");
        let mut cmd = Command::new(program);
        cmd.arg("--mode")
            .arg("rpc")
            .arg("--session-dir")
            .arg(dir.join("sessions"))
            .current_dir(&dir);
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("spawn real pi");
        let sid = conn.session_id().expect("pi reports a session id");
        assert!(!sid.is_empty());
        // Pi reports contextWindow per model, so the meter seeds before any turn.
        let cw = conn.context_window().expect("pi reports a context window at connect");
        assert!(cw > 0, "context window must be positive, got {cw}");
        let models = conn.models();
        assert!(!models.is_empty(), "pi advertises its model at handshake");
        eprintln!(
            "live pi: session={sid} context_window={cw} model={:?} file={:?}",
            conn.default_model(),
            conn.session_file()
        );
        // pi reports a session file eagerly, but a session with no assistant
        // message yet is held in memory — the path legitimately does not exist.
        if let Some(f) = conn.session_file() {
            assert!(
                !std::path::Path::new(&f).exists(),
                "a session with no assistant message is memory-only; pi should not \
                 have written {f} yet"
            );
        }
        conn.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end against the real `pi`: spawn → send → map → render. Ignored by
    /// default (needs pi + a signed-in provider) and costs one cheap turn.
    ///
    /// Run: `cargo test -p oximux-agents pi::tests::live_pi_turn_renders -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `pi` and spends provider tokens"]
    fn live_pi_turn_renders() {
        use crate::thread::state::ChatThread;

        let dir = std::env::temp_dir().join(format!("oximux-pi-turn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let program = resolve_pi_binary(None).expect("find pi");
        let mut cmd = Command::new(program);
        cmd.arg("--mode")
            .arg("rpc")
            .arg("--session-dir")
            .arg(dir.join("sessions"))
            // Keep the turn cheap and side-effect-free.
            .arg("--no-tools")
            .current_dir(&dir);
        let (conn, rx) = PiRpcConnection::spawn_command(cmd).expect("spawn real pi");

        conn.send_user_message("Say exactly: HELLO").expect("send");

        let mut thread = ChatThread::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "timed out before the turn settled");
            match rx.recv_timeout(left) {
                Ok(e) => {
                    let done = matches!(e, ThreadEvent::TurnEnded { .. });
                    thread.apply(&e);
                    if done {
                        break;
                    }
                }
                Err(e) => panic!("pi stream ended before the turn settled: {e}"),
            }
        }

        let texts: Vec<_> = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                crate::thread::entry::ThreadEntry::Assistant(m) if !m.text.is_empty() => {
                    Some(m.text.clone())
                }
                _ => None,
            })
            .collect();
        eprintln!("live pi transcript: {texts:?}");
        assert!(
            texts.iter().any(|t| t.contains("HELLO")),
            "the model's reply must reach the transcript, got {texts:?}"
        );
        // Streamed once, not duplicated by the snapshot→delta diffing.
        let hello = texts.iter().find(|t| t.contains("HELLO")).unwrap();
        assert_eq!(hello.matches("HELLO").count(), 1, "duplicated text: {hello:?}");
        conn.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn args(model: Option<&str>, posture: &PiPosture) -> Vec<String> {
        self::build_args(model, posture, None).expect("no resume → never fails")
    }

    #[test]
    fn build_args_puts_the_posture_flags_on_the_command_line() {
        // The allowlist IS the gate — pi never asks — so what lands in argv is a
        // safety property, not cosmetics.
        assert_eq!(
            args(None, &PiPosture::default()),
            vec!["--mode", "rpc"],
            "the default posture adds no flags: pi's own read/bash/edit/write applies"
        );
        assert_eq!(
            args(
                Some("openai-codex/gpt-5.5"),
                &PiPosture { tools: posture::TOOLS_READ_ONLY.into(), context_files: false }
            ),
            vec!["--mode", "rpc", "--model", "openai-codex/gpt-5.5", "--tools", "read,find,grep,ls", "--no-context-files"]
        );
        assert_eq!(
            args(None, &PiPosture { tools: posture::TOOLS_NONE.into(), context_files: true }),
            vec!["--mode", "rpc", "--no-tools"]
        );
        // An empty model selector must not emit a bare `--model`.
        assert_eq!(args(Some(""), &PiPosture::default()), vec!["--mode", "rpc"]);
    }

    #[test]
    fn resuming_passes_the_session_id_and_keeps_the_posture() {
        // A restored chat must come back under its persisted posture AND its
        // session — losing either silently changes what the agent may do or what
        // it remembers.
        let posture = PiPosture { tools: posture::TOOLS_READ_ONLY.into(), context_files: true };
        assert_eq!(
            self::build_args(None, &posture, Some("019f667f-264d-7c01-8f62-e8a675ae5b35")).unwrap(),
            vec![
                "--mode",
                "rpc",
                "--session",
                "019f667f-264d-7c01-8f62-e8a675ae5b35",
                "--tools",
                "read,find,grep,ls"
            ]
        );
        // A blank/whitespace id means "fresh session", not `--session ""`.
        assert_eq!(self::build_args(None, &PiPosture::default(), Some("  ")).unwrap(), vec!["--mode", "rpc"]);
    }

    #[test]
    fn resuming_by_path_is_refused_because_pi_would_mint_an_empty_session() {
        // THE hazard of this phase. pi takes anything path-shaped as a path and
        // never checks it exists (`main.ts:165`): a stale one makes pi create an
        // empty session there and start as if resumed (verified live —
        // messageCount 0, new session id, success reported). OxiMux would then
        // render the restored transcript over an agent with no memory of it.
        // An id, by contrast, fails loudly ("No session found matching ...").
        for path_like in [
            "/tmp/sessions/2026-07-15T15-54_019f667f.jsonl",
            "sessions/x.jsonl",
            "2026-07-15_abc.jsonl",
            r"C:\pi\sessions\x.jsonl",
        ] {
            let err = self::build_args(None, &PiPosture::default(), Some(path_like))
                .expect_err("a path-shaped resume must be refused, not passed to pi");
            assert!(err.to_string().contains("session id"), "got {err}");
        }
        // A bare uuid is not path-like and rides through.
        assert!(self::build_args(None, &PiPosture::default(), Some("019f667f-264d-7c01")).is_ok());
    }

    /// Read-only posture must actually stop pi writing — against the real binary,
    /// not a mock. This is the criterion the whole phase exists for.
    ///
    /// Run: `cargo test -p oximux-agents pi::tests::live_read_only -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `pi` and spends provider tokens"]
    fn live_read_only_posture_actually_prevents_a_write() {
        use crate::thread::state::ChatThread;

        let dir = std::env::temp_dir().join(format!("oximux-pi-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let target = dir.join("should-not-exist.txt");
        let _ = std::fs::remove_file(&target);

        let program = resolve_pi_binary(None).expect("find pi");
        let posture = PiPosture { tools: posture::TOOLS_READ_ONLY.into(), context_files: true };
        let mut cmd = Command::new(program);
        cmd.args(args(None, &posture))
            .arg("--session-dir")
            .arg(dir.join("sessions"))
            .current_dir(&dir);
        let (conn, rx) = PiRpcConnection::spawn_with_posture(cmd, posture).expect("spawn real pi");

        conn.send_user_message(&format!(
            "Create a file at {} containing the word LEAKED. If you cannot, say CANNOT.",
            target.display()
        ))
        .expect("send");

        let mut thread = ChatThread::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "timed out before the turn settled");
            match rx.recv_timeout(left) {
                Ok(e) => {
                    let done = matches!(e, ThreadEvent::TurnEnded { .. });
                    thread.apply(&e);
                    if done {
                        break;
                    }
                }
                Err(e) => panic!("pi stream ended early: {e}"),
            }
        }

        // The only assertion that matters: nothing was written.
        assert!(
            !target.exists(),
            "read-only posture MUST prevent a write, but pi created {}",
            target.display()
        );
        // And pi should not even have had a write/bash tool to try.
        let tools_used: Vec<_> = thread
            .entries
            .iter()
            .filter_map(|e| match e {
                crate::thread::entry::ThreadEntry::ToolCall(tc) => Some(tc.name.clone()),
                _ => None,
            })
            .collect();
        eprintln!("read-only turn used tools: {tools_used:?}");
        for banned in ["write", "edit", "bash"] {
            assert!(
                !tools_used.iter().any(|t| t == banned),
                "`{banned}` must not be exposed under read-only posture, saw {tools_used:?}"
            );
        }
        conn.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deletes its directory on drop, so a panicking test still removes the auth
    /// copy it made rather than leaving a token in `/tmp`.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An isolated pi home for a live test, with the user's credentials copied in
    /// (0600) because pi resolves both auth and settings from this one directory.
    ///
    /// Pinning it is not tidiness — it is required. `set_model` and
    /// `set_thinking_level` write pi's **global** defaults, so a live test run
    /// against the real home would silently change which model the user's own
    /// `pi` command picks up afterwards. `PI_CODING_AGENT_DIR` redirects that
    /// write. Returns `None` when pi isn't signed in here, so the test skips
    /// rather than failing for an unrelated reason.
    fn scratch_pi_home() -> Option<(Scratch, PathBuf)> {
        let auth = PathBuf::from(std::env::var("HOME").ok()?).join(".pi/agent/auth.json");
        if !auth.exists() {
            return None;
        }
        let root = std::env::temp_dir().join(format!("oximux-pi-home-{}", std::process::id()));
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).ok()?;
        let guard = Scratch(root);
        let copied = agent.join("auth.json");
        std::fs::copy(&auth, &copied).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o600)).ok()?;
        }
        Some((guard, agent))
    }

    /// The palette against the real `pi`: `get_commands` answers at the handshake
    /// (before any message), every row is typeable, and pi describes each one.
    /// Costs no provider tokens — `get_commands` runs no turn.
    ///
    /// The rows depend on what this machine has installed, so this asserts the
    /// SHAPE rather than a fixed list: a scratch cwd has no project skills, but
    /// user-global ones (`~/.agents/skills`) are still found.
    ///
    /// Run: `cargo test -p oximux-agents pi::tests::live_palette -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `pi` on this machine"]
    fn live_palette_is_described_by_pi_itself() {
        let Some((guard, agent_dir)) = scratch_pi_home() else {
            eprintln!("skipping: no ~/.pi/agent/auth.json — pi is not signed in here");
            return;
        };
        let program = resolve_pi_binary(None).expect("find pi");
        let mut cmd = Command::new(program);
        cmd.args(args(None, &PiPosture::default()))
            .arg("--session-dir")
            .arg(guard.0.join("sessions"))
            .env("PI_CODING_AGENT_DIR", &agent_dir)
            .current_dir(&guard.0);
        let (conn, rx) = PiRpcConnection::spawn_command(cmd).expect("spawn real pi");

        let cmds = conn.slash_commands();
        eprintln!("live pi palette: {} commands", cmds.len());
        for c in cmds.iter().take(5) {
            eprintln!("  /{} [{:?}] skill={} — {:?}", c.name, c.source_label, c.is_skill, c.description);
        }
        if cmds.is_empty() {
            eprintln!("skipping assertions: this machine has no pi skills/prompts installed");
            return;
        }
        // The names arrive with the session, not after the first message.
        let init = rx.recv_timeout(Duration::from_secs(10)).expect("SessionInit");
        let ThreadEvent::SessionInit { slash_commands, .. } = init else {
            panic!("expected SessionInit, got {init:?}")
        };
        assert_eq!(slash_commands.len(), cmds.len(), "the palette is seeded at connect");
        for c in &cmds {
            assert!(!c.name.is_empty());
            assert!(!c.name.starts_with('/'), "pi's names exclude the slash: {:?}", c.name);
            assert!(!c.name.contains(' '), "a name with a space could not be typed: {:?}", c.name);
        }
        // pi's skills are namespaced by pi, and scope is what tells two apart.
        for c in cmds.iter().filter(|c| c.is_skill) {
            assert!(c.name.starts_with("skill:"), "pi namespaces its skills: {:?}", c.name);
            assert!(
                matches!(c.source_label.as_deref(), Some("user") | Some("project")),
                "expected a scope tag, got {:?}",
                c.source_label
            );
        }
    }

    /// The model surface against the real `pi`: the catalog is real, a qualified
    /// wire loads exactly the model it names, and switching moves the meter.
    /// Costs no provider tokens — none of these commands run a turn.
    ///
    /// Run: `cargo test -p oximux-agents pi::tests::live_model -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `pi` on this machine"]
    fn live_model_catalog_qualifies_and_switching_moves_the_meter() {
        let Some((guard, agent_dir)) = scratch_pi_home() else {
            eprintln!("skipping: no ~/.pi/agent/auth.json — pi is not signed in here");
            return;
        };
        let program = resolve_pi_binary(None).expect("find pi");
        let mut cmd = Command::new(program);
        cmd.args(args(None, &PiPosture::default()))
            .arg("--session-dir")
            .arg(guard.0.join("sessions"))
            .env("PI_CODING_AGENT_DIR", &agent_dir)
            .current_dir(&guard.0);
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("spawn real pi");

        let models = conn.models();
        eprintln!("live pi catalog: {} models", models.len());
        for m in &models {
            eprintln!("  {} — {} ({:?})", m.wire, m.label, m.description);
        }
        assert!(!models.is_empty(), "pi advertises its authed models");
        // Every wire pi's own catalog produces must be qualified — a bare id is
        // a fuzzy search pattern, not a reference.
        assert!(
            models.iter().all(|m| m.wire.split_once('/').is_some_and(|(p, i)| !p.is_empty() && !i.is_empty())),
            "unqualified wire in {models:?}"
        );
        let default = conn.default_model().expect("a current model");
        assert!(models.iter().any(|m| m.wire == default), "default {default:?} missing from the catalog");

        // The thinking levels pi will actually honour for the current model.
        let efforts: Vec<_> = conn.efforts().into_iter().map(|e| e.wire).collect();
        eprintln!("live pi thinking levels for {default}: {efforts:?}");
        assert!(!efforts.is_empty(), "a real model offers at least one level");

        // Switch to a model with a DIFFERENT context window, if the machine has
        // one, and prove the meter's denominator follows.
        let before = conn.context_window().expect("pi reports a context window at connect");
        eprintln!("live pi: {default} → context_window {before}");
        let other = models.iter().find(|m| {
            m.wire != default && m.description.as_deref().is_some_and(|d| !d.contains(&fmt_window(before)))
        });
        let Some(other) = other else {
            eprintln!("skipping the switch: no authed model with a different context window");
            return;
        };
        conn.set_model(&other.wire).unwrap_or_else(|e| panic!("live switch to {}: {e:#}", other.wire));
        assert_eq!(conn.default_model().as_deref(), Some(other.wire.as_str()));
        let after = conn.context_window().expect("the new model reports a window");
        eprintln!("live pi: switched to {} → context_window {after}", other.wire);
        assert_ne!(after, before, "the meter's denominator must follow the model");
        conn.shutdown();
    }

    /// Cross-process resume against the real `pi`: run a turn, kill the process,
    /// then reconnect by session id and prove the new process **has the earlier
    /// context** — the thing a restored chat silently loses if resume is wrong.
    ///
    /// Also pins the failure this phase exists to prevent: a stale id must fail
    /// LOUDLY, never start an empty session wearing a restored transcript.
    ///
    /// Spends a few provider tokens (two cheap turns).
    /// Run: `cargo test -p oximux-agents pi::tests::live_resume -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a real `pi` and spends provider tokens"]
    fn live_resume_by_id_carries_the_conversation_across_a_restart() {
        use crate::thread::state::ChatThread;

        let Some((guard, agent_dir)) = scratch_pi_home() else {
            eprintln!("skipping: no ~/.pi/agent/auth.json — pi is not signed in here");
            return;
        };
        let program = resolve_pi_binary(None).expect("find pi");
        let project = guard.0.join("project");
        std::fs::create_dir_all(&project).expect("project dir");

        // pi in this scratch home stores sessions per project under the agent
        // dir, so the id resolves from `project` and nowhere else.
        let spawn_pi = |resume: Option<&str>| {
            let posture = PiPosture { tools: TOOLS_NONE.into(), context_files: false };
            let mut cmd = Command::new(&program);
            cmd.args(self::build_args(None, &posture, resume).expect("args"))
                .env("PI_CODING_AGENT_DIR", &agent_dir)
                .current_dir(&project);
            PiRpcConnection::spawn_with_posture(cmd, posture)
        };
        let run_turn = |conn: &PiRpcConnection, rx: &Receiver<ThreadEvent>, text: &str| -> String {
            conn.send_user_message(text).expect("send");
            let mut thread = ChatThread::default();
            let deadline = std::time::Instant::now() + Duration::from_secs(90);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "timed out before the turn settled");
                match rx.recv_timeout(left) {
                    Ok(e) => {
                        let done = matches!(e, ThreadEvent::TurnEnded { .. });
                        thread.apply(&e);
                        if done {
                            break;
                        }
                    }
                    Err(e) => panic!("pi stream ended before the turn settled: {e}"),
                }
            }
            thread
                .entries
                .iter()
                .filter_map(|e| match e {
                    crate::thread::entry::ThreadEntry::Assistant(m) if !m.text.is_empty() => {
                        Some(m.text.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        };

        // Turn 1: plant a fact only this session can know.
        let (conn, rx) = spawn_pi(None).expect("spawn real pi");
        let session_id = conn.session_id().expect("pi mints a session id");
        run_turn(&conn, &rx, "Remember this codeword: XYLOPHONE. Reply with just: OK");
        conn.shutdown();
        drop(conn);
        // The process must be gone before we resume, so the second process reads
        // the session rather than sharing it.
        std::thread::sleep(Duration::from_millis(500));

        // Turn 2: a NEW process resumes by id and must recall the codeword.
        let (conn2, rx2) = spawn_pi(Some(&session_id)).expect("resume real pi by id");
        assert_eq!(
            conn2.session_id().as_deref(),
            Some(session_id.as_str()),
            "resuming must continue the SAME session, not mint a new one"
        );
        let reply = run_turn(&conn2, &rx2, "What codeword did I ask you to remember? Reply with just the word.");
        eprintln!("live pi resumed reply: {reply:?}");
        assert!(
            reply.to_uppercase().contains("XYLOPHONE"),
            "the resumed session must carry the earlier turn's context, got {reply:?}"
        );
        conn2.shutdown();

        // A stale id fails LOUDLY — the property the whole resume-by-id design
        // rests on (a path would instead mint an empty session and say nothing).
        let err = spawn_pi(Some("019f0000-0000-7000-0000-000000000000"))
            .err()
            .expect("a stale session id must not silently start a fresh agent");
        let chain = format!("{err:#}");
        eprintln!("live pi stale-id error: {chain}");
        assert!(chain.contains("No session found"), "pi's own reason must reach the user: {chain}");

        // And the fallback's trigger really fires on the REAL error. This is the
        // brittle joint: `session_not_found` matches pi's message text because pi
        // exits 1 for every startup failure alike, so the exit code can't tell a
        // missing session from bad auth. If a pi release rewords it, THIS fails —
        // which is the point. (The fallback behavior itself is exercised at the
        // `spawn()` seam by `an_unresumable_session_starts_fresh_but_says_so`;
        // this harness can't use `spawn()` because it must pin the agent dir.)
        assert!(
            session_not_found(&err),
            "the fallback keys on this message; pi now says something else: {chain}"
        );
    }

    #[test]
    fn an_unresumable_session_starts_fresh_but_says_so() {
        // pi holds a session in memory until its first assistant reply, so "send
        // a prompt, quit before the reply, reopen" leaves an id resolving to
        // nothing. Stranding that chat forever would be worse than starting
        // fresh — but starting fresh in SILENCE would be worse still: the agent
        // would answer as a stranger under a transcript it appears to have read.
        let dir = std::env::temp_dir().join(format!("pi-noresume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        // A fake pi that behaves as pi does for a missing id, then (unflagged)
        // serves a normal session.
        let script = r#"
case "$*" in
  *--session*) echo "No session found matching 'gone'" >&2; exit 1 ;;
esac
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"get_state"'*) printf '{"id":"%s","type":"response","command":"get_state","success":true,"data":{"sessionId":"brand-new"}}\n' "$id" ;;
  esac
done
"#;
        // The same fake in PowerShell. Reads through `[Console]::In` and flushes
        // explicitly: the handshake blocks on the `get_state` reply, and
        // PowerShell's own output buffering would stall it.
        let ps = r#"
if ($args -join ' ' -match '--session') {
  [Console]::Error.WriteLine("No session found matching 'gone'")
  exit 1
}
while ($null -ne ($line = [Console]::In.ReadLine())) {
  if ($line -match '"id":"([^"]*)"') { $id = $Matches[1] }
  if ($line -match '"type":"get_state"') {
    [Console]::Out.Write('{"id":"' + $id + '","type":"response","command":"get_state","success":true,"data":{"sessionId":"brand-new"}}' + "`n")
    [Console]::Out.Flush()
  }
}
"#;
        let fake = write_fake_pi(&dir, script, ps);
        let (conn, rx) = PiRpcConnection::spawn(
            &dir,
            None,
            fake.to_str(),
            PiPosture::default(),
            Some("gone"),
        )
        .expect("an unresumable id must fall back, not fail");
        assert_eq!(conn.session_id().as_deref(), Some("brand-new"), "a genuinely new session");
        let notice = std::iter::from_fn(|| rx.recv_timeout(Duration::from_secs(3)).ok())
            .take(4)
            .find_map(|e| match e {
                ThreadEvent::Error(m) => Some(m),
                _ => None,
            })
            .expect("the fallback must be announced, not silent");
        assert!(notice.contains("gone"), "name the session that couldn't be resumed: {notice}");
        assert!(notice.contains("cannot see the conversation above"), "got {notice}");
        conn.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_spawn_failure_that_is_not_a_missing_session_is_not_papered_over() {
        // The fallback exists for ONE cause. A missing binary, bad auth or a
        // crash must still surface — retrying those as a fresh session would turn
        // a fixable error into a mystery.
        let dir = std::env::temp_dir().join(format!("pi-badauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let fake = write_fake_pi(
            &dir,
            "#!/bin/sh\necho 'pi: no credentials found' >&2\nexit 1\n",
            "[Console]::Error.WriteLine('pi: no credentials found')\nexit 1\n",
        );
        let err = PiRpcConnection::spawn(&dir, None, fake.to_str(), PiPosture::default(), Some("sess-1"))
            .err()
            .expect("a credentials failure must not be retried as a fresh session");
        assert!(format!("{err:#}").contains("no credentials found"), "got {err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_configured_absolute_path_that_is_missing_fails_readably() {
        // Absolute for the running platform. `resolve_pi_binary` gates the
        // readable bail on `is_absolute()`, and `/nope/...` is not absolute on
        // Windows — it fell through as a bare name for PATH lookup, so nothing
        // failed and the assertion read as a missing guard.
        let missing = if cfg!(windows) {
            r"C:\nope\not\here\pi.exe"
        } else {
            "/nope/not/here/pi"
        };
        let err = resolve_pi_binary(Some(missing))
            .expect_err("a configured path that doesn't exist must fail");
        assert!(err.to_string().contains("not found"), "got {err}");
    }

    #[test]
    fn a_blank_configured_path_falls_through_to_lookup() {
        // Blank/whitespace means "unset", not "spawn the empty string".
        // `pi` is installed on this machine, so lookup resolves.
        let r = resolve_pi_binary(Some("   "));
        if let Ok(p) = r {
            assert!(p.ends_with("pi"), "resolved to {p:?}");
        }
    }

    #[test]
    fn resolution_prefers_an_explicit_relative_command() {
        // A non-absolute configured value is passed through for the OS to
        // resolve, rather than being rejected for not existing on disk.
        let p = resolve_pi_binary(Some("pi")).expect("relative command passes through");
        assert_eq!(p, PathBuf::from("pi"));
    }

    /// A fake `pi --mode rpc`. Correlates every response to the id it was sent
    /// (rather than hard-coding one), answers the four commands a chat drives,
    /// and stays up until stdin closes — as real pi does.
    ///
    /// The model shapes are trimmed from real `pi 0.80.6` bytes: gpt-5.5 opts
    /// into `xhigh` but not `max`, luna opts into both, and their context windows
    /// genuinely differ (272K / 128K / 372K) — which is what makes a stale meter
    /// denominator observable.
    fn fake_pi() -> Command {
        let script = r#"
GPT55='{"id":"gpt-5.5","name":"GPT-5.5","provider":"openai-codex","reasoning":true,"contextWindow":272000,"maxTokens":128000,"thinkingLevelMap":{"xhigh":"xhigh","minimal":"low"},"input":["text","image"]}'
SPARK='{"id":"gpt-5.3-codex-spark","name":"GPT-5.3 Codex Spark","provider":"openai-codex","reasoning":true,"contextWindow":128000,"thinkingLevelMap":{"xhigh":"xhigh","minimal":"low"},"input":["text"]}'
LUNA='{"id":"gpt-5.6-luna","name":"GPT-5.6 Luna","provider":"openai-codex","reasoning":true,"contextWindow":372000,"thinkingLevelMap":{"xhigh":"xhigh","max":"max","minimal":"low"},"input":["text","image"]}'
CMDS='{"name":"skill:gpui-action","description":"Action definitions and keyboard shortcuts in GPUI.","source":"skill","sourceInfo":{"path":"/repo/.agents/skills/gpui-action/SKILL.md","scope":"project"}},{"name":"review","description":"Review a diff","source":"prompt","sourceInfo":{"path":"/home/u/.pi/prompts/review.md","scope":"user"}}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"get_state"'*)
      printf '{"id":"%s","type":"response","command":"get_state","success":true,"data":{"sessionId":"sess-1","sessionFile":"/tmp/s.jsonl","thinkingLevel":"medium","model":%s}}\n' "$id" "$GPT55" ;;
    *'"type":"get_available_models"'*)
      printf '{"id":"%s","type":"response","command":"get_available_models","success":true,"data":{"models":[%s,%s,%s]}}\n' "$id" "$GPT55" "$SPARK" "$LUNA" ;;
    *'"type":"set_model"'*)
      mid=$(printf '%s' "$line" | sed -n 's/.*"modelId":"\([^"]*\)".*/\1/p')
      prov=$(printf '%s' "$line" | sed -n 's/.*"provider":"\([^"]*\)".*/\1/p')
      if [ "$prov" != "openai-codex" ]; then
        printf '{"id":"%s","type":"response","command":"set_model","success":false,"error":"Model not found: %s/%s"}\n' "$id" "$prov" "$mid"
        continue
      fi
      case "$mid" in
        gpt-5.3-codex-spark) M="$SPARK" ;;
        gpt-5.6-luna) M="$LUNA" ;;
        gpt-5.5) M="$GPT55" ;;
        *) printf '{"id":"%s","type":"response","command":"set_model","success":false,"error":"Model not found: %s/%s"}\n' "$id" "$prov" "$mid"; continue ;;
      esac
      printf '{"id":"%s","type":"response","command":"set_model","success":true,"data":%s}\n' "$id" "$M"
      # pi re-clamps thinking to the new model's capabilities and announces the
      # result only here. gpt-5.5 has no `max`, so a session sitting at `max`
      # lands on `xhigh`.
      if [ "$mid" = "gpt-5.5" ]; then
        printf '{"type":"thinking_level_changed","level":"xhigh"}\n'
      fi ;;
    *'"type":"set_thinking_level"'*)
      printf '{"id":"%s","type":"response","command":"set_thinking_level","success":true}\n' "$id" ;;
    *'"type":"get_commands"'*)
      printf '{"id":"%s","type":"response","command":"get_commands","success":true,"data":{"commands":[%s]}}\n' "$id" "$CMDS" ;;
  esac
done
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[test]
    fn handshake_populates_session_model_and_context_window() {
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        assert_eq!(conn.session_id().as_deref(), Some("sess-1"));
        assert_eq!(conn.session_file().as_deref(), Some("/tmp/s.jsonl"));
        assert_eq!(
            conn.context_window(),
            Some(272_000),
            "pi reports contextWindow at connect — the meter needs no turn"
        );
        assert_eq!(conn.default_model().as_deref(), Some("openai-codex/gpt-5.5"));
    }

    #[test]
    fn the_palette_is_described_by_pi_and_announced_at_the_handshake() {
        let (conn, rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        assert!(conn.capabilities().supports_slash);

        // The names ride to the palette on SessionInit — no waiting for a first
        // message, because `get_commands` is a request pi answers immediately.
        let init = rx.recv_timeout(Duration::from_secs(5)).expect("SessionInit");
        let ThreadEvent::SessionInit { slash_commands, .. } = init else {
            panic!("expected SessionInit, got {init:?}")
        };
        assert_eq!(slash_commands, vec!["skill:gpui-action".to_string(), "review".to_string()]);

        let cmds = conn.slash_commands();
        assert_eq!(cmds.len(), 2);
        // pi's own `skill:` prefix survives — it is part of what must be typed for
        // the expansion to fire, so a prettier row would be a broken one.
        assert_eq!(cmds[0].name, "skill:gpui-action");
        assert!(cmds[0].is_skill, "grouped under Skills");
        assert!(cmds[0].description.as_deref().unwrap().contains("keyboard shortcuts"));
        // Attribution is the scope, which is what actually distinguishes two
        // skills — `source` would just say "skill" beside a row named `skill:…`.
        assert_eq!(cmds[0].source_label.as_deref(), Some("project"));
        assert!(!cmds[1].is_skill, "a prompt template is a command, not a skill");
        assert_eq!(cmds[1].source_label.as_deref(), Some("user"));
    }

    #[test]
    fn a_pi_without_commands_still_connects() {
        // `get_commands` failing must cost the palette, not the session.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"type":"get_state"'*)
      printf '{"id":"%s","type":"response","command":"get_state","success":true,"data":{"sessionId":"sess-1"}}\n' "$id" ;;
    *'"type":"get_commands"'*)
      printf '{"id":"%s","type":"response","command":"get_commands","success":false,"error":"boom"}\n' "$id" ;;
  esac
done
"#,
        );
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("a command failure is not fatal");
        assert_eq!(conn.session_id().as_deref(), Some("sess-1"));
        assert!(conn.slash_commands().is_empty());
    }

    #[test]
    fn steering_reaches_the_live_turn_and_is_advertised() {
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        assert!(conn.capabilities().supports_steer, "pi has a real mid-turn queue");
        conn.steer("actually, stop").expect("steer is fire-and-forget");
    }

    #[test]
    fn the_picker_lists_pis_catalog_with_provider_qualified_wires() {
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        let models = conn.models();
        assert_eq!(models.len(), 3, "the picker offers pi's whole catalog, not just the current model");
        assert_eq!(models[0].label, "GPT-5.5");
        // A bare `gpt-5.5` would fuzzy-match a different provider's model
        // entirely (verified live: azure-openai-responses, 1.05M context).
        assert_eq!(models[0].wire, "openai-codex/gpt-5.5");
        assert!(models.iter().all(|m| m.wire.contains('/')), "every wire must be qualified: {models:?}");
        // The description carries what distinguishes the rows.
        assert_eq!(models[0].description.as_deref(), Some("openai-codex · 272K context"));
        assert_eq!(models[1].description.as_deref(), Some("openai-codex · 128K context"));
        // The current model is selectable by its own wire — a picker whose
        // `default_model` matched no row would highlight nothing.
        let default = conn.default_model().expect("a default");
        assert!(models.iter().any(|m| m.wire == default), "default {default:?} not in {models:?}");
    }

    #[test]
    fn switching_model_moves_the_context_meter_denominator() {
        // The meter's denominator is per-model and the models genuinely differ
        // (272K → 128K). Left at the connect-time value, the meter would quietly
        // measure every turn against the wrong window.
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        assert_eq!(conn.context_window(), Some(272_000));
        conn.set_model("openai-codex/gpt-5.3-codex-spark").expect("live switch");
        assert_eq!(conn.default_model().as_deref(), Some("openai-codex/gpt-5.3-codex-spark"));
        assert_eq!(
            conn.context_window(),
            Some(128_000),
            "the meter must follow the model, not the handshake"
        );
    }

    #[test]
    fn set_model_refuses_an_unqualified_wire_rather_than_guessing() {
        // Refusing here is the point: pi would ACCEPT a bare id and fuzzy-match
        // it across every provider it knows, silently loading a different model.
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        let err = conn.set_model("gpt-5.5").expect_err("a bare id is not a model reference");
        assert!(err.to_string().contains("provider-qualified"), "got {err}");
        // And the session stays on the model it was on.
        assert_eq!(conn.context_window(), Some(272_000));
    }

    #[test]
    fn a_model_pi_rejects_surfaces_its_error() {
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        let err = conn.set_model("nope/nope").expect_err("pi rejects an unknown model");
        assert!(err.to_string().contains("Model not found"), "pi's own message must reach us: {err}");
    }

    #[test]
    fn efforts_track_the_selected_models_thinking_support() {
        // Support is per-model, and pi answers an unsupported level with
        // success:true after silently clamping — so a fixed list of seven would
        // lie about what the model did.
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        let wires = |c: &PiRpcConnection| {
            c.efforts().into_iter().map(|e| e.wire).collect::<Vec<_>>()
        };
        assert_eq!(
            wires(&conn),
            vec!["off", "minimal", "low", "medium", "high", "xhigh"],
            "gpt-5.5's map has no `max`, so `max` must not be offered"
        );
        assert_eq!(conn.efforts()[5].label, "Extra-high", "pi's own name for xhigh");
        assert_eq!(conn.default_effort().as_deref(), Some("medium"));

        // Luna opts into `max`; switching to it must widen the offer.
        conn.set_model("openai-codex/gpt-5.6-luna").expect("switch");
        assert!(wires(&conn).contains(&"max".to_string()), "luna supports max: {:?}", wires(&conn));
    }

    #[test]
    fn a_thinking_level_pi_reclamps_is_corrected_from_the_event() {
        // pi re-clamps thinking when the model changes and announces it ONLY as
        // an event — no response carries it. Without this the composer would keep
        // showing a level the session had already left.
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        conn.set_model("openai-codex/gpt-5.6-luna").expect("switch to a max-capable model");
        conn.set_effort("max").expect("max is supported by luna");
        assert_eq!(conn.default_effort().as_deref(), Some("max"));

        // Back to a model without `max`: pi clamps to xhigh and says so.
        conn.set_model("openai-codex/gpt-5.5").expect("switch back");
        assert!(
            wait_until(Duration::from_secs(3), || conn.default_effort().as_deref() == Some("xhigh")),
            "the clamp must be adopted, got {:?}",
            conn.default_effort()
        );
        assert!(!conn.efforts().iter().any(|e| e.wire == "max"), "and `max` is no longer offered");
    }

    #[test]
    fn set_effort_switches_in_session_so_the_app_skips_a_respawn() {
        // Returning Ok is the signal that no respawn is needed; a respawn would
        // drop the live session for a setting pi changes in place.
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        assert!(conn.set_effort("high").is_ok());
        assert_eq!(conn.default_effort().as_deref(), Some("high"));
    }

    #[test]
    fn pi_advertises_usage_so_the_meter_and_cost_render() {
        // Every assistant message carries `usage`, including real dollars.
        let (conn, _rx) = PiRpcConnection::spawn_command(fake_pi()).expect("handshake");
        let caps = conn.capabilities();
        assert!(caps.emits_usage);
        // The composer gates the reasoning-effort picker on `supports_config`
        // (`supports_effort`), so a false here would hide pi's thinking level
        // however well `efforts()` is derived.
        assert!(caps.supports_config, "thinking level renders through the effort picker");
        // Pi has no permission modes, and its session file is not a safe fork
        // source (memory-held until the first assistant message, rewritten
        // unlocked) — claiming either would surface a control that cannot work.
        assert!(!caps.supports_modes);
        assert!(!caps.supports_rewind);
        assert!(conn.permission_modes().is_empty());
    }

    #[test]
    fn context_windows_read_as_pi_reports_them() {
        assert_eq!(fmt_window(272_000), "272K");
        assert_eq!(fmt_window(128_000), "128K");
        // Two real models differ by a fraction of a million; rounding to "1M"
        // would make them look identical in the picker.
        assert_eq!(fmt_window(1_050_000), "1.05M");
        assert_eq!(fmt_window(1_000_000), "1M");
        assert_eq!(fmt_window(900), "900");
    }

    #[test]
    fn a_pi_that_dies_during_the_handshake_fails_with_its_stderr() {
        // The failure users actually hit (bad auth / bad flag). It must not hang
        // the new chat, and the message must say what pi said.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("read line; echo 'pi: no credentials found' >&2; exit 1");
        let start = std::time::Instant::now();
        let err = PiRpcConnection::spawn_command(cmd).err().expect("must fail");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must fail via the EOF drain, not wait out the 30s handshake timeout"
        );
        let chain = format!("{err:#}");
        assert!(chain.contains("no credentials found"), "stderr must reach the user: {chain}");
    }

    #[test]
    fn resolve_permission_is_an_error_because_pi_never_asks() {
        let script = r#"
read line
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"s"}}\n'
sleep 2
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("handshake");
        let err = conn
            .resolve_permission(
                "req-1",
                PermissionDecision::Allow { updated_input: serde_json::Value::Null },
            )
            .expect_err("pi has no approval round-trip to resolve");
        assert!(err.to_string().contains("posture"), "got {err}");
    }

    // Gated for the same reason as its sibling below, which already was: the
    // test is a `trap ... TERM` shell script asserting a signal, and Windows has
    // neither half. `terminate()` cfg's the `libc::kill` out and reaps the tool
    // children through the job object instead — a different mechanism with the
    // same guarantee, covered by the job-object suite rather than here. The gate
    // was written for the pair and only landed on one of them.
    #[cfg(unix)]
    #[test]
    fn shutdown_signals_term_not_kill_so_pi_can_reap_its_tool_children() {
        // The load-bearing lifecycle fact: pi's SIGTERM/SIGHUP handler is the
        // ONLY path that runs killTrackedDetachedChildren(). SIGKILL runs no
        // handler, so every running bash tool tree (a dev server, a build) would
        // be orphaned forever. A fake that traps SIGTERM proves we send it.
        let dir = std::env::temp_dir().join(format!("pi-term-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let marker = dir.join("caught-sigterm");
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            r#"
trap 'touch "{m}"; exit 0' TERM
read line
printf '{{"id":"s1","type":"response","command":"get_state","success":true,"data":{{"sessionId":"s"}}}}\n'
while true; do sleep 0.05; done
"#,
            m = marker.display()
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("handshake");
        conn.shutdown();
        // The signal is synchronous; the child's *reaction* to it is not, so poll.
        assert!(
            wait_until(Duration::from_secs(3), || marker.exists()),
            "shutdown must SIGTERM (pi's handler reaps its detached tool children); \
             a SIGKILL would orphan them"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // SIGTERM semantics, driven by a `trap`-ing shell script. There is no
    // Windows counterpart to either half, so this is gated with the contract it
    // tests rather than left to pass vacuously.
    #[cfg(unix)]
    #[test]
    fn shutdown_does_not_block_the_caller_when_pi_ignores_sigterm() {
        // Both callers of shutdown() — a respawn and Drop — run on the GPUI main
        // thread. A wedged pi must not freeze every window: the grace wait and
        // the SIGKILL escalation belong on a detached thread, so shutdown()
        // returns as soon as the signal is sent.
        let script = r#"
trap '' TERM
read line
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"s"}}\n'
while true; do sleep 0.05; done
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("handshake");
        let start = std::time::Instant::now();
        conn.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "shutdown() blocked the caller for {:?}; it must not wait out TERM_GRACE ({TERM_GRACE:?}) \
             on the UI thread",
            start.elapsed()
        );
        // The escalation still happens, just off-thread: the child ignores
        // SIGTERM, so it must be SIGKILLed once the grace period lapses.
        let pid = conn.child.lock().unwrap().id();
        assert!(
            wait_until(TERM_GRACE + Duration::from_secs(3), || !pid_alive(pid)),
            "a pi that ignores SIGTERM must still be SIGKILLed by the escalation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_without_shutdown_still_reaps_the_child() {
        // `pi --mode rpc` never self-exits, so a forgotten shutdown() would leak
        // a Node process and its detached tool children.
        let script = r#"
read line
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"s"}}\n'
sleep 30
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (conn, _rx) = PiRpcConnection::spawn_command(cmd).expect("handshake");
        let pid = conn.child.lock().unwrap().id();
        drop(conn);
        // Drop signals synchronously but reaps off-thread (so it can't freeze the
        // UI), so the guarantee is "promptly", not "by the time drop returns".
        assert!(
            wait_until(Duration::from_secs(5), || !pid_alive(pid)),
            "pid {pid} must not survive the drop"
        );
    }

    /// Poll `cond` until it holds or `limit` elapses.
    fn wait_until(limit: Duration, cond: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    /// Whether `pid` still exists. Signal 0 only probes.
    ///
    /// Unix-only on purpose: its callers assert `!pid_alive(..)`, so a stub
    /// returning `false` off unix would make both of them pass without testing
    /// anything.
    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        // SAFETY: signal 0 performs no signal delivery, only a permission +
        // existence check on the pid.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}
