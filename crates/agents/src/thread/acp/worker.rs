//! The ACP session worker: runs the `agent-client-protocol` client against any
//! configured ACP CLI subprocess (Cursor, Amp, Gemini, …) under
//! `futures::executor::block_on` on a dedicated thread. It bridges our sync
//! `Outbound` queue to async prompts and streams `SessionUpdate`s back as
//! [`ThreadEvent`]s.
//!
//! The SDK is executor-agnostic (subprocess I/O via `async_process` + `blocking`,
//! no Tokio reactor), so a plain `block_on` on an owned thread mirrors the
//! Claude/Codex reader-thread ownership without pulling in a runtime.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, CreateTerminalRequest, CreateTerminalResponse,
    FileSystemCapabilities, InitializeRequest, KillTerminalRequest, KillTerminalResponse,
    NewSessionRequest, PromptRequest, ReadTextFileRequest, ReadTextFileResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionConfigId, SessionConfigOptionValue,
    SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionModeRequest, TerminalOutputRequest, TerminalOutputResponse, TextContent, UsageUpdate,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo, Responder};
use futures::StreamExt;
use futures::channel::mpsc as fmpsc;
use futures::channel::oneshot;
use serde_json::Value;

use super::map::map_session_update;
use super::{AcpState, Outbound, approvals, client_fs, client_terminal};
use crate::thread::event::{ThreadEvent, TurnUsage};
use crate::thread::tool_call::PermissionDecision;

/// Run the whole ACP session to completion (blocks the worker thread). A spawn
/// or protocol failure is surfaced as a [`ThreadEvent::Error`] so the app's
/// disconnect path fires (the same degradation as a Claude spawn failure).
pub(crate) fn run(
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    event_tx: Sender<ThreadEvent>,
    outbound_rx: fmpsc::UnboundedReceiver<Outbound>,
    state: Arc<Mutex<AcpState>>,
) {
    let result =
        futures::executor::block_on(session(command, args, cwd, event_tx.clone(), outbound_rx, state));
    if let Err(e) = result {
        let _ = event_tx.send(ThreadEvent::Error(e));
    }
}

async fn session(
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    event_tx: Sender<ThreadEvent>,
    mut outbound_rx: fmpsc::UnboundedReceiver<Outbound>,
    state: Arc<Mutex<AcpState>>,
) -> Result<(), String> {
    // `AcpAgent::from_str` splits on whitespace into program + argv (verified in
    // the crate's `yolo_one_shot_client` example), so join command + args.
    let cmdline =
        if args.is_empty() { command } else { format!("{} {}", command, args.join(" ")) };
    let agent = AcpAgent::from_str(&cmdline).map_err(|e| format!("spawn `{cmdline}`: {e}"))?;

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
                        // `TurnEnded.usage`, keeping the footer turn-scoped.
                        if let SessionUpdate::UsageUpdate(u) = &n.update {
                            if let Ok(mut s) = st.lock() {
                                s.last_usage = Some(usage_from_acp(u));
                            }
                            return Ok(());
                        }
                        // A runtime config push carries the FULL option set (models /
                        // reasoning + current values). Swap it into state so
                        // `models()`/`current_model()` read the live vocabulary, then
                        // fall through: the mapper emits `ControlsUpdated` so the
                        // composer re-pulls its pickers.
                        if let SessionUpdate::ConfigOptionUpdate(u) = &n.update {
                            if let Ok(mut s) = st.lock() {
                                s.config_options = u.config_options.clone();
                            }
                        }
                        for ev in map_session_update(n.update) {
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
            let _init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(init_caps))
                .block_task()
                .await?;
            let sess = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?;
            let session_id = sess.session_id.clone();

            // Resolve capabilities from what the agent actually advertised at the
            // handshake (modes → picker, config_options → config control). Slash +
            // usage are wired via `session/update`s, so the cap is on and the
            // affordance stays empty/hidden until one arrives.
            let config_options = sess.config_options.clone().unwrap_or_default();
            let caps = super::caps_from_handshake(sess.modes.as_ref(), &config_options);

            // Stash the live connection + session id (so `cancel()`/`set_mode()`
            // can fire out-of-band) AND the discovered modes/config/caps BEFORE
            // `SessionInit` is emitted — so the app reads real caps the moment it
            // processes init and lights up the pickers.
            if let Ok(mut s) = state.lock() {
                s.session_id = Some(session_id.clone());
                s.connection = Some(cx.clone());
                s.modes = sess.modes.clone();
                s.config_options = config_options;
                s.caps = caps;
            }
            let _ = event_tx.send(ThreadEvent::SessionInit {
                session_id: session_id.0.to_string(),
                model: String::new(),
                permission_mode: sess
                    .modes
                    .as_ref()
                    .map(|m| m.current_mode_id.0.to_string())
                    .unwrap_or_default(),
                slash_commands: Vec::new(),
            });

            // Prompt loop: one turn per queued Outbound::Prompt. `block_task()`
            // resolves when the turn ends (updates stream via the handler above,
            // concurrently). Draining the sender (all clones gone) also ends it.
            while let Some(msg) = outbound_rx.next().await {
                match msg {
                    Outbound::Prompt(text) => {
                        let resp = cx
                            .send_request(PromptRequest::new(
                                session_id.clone(),
                                vec![ContentBlock::Text(TextContent::new(text))],
                            ))
                            .block_task()
                            .await;
                        // The streamed chunks are authoritative and already built
                        // the assistant blocks; `TurnEnded` seals the window (no
                        // finalized text — that would clobber tool-interleaved text).
                        // Fold the latest stashed usage (cloned, not taken — the
                        // context readout is cumulative, so it stays accurate on a
                        // later turn that reports no fresh usage).
                        let err = resp.err();
                        let usage = state.lock().ok().and_then(|s| s.last_usage.clone());
                        let _ = event_tx.send(ThreadEvent::TurnEnded {
                            result: err.as_ref().map(|e| e.to_string()),
                            usage,
                            is_error: err.is_some(),
                        });
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
                            let _ = cx
                                .send_request(SetSessionConfigOptionRequest::new(
                                    session_id.clone(),
                                    SessionConfigId::new(id),
                                    val,
                                ))
                                .block_task()
                                .await;
                        }
                    }
                    Outbound::Shutdown => break,
                }
            }
            Ok::<(), agent_client_protocol::Error>(())
        });

    connect.await.map_err(|e| format!("acp connection: {e}"))
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
