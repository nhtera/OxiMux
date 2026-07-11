//! A deterministic mock ACP **agent** that advertises `loadSession` and replays
//! history on `session/load` — for verifying OxiMux's true-restore path: the
//! client must send `session/load` (not `session/new`) with the stored id, DROP
//! the replayed transcript (it repaints its own persisted blob), and still render
//! live updates after the load resolves.
//!
//! On `session/load` it replays one `REPLAYED_HISTORY` message chunk, then
//! resolves; on the next prompt it emits a `LIVE_REPLY` chunk. The companion
//! `acp_load_smoke` asserts the replay was suppressed and the live reply shown.
//!
//! Build:  `cargo build -p oximux-agents --example mock_acp_load_agent`
//! Binary: `target/debug/examples/mock_acp_load_agent`

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Responder, Result, Stdio};

fn main() -> Result<()> {
    futures::executor::block_on(run())
}

async fn run() -> Result<()> {
    Agent
        .builder()
        .name("mock-load-agent")
        .on_receive_request(
            async move |init: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                // Advertise loadSession so the client takes the session/load path.
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_capabilities(AgentCapabilities::new().load_session(true)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                // Only reached on the fallback path (client didn't send load).
                responder.respond(NewSessionResponse::new(SessionId::new("mock-load-fresh")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                // Replay one history message BEFORE resolving — the client must
                // drop it (replay-before-response, per spec).
                say(&cx, &req.session_id, "REPLAYED_HISTORY");
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                // A live turn AFTER the load — this one must reach the client.
                say(&cx, &req.session_id, "LIVE_REPLY");
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, _cx: ConnectionTo<Client>| match message {
                Dispatch::Response(result, router) => router.respond_with_result(result),
                Dispatch::Notification(_) => Ok(()),
                Dispatch::Request(_req, responder) => responder.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled request"),
                ),
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Send a plain assistant message chunk.
fn say(cx: &ConnectionTo<Client>, sid: &SessionId, text: &str) {
    let _ = cx.send_notification(SessionNotification::new(
        sid.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text.to_string(),
        )))),
    ));
}
