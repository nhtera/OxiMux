//! `Steer` against backends that can and cannot take a message mid-turn.
//!
//! Only `pi` advertises `supports_steer`; claude, codex and ACP all fall through to
//! the default `AgentConnection::steer`, which bails. That refusal used to travel the
//! generic session-command path and reach the client as
//! `Internal("session command failed")` — a transient-looking error for a permanent,
//! knowable property, with the real sentence going only to the host's log. A caller
//! could not tell "retry later" from "this will never work".

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{AgentCapabilities, StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn register_req(pubkey: [u8; 32]) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&SECRET, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: None,
    }
}

/// One session whose backend does or does not offer a mid-turn queue.
fn registry_with_steer(supports_steer: bool) -> (Arc<SessionRegistry>, Arc<StubConnection>) {
    let conn = Arc::new(
        StubConnection::default()
            .with_capabilities(AgentCapabilities { supports_steer, ..Default::default() }),
    );
    let registry = Arc::new(SessionRegistry::new());
    registry.register("sess-1".into(), conn.clone());
    (registry, conn)
}

/// Run `script` against a dispatcher over the in-memory pair.
async fn against<F, Fut>(registry: Arc<SessionRegistry>, script: F)
where
    F: FnOnce(oximux_remote_proto::testing::DuplexTransport) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    futures::future::join(serve, script(client)).await;
}

/// A backend with no mid-turn queue answers `Unsupported` — the capability class,
/// not the internal-fault class.
///
/// `Unsupported` also carries no host-authored text, which is the constraint that
/// rules out the obvious alternative of forwarding the backend's own sentence:
/// `set_choice` documents why raw backend strings must not reach a client.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backend_without_a_mid_turn_queue_reports_unsupported() {
    let (registry, conn) = registry_with_steer(false);
    against(registry, |client| async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let reply =
            call(&client, Request::Steer { session_id: "sess-1".into(), text: "focus".into() })
                .await;
        assert_eq!(
            reply,
            Response::Error(RpcError::Unsupported),
            "a permanent capability gap must not read as a transient internal fault",
        );
        drop(client);
    })
    .await;

    // Refused *before* the backend was touched. Reaching it and discarding the
    // result would look identical on the wire but leave the door open for a
    // backend that half-accepts.
    assert!(
        conn.sent().iter().all(|v| v["type"] != "steer"),
        "nothing should have been sent to a backend that cannot take it: {:?}",
        conn.sent(),
    );
}

/// The capable path is untouched: a steer-capable backend still acks, and the
/// guidance still reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_capable_backend_still_acks_and_receives_it() {
    let (registry, conn) = registry_with_steer(true);
    against(registry, |client| async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        assert_eq!(
            call(&client, Request::Steer { session_id: "sess-1".into(), text: "focus".into() })
                .await,
            Response::Ack,
        );
        drop(client);
    })
    .await;

    assert!(
        conn.sent().iter().any(|v| v["type"] == "steer" && v["message"] == "focus"),
        "the guidance reached the backend, saw {:?}",
        conn.sent(),
    );
}

/// The capability check does not become a way to probe for sessions: an unknown id
/// is still `UnknownSession`, and an unauthorized peer is still refused, both
/// *before* any capability is read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_is_still_unknown_not_unsupported() {
    let (registry, _conn) = registry_with_steer(false);
    against(registry, |client| async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        assert_eq!(
            call(&client, Request::Steer { session_id: "nope".into(), text: "hi".into() }).await,
            Response::Error(RpcError::UnknownSession),
        );
        drop(client);
    })
    .await;
}
