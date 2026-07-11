//! A deterministic mock ACP **agent** that exercises the client's cancel path:
//! on every prompt it sends a `session/request_permission` and parks on the
//! response, then reports (as an assistant message the client can observe) which
//! outcome it received. Point the companion `acp_cancel_smoke` example at it to
//! prove that a client `session/cancel` resolves an outstanding permission
//! request with a `Cancelled` outcome instead of hanging the agent forever.
//!
//! Build:  `cargo build -p oximux-agents --example mock_acp_cancel_agent`
//! Binary: `target/debug/examples/mock_acp_cancel_agent`

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallUpdate,
    ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Responder, Result, Stdio};

fn main() -> Result<()> {
    futures::executor::block_on(run())
}

async fn run() -> Result<()> {
    Agent
        .builder()
        .name("mock-cancel-agent")
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
                responder.respond(NewSessionResponse::new(SessionId::new("mock-cancel-session")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                // The permission request awaits the client's answer via
                // `block_task`, only safe OFF the event-loop handler — so run it
                // (and send the prompt response) in a concurrent spawned task.
                let sid = req.session_id.clone();
                let cx2 = cx.clone();
                cx.spawn(async move {
                    let outcome = request_permission(&sid, &cx2).await;
                    let label = match outcome {
                        Some(RequestPermissionOutcome::Cancelled) => "cancelled",
                        Some(RequestPermissionOutcome::Selected(_)) => "selected",
                        _ => "none",
                    };
                    // Report the outcome as a message the smoke driver observes.
                    say(&cx2, &sid, &format!("PERMISSION_OUTCOME: {label}"));
                    responder.respond(PromptResponse::new(StopReason::Cancelled))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, _cx: ConnectionTo<Client>| {
                // A catch-all must ROUTE responses to the requests this agent sent
                // (session/request_permission) — not error them.
                match message {
                    Dispatch::Response(result, router) => router.respond_with_result(result),
                    Dispatch::Notification(_) => Ok(()),
                    Dispatch::Request(_req, responder) => responder.respond_with_error(
                        agent_client_protocol::util::internal_error("unhandled request"),
                    ),
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Ask the client for permission and return the outcome it answered with (or
/// `None` if the request itself failed on the wire).
async fn request_permission(
    sid: &SessionId,
    cx: &ConnectionTo<Client>,
) -> Option<RequestPermissionOutcome> {
    let tool_call = ToolCallUpdate::new("call-perm", ToolCallUpdateFields::new());
    let options = vec![
        PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ];
    match cx
        .send_request(RequestPermissionRequest::new(sid.clone(), tool_call, options))
        .block_task()
        .await
    {
        Ok(resp) => Some(resp.outcome),
        Err(_) => None,
    }
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
