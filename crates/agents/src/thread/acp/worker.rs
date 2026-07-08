//! The ACP session worker: runs the `agent-client-protocol` client against a
//! `gemini --experimental-acp` subprocess under `futures::executor::block_on` on
//! a dedicated thread. It bridges our sync `Outbound` queue to async prompts and
//! streams `SessionUpdate`s back as [`ThreadEvent`]s.
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
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo};
use futures::StreamExt;
use futures::channel::mpsc as fmpsc;

use super::map::map_session_update;
use super::{AcpState, Outbound};
use crate::thread::event::ThreadEvent;

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
                move |n: SessionNotification, _cx: ConnectionTo<Agent>| {
                    let tx = tx.clone();
                    async move {
                        for ev in map_session_update(n.update) {
                            let _ = tx.send(ev);
                        }
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // Phase 1 declines every permission request (safe default). Phase 3 routes
        // it to the UI's approval card and answers with the user's choice.
        .on_receive_request(
            move |_req: RequestPermissionRequest,
                  responder: agent_client_protocol::Responder<RequestPermissionResponse>,
                  _cx: ConnectionTo<Agent>| async move {
                responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |cx: ConnectionTo<Agent>| async move {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
            let sess = cx.send_request(NewSessionRequest::new(cwd)).block_task().await?;
            let session_id = sess.session_id.clone();

            // Stash the live connection + session id so `cancel()` can fire
            // `session/cancel` out-of-band while a prompt future is parked.
            if let Ok(mut s) = state.lock() {
                s.session_id = Some(session_id.clone());
                s.connection = Some(cx.clone());
            }
            let _ = event_tx.send(ThreadEvent::SessionInit {
                session_id: session_id.0.to_string(),
                model: String::new(),
                permission_mode: String::new(),
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
                        let err = resp.err();
                        let _ = event_tx.send(ThreadEvent::TurnEnded {
                            result: err.as_ref().map(|e| e.to_string()),
                            usage: None,
                            is_error: err.is_some(),
                        });
                    }
                    Outbound::Shutdown => break,
                }
            }
            Ok::<(), agent_client_protocol::Error>(())
        });

    connect.await.map_err(|e| format!("acp connection: {e}"))
}
