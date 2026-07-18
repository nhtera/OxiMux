//! The end-to-end proof: the client [`RemoteSession`] drives the real
//! `remote-host` `Dispatcher` over the in-memory loopback — pairing, commands, and
//! reconnect all cross our actual wire code on both sides, with no network.

use std::sync::Arc;

use futures::executor::block_on;
use futures::future::join;
use oximux_agent_core::thread::{PermissionDecision, PermissionKind, ThreadEvent};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{AgentCapabilities, StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_proto::PairingTicket;
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_session::{ClientSigner, RemoteSession};
use serde_json::json;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];
const CLIENT_SEED: [u8; 32] = [7u8; 32];

fn ticket(session_id: Option<&str>) -> PairingTicket {
    PairingTicket {
        endpoint_id: [0u8; 32],
        handshake_secret: SECRET,
        session_id: session_id.map(Into::into),
    }
}

/// A registry with one session `sess-1` carrying a text event and an outstanding
/// permission request.
fn seeded_registry() -> Arc<SessionRegistry> {
    let registry = Arc::new(SessionRegistry::new());
    let conn = StubConnection::default()
        .with_capabilities(AgentCapabilities { supports_steer: true, ..Default::default() });
    let handle = registry.register("sess-1".into(), Arc::new(conn));
    handle.ingest(ThreadEvent::AssistantText("hi".into()));
    handle.ingest(ThreadEvent::PermissionRequested {
        request_id: "req-1".into(),
        tool_use_id: None,
        tool_name: "Bash".into(),
        input: json!({ "command": "ls" }),
        description: "run ls".into(),
        suggestions: vec![],
        kind: PermissionKind::Tool,
    });
    registry
}

#[test]
fn client_pairs_and_drives_a_session_over_the_loopback() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    let (client_transport, server) = duplex_pair();
    let client = RemoteSession::new(Arc::new(client_transport), ClientSigner::from_seed(&CLIENT_SEED));

    let serve = dispatcher.serve(&server);
    let script = async move {
        // Pair, then the token is cached for a fast reconnect.
        client.pair(&ticket(None), "phone", NOW).await.expect("pair");
        assert!(client.session_token().is_some(), "reconnect token cached on pair");

        // The seeded session shows up with its real seq + awaiting-permission.
        let sessions = client.list_sessions().await.expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-1");
        assert_eq!(sessions[0].last_seq, 2);
        assert!(sessions[0].awaiting_permission);

        // Gap-fill decodes back into the exact events, `Value` intact.
        let backlog = client.events_since("sess-1", 0).await.expect("events_since");
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog[0].event().unwrap(), ThreadEvent::AssistantText("hi".into()));

        // Commands ack.
        client.send_prompt("sess-1", "go", &[], 1).await.expect("send_prompt");
        client.steer("sess-1", "focus").await.expect("steer");
        client.cancel("sess-1").await.expect("cancel");

        // Resolve is idempotent: first call wins, a re-resolve reports already-decided.
        let allow = PermissionDecision::Allow { updated_input: json!({}) };
        assert!(client.resolve_permission("sess-1", "req-1", &allow).await.expect("resolve"));
        assert!(
            !client.resolve_permission("sess-1", "req-1", &allow).await.expect("re-resolve"),
            "already-decided is Ok(false), not an error"
        );
        // client dropped here → serve loop ends
    };

    block_on(join(serve, script));
}

#[test]
fn client_reconnects_via_token_fast_path_and_challenge() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    // Connection 1: pair, capture the reconnect token, then drop the connection.
    let (c1, s1) = duplex_pair();
    let client1 = RemoteSession::new(Arc::new(c1), ClientSigner::from_seed(&CLIENT_SEED));
    let token = block_on(join(dispatcher.serve(&s1), async move {
        client1.pair(&ticket(None), "phone", NOW).await.expect("pair");
        client1.session_token().expect("token issued")
    }))
    .1;

    // Connection 2: same identity, seeded token → the fast path (no challenge).
    let (c2, s2) = duplex_pair();
    let client2 = RemoteSession::new(Arc::new(c2), ClientSigner::from_seed(&CLIENT_SEED));
    client2.set_session_token(Some(token));
    block_on(join(dispatcher.serve(&s2), async move {
        client2.connect().await.expect("token reconnect");
        assert!(client2.list_sessions().await.is_ok(), "authenticated via token");
    }));

    // Connection 3: same identity, NO token → the Ed25519 challenge path.
    let (c3, s3) = duplex_pair();
    let client3 = RemoteSession::new(Arc::new(c3), ClientSigner::from_seed(&CLIENT_SEED));
    block_on(join(dispatcher.serve(&s3), async move {
        client3.connect().await.expect("challenge reconnect");
        assert!(client3.session_token().is_some(), "challenge issues a fresh token");
        assert!(client3.list_sessions().await.is_ok(), "authenticated via challenge");
    }));
}
