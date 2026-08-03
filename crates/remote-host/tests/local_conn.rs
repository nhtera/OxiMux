//! Local-connection authority tests over the in-memory loopback: what a
//! `serve_local` caller may reach at each scope, and — the property the whole
//! `Peer` split exists for — that no remote conversation can mint local
//! authority.

use std::sync::Arc;

use futures::executor::block_on;
use futures::future::join;
use oximux_agent_core::thread::ThreadEvent;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope, PairingSlot, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{RegisterReq, SendPromptReq};
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

/// A registry with two sessions, so scope confinement has something to leak.
fn two_session_registry() -> Arc<SessionRegistry> {
    let registry = Arc::new(SessionRegistry::new());
    for id in ["sess-1", "sess-2"] {
        let handle = registry.register(id.into(), Arc::new(StubConnection::default()));
        handle.ingest(ThreadEvent::AssistantText(format!("hello from {id}")));
    }
    registry
}

fn dispatcher(registry: Arc<SessionRegistry>, auth: Arc<AuthStore>) -> Dispatcher {
    Dispatcher::new(registry, auth).with_clock(clock)
}

fn send_prompt(session_id: &str) -> Request {
    Request::SendPrompt(SendPromptReq {
        session_id: session_id.into(),
        text: "go".into(),
        images: vec![],
        corr_id: 1,
    })
}

/// A full-scope local caller is the operator: no pairing, immediate access,
/// reads and writes served.
#[test]
fn local_full_scope_serves_without_pairing() {
    let dispatcher = dispatcher(two_session_registry(), Arc::new(AuthStore::new()));
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let Response::Sessions(sessions) = call(&client, Request::ListSessions).await else {
            panic!("expected Sessions");
        };
        assert_eq!(sessions.len(), 2, "operator sees every session");
        assert_eq!(call(&client, send_prompt("sess-2")).await, Response::Ack, "operator writes");
        drop(client);
    };
    block_on(join(serve, script));
}

/// A session-scoped local caller (an agent presenting its own session id) is
/// confined exactly as a session-scoped paired device is: its own session
/// works, everything else — other sessions, session creation, terminals,
/// schedules, project enumeration — is refused.
#[test]
fn local_session_scope_is_confined_to_its_session() {
    let dispatcher = dispatcher(two_session_registry(), Arc::new(AuthStore::new()));
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Session("sess-1".into()));
    let script = async move {
        // Its own session: read and write both served.
        let Response::Sessions(sessions) = call(&client, Request::ListSessions).await else {
            panic!("expected Sessions");
        };
        assert_eq!(sessions.len(), 1, "a narrowed caller must not enumerate other sessions");
        assert_eq!(sessions[0].session_id, "sess-1");
        assert_eq!(call(&client, send_prompt("sess-1")).await, Response::Ack);

        // Everything past the scope: refused, including creating its way out.
        for (what, req) in [
            ("cross-session write", send_prompt("sess-2")),
            ("cross-session read", Request::EventsSince { session_id: "sess-2".into(), after_seq: 0 }),
            (
                "session creation",
                Request::CreateSession { cwd: "/tmp".into(), agent_id: None },
            ),
            ("terminal list", Request::ListTerminals),
            ("schedule list", Request::ListSchedules),
            ("project list", Request::ListProjects),
        ] {
            assert_eq!(
                call(&client, req).await,
                Response::Error(RpcError::Unauthorized),
                "{what} must be refused for a session-scoped local caller"
            );
        }
        drop(client);
    };
    block_on(join(serve, script));
}

/// The remote handshake is refused on a local connection — it could only
/// replace already-proven local authority with something weaker (or launder a
/// local caller into the paired-device world).
#[test]
fn local_conn_refuses_the_remote_handshake() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = dispatcher(two_session_registry(), auth);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let script = async move {
        let pubkey = [0x44; 32];
        let register = Request::Register(RegisterReq {
            app_pubkey: pubkey,
            device_name: "imposter".into(),
            proof: registration_proof(&SECRET, &pubkey, NOW),
            timestamp_secs: NOW,
            session_id: None,
        });
        assert!(
            matches!(call(&client, register).await, Response::Error(RpcError::BadRequest(_))),
            "a valid pairing proof is still refused on a local connection"
        );
        // The refusal did not disturb the local authority already held.
        assert_eq!(call(&client, send_prompt("sess-1")).await, Response::Ack);
        drop(client);
    };
    block_on(join(serve, script));
}

/// No remote conversation reaches local authority. Driven behaviorally through
/// the public serve loop: `Unpair` is the RPC whose answer distinguishes the
/// two peer kinds (a remote device un-enrolls with `Ack`; a local caller is
/// told it has no enrollment), so a fully-authenticated remote connection
/// answering `Ack` proves it is served as a remote peer — and an
/// unauthenticated one gets nothing at all.
#[test]
fn remote_conn_never_gains_local_authority() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = dispatcher(two_session_registry(), auth);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        // Before any handshake: nothing — a remote conn starts with no ambient
        // authority (a local conn starts with all of it; see the tests above).
        assert_eq!(
            call(&client, Request::ListSessions).await,
            Response::Error(RpcError::Unauthorized)
        );
        // After the strongest thing a remote peer can do (full pairing), the
        // connection is served as a *remote* device, not a local caller.
        let pubkey = [0x55; 32];
        let register = Request::Register(RegisterReq {
            app_pubkey: pubkey,
            device_name: "phone".into(),
            proof: registration_proof(&SECRET, &pubkey, NOW),
            timestamp_secs: NOW,
            session_id: None,
        });
        assert!(matches!(call(&client, register).await, Response::Registered { .. }));
        assert_eq!(
            call(&client, Request::Unpair).await,
            Response::Ack,
            "a paired remote device un-enrolls — the answer a local caller never gets"
        );
        drop(client);
    };
    block_on(join(serve, script));
}
