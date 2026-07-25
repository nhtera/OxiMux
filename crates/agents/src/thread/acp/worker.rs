//! The ACP session worker: runs the `agent-client-protocol` client against any
//! configured ACP CLI subprocess (Cursor, Amp, Gemini, …) under
//! `futures::executor::block_on` on a dedicated thread. It bridges our sync
//! `Outbound` queue to async prompts and streams `SessionUpdate`s back as
//! [`ThreadEvent`]s.
//!
//! The SDK is executor-agnostic (subprocess I/O via `async_process` + `blocking`,
//! no Tokio reactor), so a plain `block_on` on an owned thread mirrors the
//! Claude/Codex reader-thread ownership without pulling in a runtime.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, ClientCapabilities, ContentBlock, CreateTerminalRequest,
    CreateTerminalResponse,
    FileSystemCapabilities, ImageContent, InitializeRequest, KillTerminalRequest, KillTerminalResponse,
    LoadSessionRequest, NewSessionRequest, PromptRequest, PromptResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionConfigId, SessionConfigOption,
    SessionConfigOptionValue, SessionId, SessionModeId, SessionModeState, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason,
    TerminalOutputRequest, TerminalOutputResponse, TextContent, UsageUpdate,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo, Responder};
use futures::StreamExt;
use futures::channel::mpsc as fmpsc;
use futures::channel::oneshot;
use serde_json::Value;

use super::map::map_session_update;
use super::{AcpState, Outbound, approvals, auth, client_fs, client_terminal};
use crate::thread::entry::ChatImage;
use crate::thread::event::{AuthMethodInfo, AuthMethodKind, ThreadEvent, TurnUsage};
use crate::thread::tool_call::PermissionDecision;

/// Run the whole ACP session to completion (blocks the worker thread). A spawn
/// or protocol failure is surfaced as a [`ThreadEvent::Error`] so the app's
/// disconnect path fires (the same degradation as a Claude spawn failure).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    resume_session_id: Option<String>,
    env: Vec<(String, String)>,
    auth_method: Option<String>,
    event_tx: Sender<ThreadEvent>,
    outbound_rx: fmpsc::UnboundedReceiver<Outbound>,
    state: Arc<Mutex<AcpState>>,
) {
    let result = futures::executor::block_on(session(
        command,
        args,
        cwd,
        resume_session_id,
        env,
        auth_method,
        event_tx.clone(),
        outbound_rx,
        state,
    ));
    if let Err(e) = result {
        let _ = event_tx.send(ThreadEvent::Error(e));
    }
}

