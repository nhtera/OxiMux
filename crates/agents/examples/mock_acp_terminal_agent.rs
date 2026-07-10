//! A deterministic mock ACP **agent** that exercises the client's `terminal/*`
//! path — for verifying OxiMux's ACP embedded-terminal feature without depending
//! on whether a third-party agent (gemini/cursor-agent/amp) happens to use
//! terminals for a given prompt.
//!
//! On every prompt it: opens a tool card, calls `terminal/create` (a short shell
//! loop), embeds the returned terminal in the card (`ToolCallContent::Terminal`),
//! waits for it to exit, marks the card completed, and ends the turn. Point
//! OxiMux's ACP chat at this binary, or drive it headlessly with the companion
//! `acp_terminal_smoke` example.
//!
//! Build:  `cargo build -p oximux-agents --example mock_acp_terminal_agent`
//! Binary: `target/debug/examples/mock_acp_terminal_agent`

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, CreateTerminalRequest, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, Terminal, TextContent, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Responder, Result, Stdio};

/// The shell command the embedded terminal runs — a visible, ~2.5s stream so the
/// inline terminal is observably live before it exits.
const DEMO_CMD: &str = "for i in $(seq 1 6); do echo \"tick $i\"; sleep 0.4; done; echo DONE";

fn main() -> Result<()> {
    // Executor-agnostic SDK (subprocess I/O via async_process), so a plain
    // block_on on the main thread runs the whole agent — no tokio runtime needed.
    futures::executor::block_on(run())
}

async fn run() -> Result<()> {
    Agent
        .builder()
        .name("mock-terminal-agent")
        .on_receive_request(
            async move |init: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                responder.respond(NewSessionResponse::new(SessionId::new("mock-terminal-session")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                // The terminal work awaits client responses via `block_task`, which
                // is only safe OFF the event-loop handler — so run it (and send the
                // prompt response) inside a concurrent spawned task.
                let sid = req.session_id.clone();
                let cx2 = cx.clone();
                cx.spawn(async move {
                    run_terminal_turn(&sid, &cx2).await;
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, _cx: ConnectionTo<Client>| {
                // CRITICAL: a catch-all must ROUTE responses to the requests this
                // agent sent (terminal/create, wait_for_exit) — not error them, or
                // it hijacks its own pending requests. Only unhandled *requests*
                // error; notifications are ignored.
                match message {
                    Dispatch::Response(result, router) => router.respond_with_result(result),
                    Dispatch::Notification(_) => Ok(()),
                    Dispatch::Request(_req, responder) => responder
                        .respond_with_error(agent_client_protocol::util::internal_error(
                            "unhandled request",
                        )),
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Drive one full embedded-terminal tool call against the client.
async fn run_terminal_turn(sid: &SessionId, cx: &ConnectionTo<Client>) {
    let sid = sid.clone();
    let card = "call-term";

    // Open the tool card.
    notify(cx, &sid, SessionUpdate::ToolCall(ToolCall::new(card, "Run a demo command")
        .status(ToolCallStatus::InProgress)));

    // Ask the client to spawn the command in a terminal.
    let created = cx
        .send_request(
            CreateTerminalRequest::new(sid.clone(), "bash")
                .args(vec!["-lc".to_string(), DEMO_CMD.to_string()]),
        )
        .block_task()
        .await;

    match created {
        Ok(resp) => {
            let term_id = resp.terminal_id.clone();
            // Embed the live terminal in the tool card.
            notify(cx, &sid, SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                card,
                ToolCallUpdateFields::new()
                    .content(vec![ToolCallContent::Terminal(Terminal::new(term_id.clone()))]),
            )));
            // Wait for the command to finish, then settle the card. (Deliberately
            // NOT released here — leaving it alive lets the client mount + keep
            // rendering the terminal; the client reaps it on tab close.)
            let _ = cx
                .send_request(agent_client_protocol::schema::v1::WaitForTerminalExitRequest::new(
                    sid.clone(),
                    term_id,
                ))
                .block_task()
                .await;
            notify(cx, &sid, SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                card,
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )));
            say(cx, &sid, "Ran the demo command in an embedded terminal above.");
        }
        Err(e) => {
            say(cx, &sid, &format!("terminal/create failed: {e}"));
        }
    }
}

/// Send a `session/update` notification.
fn notify(cx: &ConnectionTo<Client>, sid: &SessionId, update: SessionUpdate) {
    let _ = cx.send_notification(SessionNotification::new(sid.clone(), update));
}

/// Send a plain assistant message chunk.
fn say(cx: &ConnectionTo<Client>, sid: &SessionId, text: &str) {
    notify(
        cx,
        sid,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new(text.to_string()),
        ))),
    );
}
