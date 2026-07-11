//! A deterministic mock ACP **agent** that requires an ENV-VAR credential — for
//! verifying OxiMux's EnvVar-auth "respawn with env, then authenticate" flow.
//!
//! It advertises one `env_var`-kind auth method at `initialize` (variable
//! `OXIMUX_MOCK_KEY`) and fails `session/new` with `AuthRequired` (-32000) until
//! BOTH the env var is present in its process environment AND the client has
//! `authenticate`d. Because a running process can't gain env after it spawned, a
//! client that only calls `authenticate` on the original process never gets in —
//! it must RESPAWN the agent with the var set, then authenticate. The companion
//! `acp_env_auth_smoke` drives it.
//!
//! Build:  `cargo build -p oximux-agents --example mock_acp_env_auth_agent`
//! Binary: `target/debug/examples/mock_acp_env_auth_agent`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthEnvVar, AuthMethod, AuthMethodEnvVar, AuthenticateRequest,
    AuthenticateResponse, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Error, Responder, Result, Stdio};

/// JSON-RPC `AuthRequired`.
const AUTH_REQUIRED: i32 = -32000;
/// The env var this agent's credential rides in.
const KEY_VAR: &str = "OXIMUX_MOCK_KEY";

/// Whether the credential is actually present in THIS process's environment —
/// true only after a respawn that set it, never on the original env-less spawn.
fn key_present() -> bool {
    std::env::var(KEY_VAR).map(|v| !v.is_empty()).unwrap_or(false)
}

fn main() -> Result<()> {
    futures::executor::block_on(run())
}

async fn run() -> Result<()> {
    // Flipped true once the client authenticates — but only granted when the key
    // is actually set, so an in-place authenticate on the env-less process fails.
    let authed = Arc::new(AtomicBool::new(false));
    Agent
        .builder()
        .name("mock-env-auth-agent")
        .on_receive_request(
            async move |init: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_capabilities(AgentCapabilities::new())
                        .auth_methods(vec![AuthMethod::EnvVar(AuthMethodEnvVar::new(
                            "api-key",
                            "API Key",
                            vec![AuthEnvVar::new(KEY_VAR)],
                        ))]),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let authed = authed.clone();
                async move |_req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                    if key_present() && authed.load(Ordering::SeqCst) {
                        responder.respond(NewSessionResponse::new(SessionId::new("mock-env-session")))
                    } else {
                        responder.respond_with_error(Error::new(
                            AUTH_REQUIRED,
                            "set the API key and authenticate",
                        ))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let authed = authed.clone();
                async move |_req: AuthenticateRequest,
                            responder: Responder<AuthenticateResponse>,
                            _cx| {
                    // Authentication only succeeds once the credential is actually in
                    // the process env — i.e. after a respawn-with-env, not an in-place
                    // authenticate on the original env-less process.
                    if key_present() {
                        authed.store(true, Ordering::SeqCst);
                        responder.respond(AuthenticateResponse::new())
                    } else {
                        responder
                            .respond_with_error(Error::new(AUTH_REQUIRED, "API key env var not set"))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                say(&cx, &req.session_id, "env auth ok");
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