#[allow(clippy::too_many_arguments)]
async fn session(
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    resume_session_id: Option<String>,
    env: Vec<(String, String)>,
    auth_method: Option<String>,
    event_tx: Sender<ThreadEvent>,
    mut outbound_rx: fmpsc::UnboundedReceiver<Outbound>,
    state: Arc<Mutex<AcpState>>,
) -> Result<(), String> {
    // Keep `command` (the agent binary) — a terminal-kind auth method runs it with
    // the advertised login args in an embedded terminal.
    let agent = if env.is_empty() {
        // No env override → the original cmdline path (byte-identical): join the
        // command + args and let `AcpAgent::from_str` split them into program+argv
        // (verified in the crate's `yolo_one_shot_client` example).
        let cmdline = if args.is_empty() {
            command.clone()
        } else {
            format!("{} {}", command, args.join(" "))
        };
        AcpAgent::from_str(&cmdline).map_err(|e| format!("spawn `{cmdline}`: {e}"))?
    } else {
        // EnvVar auth: leading `NAME=value` tokens become the child's ENVIRONMENT
        // (via `AcpAgent::from_args`), never argv — so the typed secret can't leak
        // onto the process command line / `ps`. The first non-`NAME=value` token is
        // the command; the rest are its argv.
        let mut parts: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.push(command.clone());
        parts.extend(args.iter().cloned());
        AcpAgent::from_args(parts).map_err(|e| format!("spawn `{command}`: {e}"))?
    };

    let connect = Client
        .builder()
        // Stream agent updates → ThreadEvents. Cloned per call so the future owns
        // its handles (no borrow of the closure env across the mapping).
        .on_receive_notification(
            {
                let tx = event_tx.clone();
                let st = state.clone();
                move |n: SessionNotification, _cx: ConnectionTo<Agent>| {
                    let tx = tx.clone();
                    let st = st.clone();
                    async move {
                        // Usage arrives out-of-band, not per-turn: stash the latest
                        // (lossy map) so the prompt loop can fold it into the next
                        // `TurnEnded.usage`, keeping the footer turn-scoped. ALSO
                        // emit it live so the composer's context meter updates
                        // mid-turn — ACP carries the window size, so the % is real.
                        if let SessionUpdate::UsageUpdate(u) = &n.update {
                            let usage = usage_from_acp(u);
                            if let Ok(mut s) = st.lock() {
                                s.last_usage = Some(usage.clone());
                            }
                            let _ = tx.send(ThreadEvent::LiveUsage(usage));
                            return Ok(());
                        }
                        // A runtime config push carries the FULL option set (models /
                        // reasoning + current values). Swap it into state so
                        // `models()`/`current_model()` read the live vocabulary, then
                        // fall through: the mapper emits `ControlsUpdated` so the
                        // composer re-pulls its pickers.
                        if let SessionUpdate::ConfigOptionUpdate(u) = &n.update
                            && let Ok(mut s) = st.lock()
                        {
                            s.config_options = u.config_options.clone();
                        }
                        // During a `session/load` replay, drop transcript-bearing
                        // updates: OxiMux repaints from its own persisted blob, so
                        // replayed message/tool events would double-render. Control
                        // updates (mode/model/slash/title) still pass — they re-sync
                        // live pickers, not history.
                        let replaying = st.lock().map(|s| s.replaying).unwrap_or(false);
                        for ev in map_session_update(n.update) {
                            if replaying && is_transcript_event(&ev) {
                                continue;
                            }
                            let _ = tx.send(ev);
                        }
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // Route a permission request to the UI's approval card and answer with the
        // user's choice: emit the card, park on a per-request oneshot, then
        // translate the decision to the agent's option. A dropped sender (the
        // connection is closing) resolves to Cancelled so the turn never hangs.
        .on_receive_request(
            {
                let tx = event_tx.clone();
                let st = state.clone();
                move |req: RequestPermissionRequest,
                      responder: agent_client_protocol::Responder<RequestPermissionResponse>,
                      _cx: ConnectionTo<Agent>| {
                    let tx = tx.clone();
                    let st = st.clone();
                    async move {
                        let request_id = req.tool_call.tool_call_id.0.to_string();
                        let (otx, orx) = oneshot::channel::<PermissionDecision>();
                        {
                            let mut s = st.lock().unwrap_or_else(|p| p.into_inner());
                            s.pending.insert(request_id.clone(), otx);
                        }
                        let _ = tx.send(approvals::permission_event(&req, &request_id));
                        let outcome = match orx.await {
                            Ok(decision) => approvals::decision_to_outcome(&decision, &req.options),
                            Err(_) => RequestPermissionOutcome::Cancelled,
                        };
                        responder.respond(RequestPermissionResponse::new(outcome))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `fs/read_text_file` within the session cwd tree (out-of-tree paths
        // are denied + logged; see `client_fs`). The fs op is quick + synchronous.
        .on_receive_request(
            {
                let cwd = cwd.clone();
                move |req: ReadTextFileRequest,
                      responder: Responder<ReadTextFileResponse>,
                      _cx: ConnectionTo<Agent>| {
                    let cwd = cwd.clone();
                    async move {
                        match client_fs::read_text_file(&req, &cwd) {
                            Ok(resp) => responder.respond(resp),
                            Err(e) => responder.respond_with_error(e),
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `fs/write_text_file` within the session cwd tree (same guard).
        .on_receive_request(
            {
                let cwd = cwd.clone();
                move |req: WriteTextFileRequest,
                      responder: Responder<WriteTextFileResponse>,
                      _cx: ConnectionTo<Agent>| {
                    let cwd = cwd.clone();
                    async move {
                        match client_fs::write_text_file(&req, &cwd) {
                            Ok(resp) => responder.respond(resp),
                            Err(e) => responder.respond_with_error(e),
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `terminal/create`: spawn the agent's command on the app-provided
        // embedded-terminal host (reusing the app's PTY/relay stack). Each handler
        // reads the process-global host; when none is installed it rejects (the
        // handshake also advertised `terminal:false`, so an honest agent never
        // reaches here).
        .on_receive_request(
            |req: CreateTerminalRequest,
             responder: Responder<CreateTerminalResponse>,
             _cx: ConnectionTo<Agent>| async move {
                match super::terminal_host() {
                    Some(host) => match client_terminal::create(host.as_ref(), &req) {
                        Ok(resp) => responder.respond(resp),
                        Err(e) => responder.respond_with_error(e),
                    },
                    None => responder.respond_with_error(client_terminal::no_host_error()),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `terminal/output`: current captured output + exit status.
        .on_receive_request(
            |req: TerminalOutputRequest,
             responder: Responder<TerminalOutputResponse>,
             _cx: ConnectionTo<Agent>| async move {
                match super::terminal_host() {
                    Some(host) => match client_terminal::output(host.as_ref(), &req) {
                        Ok(resp) => responder.respond(resp),
                        Err(e) => responder.respond_with_error(e),
                    },
                    None => responder.respond_with_error(client_terminal::no_host_error()),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `terminal/wait_for_exit`: await the command's exit (async).
        .on_receive_request(
            |req: WaitForTerminalExitRequest,
             responder: Responder<WaitForTerminalExitResponse>,
             _cx: ConnectionTo<Agent>| async move {
                match super::terminal_host() {
                    Some(host) => match client_terminal::wait_for_exit(host.as_ref(), &req).await {
                        Ok(resp) => responder.respond(resp),
                        Err(e) => responder.respond_with_error(e),
                    },
                    None => responder.respond_with_error(client_terminal::no_host_error()),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `terminal/kill`: terminate but keep the terminal readable.
        .on_receive_request(
            |req: KillTerminalRequest,
             responder: Responder<KillTerminalResponse>,
             _cx: ConnectionTo<Agent>| async move {
                match super::terminal_host() {
                    Some(host) => match client_terminal::kill(host.as_ref(), &req) {
                        Ok(resp) => responder.respond(resp),
                        Err(e) => responder.respond_with_error(e),
                    },
                    None => responder.respond_with_error(client_terminal::no_host_error()),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Serve `terminal/release`: kill (if running) and free resources.
        .on_receive_request(
            |req: ReleaseTerminalRequest,
             responder: Responder<ReleaseTerminalResponse>,
             _cx: ConnectionTo<Agent>| async move {
                match super::terminal_host() {
                    Some(host) => match client_terminal::release(host.as_ref(), &req) {
                        Ok(resp) => responder.respond(resp),
                        Err(e) => responder.respond_with_error(e),
                    },
                    None => responder.respond_with_error(client_terminal::no_host_error()),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Advertise the client-side file capabilities we serve so an agent that
        // delegates file IO (per ACP client-capabilities) can call `fs/*` instead
        // of getting an automatic "method not found". `terminal` is advertised
        // only when the app installed an embedded-terminal host (else the agent
        // must run commands itself); the five handlers above serve it.
        .connect_with(agent, move |cx: ConnectionTo<Agent>| async move {
            let init_caps = ClientCapabilities::new()
                .fs(FileSystemCapabilities::new().read_text_file(true).write_text_file(true))
                .terminal(super::terminal_host().is_some());
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(init_caps))
                .block_task()
                .await?;
            // Whether this agent accepts image content blocks in a prompt. Off →
            // attached images are dropped (text-only) rather than erroring the turn.
            let supports_image = init.agent_capabilities.prompt_capabilities.image;

            // Stash the advertised auth methods so the auth loop can map a picked
            // method id to its kind (e.g. a terminal method's login args).
            let auth_methods: Vec<AuthMethodInfo> =
                init.auth_methods.iter().map(auth::auth_method_info).collect();
            if let Ok(mut s) = state.lock() {
                s.auth_methods = auth_methods.clone();
            }

            // Open the session, wrapped in an auth-retry loop: when the agent
            // reports `AuthRequired` (-32000) it needs login, so surface its
            // methods, park for the user's pick, authenticate, and retry — all on
            // the same connection (no respawn). Any other error is fatal.
            let load_id = resume_session_id.filter(|_| init.agent_capabilities.load_session);
            // A respawn that seeded EnvVar-auth credentials also carries the method
            // to authenticate; spend it on the FIRST `AuthRequired` so the env-seeded
            // agent signs in without re-showing the card. Consumed once.
            let mut auto_auth = auth_method;
            let (session_id, modes, config_options) = loop {
                match open_session(&cx, load_id.as_deref(), &cwd, &state, &event_tx).await {
                    Ok(opened) => break opened,
                    Err(e) if auth::is_auth_required(&e) => {
                        // The env is set now, so `authenticate` can succeed on this
                        // same process (unlike an in-place authenticate on the old
                        // env-less one). On success retry the open; on failure fall
                        // through to the interactive card so the user re-enters.
                        if let Some(method_id) = auto_auth.take()
                            && cx
                                .send_request(AuthenticateRequest::new(method_id))
                                .block_task()
                                .await
                                .is_ok()
                        {
                            continue;
                        }
                        match run_auth(&cx, &auth_methods, &mut outbound_rx, &event_tx, &command, &cwd)
                            .await
                        {
                            // Authenticated (or terminal login done) → retry the open.
                            AuthOutcome::Retry => continue,
                            // Shutdown arrived while the card was up → end quietly.
                            AuthOutcome::Aborted => return Ok(()),
                        }
                    }
                    // Any non-auth error is fatal; the outer `map_err` wraps it.
                    Err(e) => return Err(e),
                }
            };

            // Resolve capabilities from what the agent actually advertised at the
            // handshake (modes → picker, config_options → config control). Slash +
            // usage are wired via `session/update`s, so the cap is on and the
            // affordance stays empty/hidden until one arrives.
            let caps = super::caps_from_handshake(modes.as_ref(), &config_options);

            // Stash the live connection + session id (so `cancel()`/`set_mode()`
            // can fire out-of-band) AND the discovered modes/config/caps BEFORE
            // `SessionInit` is emitted — so the app reads real caps the moment it
            // processes init and lights up the pickers. A fallback-minted fresh id
            // rides here too, so persistence overwrites the stale one on SessionInit.
            let permission_mode =
                modes.as_ref().map(|m| m.current_mode_id.0.to_string()).unwrap_or_default();
            if let Ok(mut s) = state.lock() {
                s.session_id = Some(session_id.clone());
                s.connection = Some(cx.clone());
                s.modes = modes;
                s.config_options = config_options;
                s.caps = caps;
            }
            let _ = event_tx.send(ThreadEvent::SessionInit {
                session_id: session_id.0.to_string(),
                model: String::new(),
                permission_mode,
                slash_commands: Vec::new(),
                // ACP's init advertises no tool/MCP/agent inventory, so the
                // session-detail popover stays hidden for these sessions.
                meta: Default::default(),
            });

            // Prompt loop: one turn per queued Outbound::Prompt. `block_task()`
            // resolves when the turn ends (updates stream via the handler above,
            // concurrently). Draining the sender (all clones gone) also ends it.
            while let Some(msg) = outbound_rx.next().await {
                match msg {
                    Outbound::Prompt { text, images } => {
                        // Defensive: a live turn's updates must never be gated by a
                        // stale replay flag (belt-and-suspenders — the flag is
                        // already cleared when `session/load` resolved).
                        if let Ok(mut s) = state.lock() {
                            s.replaying = false;
                        }
                        let resp = cx
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                prompt_blocks(text, &images, supports_image),
                            ))
                            .block_task()
                            .await;
                        // The streamed chunks are authoritative and already built
                        // the assistant blocks; `TurnEnded` seals the window (no
                        // finalized text — that would clobber tool-interleaved text).
                        // The stop reason decides whether the turn is a success or
                        // wears the error banner (a Refusal/limit renders a reason).
                        // Fold the latest stashed usage (cloned, not taken — the
                        // context readout is cumulative, so it stays accurate on a
                        // later turn that reports no fresh usage).
                        let (result, is_error) = turn_outcome(resp);
                        let usage = state.lock().ok().and_then(|s| s.last_usage.clone());
                        let _ = event_tx.send(ThreadEvent::TurnEnded { result, usage, is_error, turn_diff: None });
                    }
                    Outbound::SetMode(mode) => {
                        // Fire-and-forget: a rejected mode change just leaves the
                        // picker on its prior value (the agent may echo the change
                        // back via `CurrentModeUpdate`, which re-syncs the picker).
                        let _ = cx
                            .send_request(SetSessionModeRequest::new(
                                session_id.clone(),
                                SessionModeId::new(mode),
                            ))
                            .block_task()
                            .await;
                    }
                    Outbound::SetConfig { id, value } => {
                        if let Some(val) = config_value_from_json(value) {
                            let resp = cx
                                .send_request(SetSessionConfigOptionRequest::new(
                                    session_id.clone(),
                                    SessionConfigId::new(id),
                                    val,
                                ))
                                .block_task()
                                .await;
                            // The response carries the FULL refreshed option set —
                            // switching the model changes which reasoning levels
                            // (and other options) apply, and the agent returns the
                            // new set here rather than always pushing a separate
                            // `ConfigOptionUpdate` notification. Swap it into state
                            // and signal the composer to re-pull its pickers so the
                            // effort/other controls track the new model. Guard the
                            // empty case so a terse agent reply can't wipe the vocab.
                            if let Ok(resp) = resp
                                && !resp.config_options.is_empty()
                            {
                                if let Ok(mut s) = state.lock() {
                                    s.config_options = resp.config_options;
                                }
                                let _ = event_tx.send(ThreadEvent::ControlsUpdated);
                            }
                        }
                    }
                    // The session is already open — a stray Authenticate (e.g. a
                    // double-click before the card cleared) is a no-op.
                    Outbound::Authenticate(_) => {}
                    Outbound::Shutdown => break,
                }
            }
            Ok::<(), agent_client_protocol::Error>(())
        });

    connect.await.map_err(|e| format!("acp connection: {e}"))
}

/// Open a fresh ACP session (`session/new`) and pull the id + advertised
/// modes/config out of the response — the shared handshake continuation both the
/// new-session path and the `session/load` fallback feed into.
async fn new_session(
    cx: &ConnectionTo<Agent>,
    cwd: PathBuf,
) -> Result<(SessionId, Option<SessionModeState>, Vec<SessionConfigOption>), agent_client_protocol::Error>
{
    let sess = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?;
    Ok((sess.session_id.clone(), sess.modes.clone(), sess.config_options.clone().unwrap_or_default()))
}

/// The opened-session tuple the handshake continuation consumes.
type Opened = (SessionId, Option<SessionModeState>, Vec<SessionConfigOption>);

/// Open the session for the handshake: `session/load` when handed a resumable id
/// (guarding the history replay), else `session/new`. A NON-auth load failure
/// (unknown session, method not found) falls back to a fresh session with a
/// visible notice; an auth failure (-32000) propagates so the caller runs the
/// auth flow and retries.
async fn open_session(
    cx: &ConnectionTo<Agent>,
    load_id: Option<&str>,
    cwd: &Path,
    state: &Mutex<AcpState>,
    event_tx: &Sender<ThreadEvent>,
) -> Result<Opened, agent_client_protocol::Error> {
    let Some(id) = load_id else {
        return new_session(cx, cwd.to_path_buf()).await;
    };
    let sid = SessionId::new(id);
    // Guard the replay: per spec the agent re-sends its history as
    // `session/update`s BEFORE this response resolves; the notification handler
    // drops those while `replaying` is set.
    if let Ok(mut s) = state.lock() {
        s.replaying = true;
    }
    let loaded =
        cx.send_request(LoadSessionRequest::new(sid.clone(), cwd.to_path_buf())).block_task().await;
    if let Ok(mut s) = state.lock() {
        s.replaying = false;
    }
    match loaded {
        Ok(resp) => Ok((sid, resp.modes, resp.config_options.unwrap_or_default())),
        // Auth needed → let the caller run the auth flow and retry the load.
        Err(e) if auth::is_auth_required(&e) => Err(e),
        Err(e) => {
            let _ = event_tx.send(ThreadEvent::Error(format!(
                "couldn't resume the previous agent session ({e}); starting fresh"
            )));
            new_session(cx, cwd.to_path_buf()).await
        }
    }
}

/// The result of the auth card loop.
enum AuthOutcome {
    /// The user authenticated (or finished a terminal login) — retry the open.
    Retry,
    /// The connection is shutting down — end the worker quietly.
    Aborted,
}

/// Drive the auth card: surface the advertised methods, park for the user's pick,
/// and authenticate. An Agent/EnvVar method runs `authenticate`; a Terminal method
/// runs the agent's login command in an embedded terminal first. Returns `Retry`
/// once the user has acted (the caller retries the open) or `Aborted` on shutdown.
async fn run_auth(
    cx: &ConnectionTo<Agent>,
    methods: &[AuthMethodInfo],
    outbound_rx: &mut fmpsc::UnboundedReceiver<Outbound>,
    event_tx: &Sender<ThreadEvent>,
    command: &str,
    cwd: &Path,
) -> AuthOutcome {
    let mut error: Option<String> = None;
    loop {
        let _ = event_tx
            .send(ThreadEvent::AuthRequired { methods: methods.to_vec(), error: error.take() });
        // Park for the user's pick. Before the session opens any other outbound (a
        // queued prompt, a mode/config change) has nowhere to go, so it's dropped;
        // Shutdown or a closed channel aborts.
        let method_id = loop {
            match outbound_rx.next().await {
                Some(Outbound::Authenticate(id)) => break id,
                Some(Outbound::Shutdown) | None => return AuthOutcome::Aborted,
                // A prompt that raced in before the composer gated on the auth card
                // must be sealed as an error turn, else the app's optimistic
                // `turn_active` would wedge the composer forever. The user signs in
                // first; other outbound (mode/config) has no session yet, so drop it.
                Some(Outbound::Prompt { .. }) => {
                    let _ = event_tx.send(ThreadEvent::TurnEnded {
                        result: Some("sign in to continue".to_string()),
                        usage: None,
                        is_error: true, turn_diff: None });
                }
                Some(_) => continue,
            }
        };
        // A terminal-kind method authenticates via an interactive login TUI: run
        // the agent's login command inline, then retry the open — the login itself
        // is the authentication (no `authenticate` call).
        if let Some(AuthMethodKind::Terminal { args, env }) =
            methods.iter().find(|m| m.id == method_id).map(|m| &m.kind)
        {
            run_login_terminal(command, args, env, cwd, event_tx).await;
            return AuthOutcome::Retry;
        }
        // Agent / EnvVar → authenticate on the connection, then retry the open. On
        // failure, re-emit the card with the error note so the user can retry.
        match cx.send_request(AuthenticateRequest::new(method_id)).block_task().await {
            Ok(_) => return AuthOutcome::Retry,
            Err(e) => error = Some(e.to_string()),
        }
    }
}

/// Run a terminal-kind auth method's login command (the agent binary + advertised
/// args) in an embedded terminal, mount it inline via [`ThreadEvent::AuthTerminal`],
/// and block until the user finishes the login. Degrades to a notice (not a hang)
/// when no terminal host is installed or the spawn fails; the caller still retries
/// the open, so a stuck login re-shows the card rather than wedging the tab.
async fn run_login_terminal(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    event_tx: &Sender<ThreadEvent>,
) {
    let Some(host) = super::terminal_host() else {
        let _ = event_tx.send(ThreadEvent::Error(
            "terminal login isn't available in this build".to_string(),
        ));
        return;
    };
    let spec = client_terminal::TerminalSpawnSpec {
        command: command.to_string(),
        args: args.to_vec(),
        cwd: Some(cwd.to_path_buf()),
        // The method's advertised env vars for the login command (per spec).
        env: env.to_vec(),
        output_byte_limit: None,
    };
    match host.create(spec) {
        Ok(term_id) => {
            let _ = event_tx.send(ThreadEvent::AuthTerminal { terminal_id: term_id.clone() });
            // Block until the login command exits; then the caller retries the open.
            let _ = host.wait_for_exit(&term_id).await;
        }
        Err(e) => {
            let _ = event_tx
                .send(ThreadEvent::Error(format!("couldn't start the login terminal: {e}")));
        }
    }
}

/// Whether a mapped event carries transcript content (assistant/thought text,
/// tool activity, plan) as opposed to control state (mode, slash commands, model
/// config, title). During `session/load` replay we drop the former — OxiMux
/// already repainted its persisted blob — but let control updates through, since
/// they re-sync live pickers rather than re-render history.
fn is_transcript_event(ev: &ThreadEvent) -> bool {
    matches!(
        ev,
        ThreadEvent::AssistantTextDelta(_)
            | ThreadEvent::ThinkingDelta(_)
            | ThreadEvent::AssistantText(_)
            | ThreadEvent::AssistantThinking(_)
            | ThreadEvent::ToolCallStarted { .. }
            | ThreadEvent::ToolKind { .. }
            | ThreadEvent::ToolTerminal { .. }
            | ThreadEvent::ToolResult { .. }
            | ThreadEvent::ToolResultImages { .. }
            | ThreadEvent::ToolOutputDelta { .. }
            | ThreadEvent::PlanUpdated { .. }
    )
}

/// Build the `session/prompt` content blocks: the text block followed by one
/// base64 `Image` block per attachment. Images are included only when the agent
/// advertised `prompt_capabilities.image` — otherwise they're dropped and the
/// prompt carries text alone (the pre-image behavior), so a text-only agent sees
/// an unchanged request rather than a rejected turn. An image-only prompt (empty
/// text + images) omits the empty text block, mirroring the Claude image path.
fn prompt_blocks(text: String, images: &[ChatImage], supports_image: bool) -> Vec<ContentBlock> {
    let include_images = supports_image && !images.is_empty();
    let mut blocks = Vec::new();
    if !text.is_empty() || !include_images {
        blocks.push(ContentBlock::Text(TextContent::new(text)));
    }
    if include_images {
        for img in images {
            blocks.push(ContentBlock::Image(ImageContent::new(
                img.data.clone(),
                img.media_type.clone(),
            )));
        }
    }
    blocks
}

/// Decide a turn's `(result, is_error)` from the `session/prompt` outcome. A
/// protocol/transport error is fatal (today's behavior). Otherwise the response's
/// `stop_reason` decides: `Refusal`/`MaxTokens`/`MaxTurnRequests` end the turn
/// with the error banner + a human-readable reason (previously these rendered as
/// clean turns); `EndTurn`, `Cancelled` (the user's own Stop — mirrors the Claude
/// suppressed-interrupt convention), and any future `#[non_exhaustive]` variant
/// seal the turn cleanly.
fn turn_outcome(resp: Result<PromptResponse, agent_client_protocol::Error>) -> (Option<String>, bool) {
    match resp {
        Err(e) => (Some(e.to_string()), true),
        Ok(r) => match r.stop_reason {
            StopReason::Refusal => (Some("the agent refused the request".to_string()), true),
            StopReason::MaxTokens => (Some("the turn hit the agent's token limit".to_string()), true),
            StopReason::MaxTurnRequests => {
                (Some("the turn hit the agent's request limit".to_string()), true)
            }
            _ => (None, false),
        },
    }
}

/// Map an ACP `UsageUpdate{used,size,cost}` into our `TurnUsage`. ACP reports
/// only context occupancy + a cumulative cost, not the input/output/cache token
/// breakdown Claude does — so this is deliberately lossy: `used` fills the input
/// slot (and drives the "% ctx" readout against `size`), and the cost's amount is
/// carried; output/cache stay zero.
fn usage_from_acp(u: &UsageUpdate) -> TurnUsage {
    TurnUsage {
        input_tokens: u.used,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        context_window: Some(u.size),
        cost_usd: u.cost.as_ref().map(|c| c.amount),
    }
}

/// Map a JSON config value from the UI to an ACP `SessionConfigOptionValue`:
/// a bool → a boolean toggle, a string → a select value-id. Anything else has no
/// ACP config wire shape and is skipped (the request isn't sent).
fn config_value_from_json(value: Value) -> Option<SessionConfigOptionValue> {
    match value {
        Value::Bool(b) => Some(SessionConfigOptionValue::from(b)),
        Value::String(s) => Some(SessionConfigOptionValue::from(s.as_str())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::UsageUpdate;
    use serde_json::json;

    #[test]
    fn end_turn_and_cancelled_seal_the_turn_cleanly() {
        // A normal completion and a user-initiated cancel both render as a clean
        // turn (no error banner) — the Cancelled case mirrors Claude's suppressed
        // interrupt.
        assert_eq!(turn_outcome(Ok(PromptResponse::new(StopReason::EndTurn))), (None, false));
        assert_eq!(turn_outcome(Ok(PromptResponse::new(StopReason::Cancelled))), (None, false));
    }

    #[test]
    fn refusal_and_limits_end_with_the_error_banner() {
        // Refusal / token limit / request limit previously rendered as successful
        // turns; each now ends with the error banner and a human-readable reason.
        let (msg, is_error) = turn_outcome(Ok(PromptResponse::new(StopReason::Refusal)));
        assert!(is_error);
        assert_eq!(msg.as_deref(), Some("the agent refused the request"));

        let (msg, is_error) = turn_outcome(Ok(PromptResponse::new(StopReason::MaxTokens)));
        assert!(is_error);
        assert_eq!(msg.as_deref(), Some("the turn hit the agent's token limit"));

        let (msg, is_error) = turn_outcome(Ok(PromptResponse::new(StopReason::MaxTurnRequests)));
        assert!(is_error);
        assert_eq!(msg.as_deref(), Some("the turn hit the agent's request limit"));
    }

    fn img(mime: &str, data: &str) -> ChatImage {
        ChatImage { media_type: mime.to_string(), data: data.to_string() }
    }

    fn as_image(block: &ContentBlock) -> &ImageContent {
        match block {
            ContentBlock::Image(ic) => ic,
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn prompt_blocks_text_only_is_a_single_text_block() {
        let blocks = prompt_blocks("hi".into(), &[], true);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "hi"));
    }

    #[test]
    fn prompt_blocks_text_plus_two_images_builds_three_blocks() {
        let imgs = [img("image/png", "QUJD"), img("image/gif", "R0lG")];
        let blocks = prompt_blocks("look".into(), &imgs, true);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "look"));
        // mime + data survive intact, in order.
        assert_eq!(as_image(&blocks[1]).mime_type, "image/png");
        assert_eq!(as_image(&blocks[1]).data, "QUJD");
        assert_eq!(as_image(&blocks[2]).mime_type, "image/gif");
        assert_eq!(as_image(&blocks[2]).data, "R0lG");
    }

    #[test]
    fn prompt_blocks_image_only_omits_empty_text_block() {
        let blocks = prompt_blocks(String::new(), &[img("image/png", "QUJD")], true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(as_image(&blocks[0]).data, "QUJD");
    }

    #[test]
    fn prompt_blocks_drops_images_when_capability_off() {
        // An agent without `prompt_capabilities.image` gets a text-only request
        // (images dropped) rather than a rejected turn.
        let blocks = prompt_blocks("look".into(), &[img("image/png", "QUJD")], false);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "look"));
    }

    #[test]
    fn transcript_events_are_gated_but_control_events_pass_during_replay() {
        // Transcript-bearing updates are dropped during session/load replay;
        // control updates (mode/slash/model/title) always pass so live pickers
        // stay synced.
        assert!(is_transcript_event(&ThreadEvent::AssistantTextDelta("hi".into())));
        assert!(is_transcript_event(&ThreadEvent::ThinkingDelta("hm".into())));
        assert!(is_transcript_event(&ThreadEvent::ToolCallStarted {
            id: "c".into(),
            name: "Read".into(),
            input: json!({}),
        }));
        assert!(is_transcript_event(&ThreadEvent::PlanUpdated { entries: vec![] }));

        assert!(!is_transcript_event(&ThreadEvent::ModeChanged { mode_id: "x".into() }));
        assert!(!is_transcript_event(&ThreadEvent::ControlsUpdated));
        assert!(!is_transcript_event(&ThreadEvent::TitleUpdated { title: "t".into() }));
        assert!(!is_transcript_event(&ThreadEvent::SlashCommandsUpdated {
            commands: vec![],
            descriptions: vec![],
            hints: vec![],
        }));
    }

    #[test]
    fn new_session_request_serializes_empty_mcp_servers() {
        // Some ACP agents reject a `session/new` that omits `mcpServers`. v1
        // NewSessionRequest has no skip_serializing_if, so `::new(cwd)` already
        // serializes `[]`; lock it so a future crate bump can't silently regress.
        let v = serde_json::to_value(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
            .expect("serialize NewSessionRequest");
        assert_eq!(v["mcpServers"], json!([]));
    }

    #[test]
    fn transport_error_stays_fatal() {
        // A protocol/transport failure is surfaced as an error turn (unchanged).
        let (msg, is_error) =
            turn_outcome(Err(agent_client_protocol::Error::new(-32603, "boom")));
        assert!(is_error);
        assert!(msg.unwrap().contains("boom"));
    }

    #[test]
    fn usage_maps_context_lossily() {
        // `used` fills the input slot (drives the % ctx readout); `size` is the
        // window; output/cache stay zero (ACP has no breakdown).
        let u = usage_from_acp(&UsageUpdate::new(45_000, 200_000));
        assert_eq!(u.input_tokens, 45_000);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_read_tokens, 0);
        assert_eq!(u.context_window, Some(200_000));
        assert_eq!(u.cost_usd, None, "no cost when the agent omits it");
    }

    #[test]
    fn config_value_maps_bool_and_string_and_skips_others() {
        assert!(matches!(
            config_value_from_json(json!(true)),
            Some(SessionConfigOptionValue::Boolean { value: true })
        ));
        assert!(config_value_from_json(json!("high")).is_some());
        // A number/object/null has no ACP config shape → skipped.
        assert!(config_value_from_json(json!(3)).is_none());
        assert!(config_value_from_json(json!(null)).is_none());
    }
}
