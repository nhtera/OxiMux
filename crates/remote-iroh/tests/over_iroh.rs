//! The over-the-wire proof: the real client [`RemoteSession`] pairs with and
//! drives the real `remote-host` `Dispatcher` across a **real iroh QUIC link
//! between two endpoints on this machine** — the same code the loopback test
//! exercises, now carried by the production transport instead of an in-memory
//! channel. Direct-address hints keep the dial on the local link (no dependence
//! on public relays), while still exercising the genuine iroh connect/stream path.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::future::join;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{AgentCapabilities, PermissionKind, StubConnection, ThreadEvent};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_iroh::{IrohConnector, accept, bind_client, bind_host};
use oximux_remote_proto::PairingTicket;
use oximux_remote_session::{ClientSigner, Connector, RemoteSession};
use serde_json::json;

const SECRET: [u8; 16] = [0x22; 16];
const CLIENT_SEED: [u8; 32] = [7u8; 32];
const NOW: u64 = 1_700_000_000;

/// A fixed host clock so the registration-timestamp window check is deterministic
/// (the client pairs at `NOW`), independent of wall time.
fn clock() -> u64 {
    NOW
}

/// A registry with one session `sess-1` carrying a text event and an outstanding
/// permission request (mirrors the loopback test's fixture).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_pairs_and_drives_a_session_over_real_iroh_quic() {
    // ── Host: bind an iroh endpoint and learn its dialable direct addresses ──
    let host_ep = bind_host(None).await.expect("bind host endpoint");
    host_ep.online().await; // wait until direct addresses are known
    let host_id = *host_ep.id().as_bytes();
    let direct: Vec<SocketAddr> = host_ep.addr().ip_addrs().copied().collect();
    assert!(!direct.is_empty(), "host must have at least one direct address to dial");

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    let dispatcher = Arc::new(Dispatcher::new(seeded_registry(), auth).with_clock(clock));

    // Host accept-and-serve loop, on its own task.
    let host_task = {
        let dispatcher = dispatcher.clone();
        let host_ep = host_ep.clone();
        tokio::spawn(async move {
            let transport = accept(&host_ep).await.expect("accept").expect("one connection");
            dispatcher.serve(&transport).await;
        })
    };

    // ── Client: dial the host over real QUIC via the iroh Connector ──
    let client_ep = bind_client().await.expect("bind client endpoint");
    let connector =
        IrohConnector::new(client_ep, host_id).expect("connector").with_direct_addrs(direct);
    let transport = connector.connect().await.expect("dial host over iroh");

    let client = RemoteSession::new(transport, ClientSigner::from_seed(&CLIENT_SEED));
    let pump = client.take_pump().expect("pump");

    let ticket =
        PairingTicket { endpoint_id: host_id, handshake_secret: SECRET, session_id: None };
    let script = async move {
        // Pair over the real link; a reconnect token is cached on success.
        client.pair(&ticket, "phone", NOW).await.expect("pair over iroh");
        assert!(client.session_token().is_some(), "reconnect token cached on pair");

        // The seeded session round-trips with its real seq + awaiting-permission.
        let sessions = client.list_sessions().await.expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-1");
        assert_eq!(sessions[0].last_seq, 2);
        assert!(sessions[0].awaiting_permission);

        // Backlog decodes back to the exact events, `Value` intact.
        let backlog = client.events_since("sess-1", 0).await.expect("events_since");
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog[0].event().unwrap(), ThreadEvent::AssistantText("hi".into()));

        // A command acks over the wire.
        client.send_prompt("sess-1", "go", &[], 1).await.expect("send_prompt");
        // `client` drops here → its shutdown fires → the pump stops → serve ends.
    };

    let (pump_res, ()) = join(pump.run(), script).await;
    pump_res.expect("pump ran to a clean shutdown");
    host_task.await.expect("host task joined");
}
