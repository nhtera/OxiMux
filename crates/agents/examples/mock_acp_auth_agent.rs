//! A deterministic mock ACP **agent** that requires authentication — for
//! verifying OxiMux's auth flow: the client must surface the advertised methods,
//! call `authenticate`, and retry the session open on the same connection.
//!
//! It advertises one `agent`-kind auth method at `initialize` and fails
//! `session/new` with `AuthRequired` (-32000) until `authenticate` is called;
//! after that, `session/new` succeeds. The companion `acp_auth_smoke` drives it.
//!
//! Build:  `cargo build -p oximux-agents --example mock_acp_auth_agent`
//! Binary: `target/debug/examples/mock_acp_auth_agent`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthenticateRequest, AuthenticateResponse,
    ContentBlock, ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, SessionId, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Error, Responder, Result, Stdio};

/// JSON-RPC `AuthRequired`.
const AUTH_REQUIRED: i32 = -32000;

fn main() -> Result<()> {
    futures::executor::block_on(run())
}

async fn run() -> Result<()> {
    // Shared across handlers: flipped true once the client authenticates.
    let authed = Arc::new(AtomicBool::new(false));
    Agent
        .builder()
        .name("mock-auth-agent")
        .on_receive_request(
            async move |init: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                            "oauth",
                            "Sign in with Google",
                        ))]),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let authed = authed.clone();
                async move |_req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                    if authed.load(Ordering::SeqCst) {
                        responder.respond(NewSessionResponse::new(SessionId::new("mock-auth-session")))
                    } else {
                        responder.respond_with_error(Error::new(AUTH_REQUIRED, "authentication required"))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let authed = authed.clone();
                async move |_req: AuthenticateRequest, responder: Responder<AuthenticateResponse>, _cx| {
                    authed.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder: Responder<PromptResponse>, cx: ConnectionTo<Client>| {
                say(&cx, &req.session_id, "authenticated and ready");
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
