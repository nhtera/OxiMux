//! Proves the host accept *loop* ([`serve_host`]) over real iroh QUIC: it serves
//! more than one connection in sequence (a single-`accept` host could only ever
//! serve the first) and stops promptly when its shutdown signal fires. The
//! per-connection correctness (pair/list/backlog/command) is covered by
//! `over_iroh.rs`; here we only assert that a second phone is accepted after the
//! first hangs up, and that `serve_host` returns once asked to stop.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::future::join;
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{StubConnection, ThreadEvent};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_iroh::{IrohConnector, bind_client, bind_host, serve_host, start_host};
use oximux_remote_proto::PairingTicket;
use oximux_remote_session::{ClientSigner, Connector, RemoteSession};

const SECRET: [u8; 16] = [0x22; 16];
const NOW: u64 = 1_700_000_000;

fn clock() -> u64 {
    NOW
}

/// A registry with one session so `list_sessions` returns a stable count.
fn seeded_registry() -> Arc<SessionRegistry> {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.ingest(ThreadEvent::AssistantText("hi".into()));
    registry
}

/// Dial the host over real iroh QUIC as a fresh device, pair, and return how many
/// sessions it sees — then drop the client so its serve task on the host ends.
async fn dial_and_list(host_id: [u8; 32], direct: Vec<SocketAddr>, seed: [u8; 32]) -> usize {
    let client_ep = bind_client().await.expect("bind client");
    let connector =
        IrohConnector::new(client_ep, host_id).expect("connector").with_direct_addrs(direct);
    let transport = connector.connect().await.expect("dial host");

    let client = RemoteSession::new(transport, ClientSigner::from_seed(&seed));
    let pump = client.take_pump().expect("pump");
    let ticket =
        PairingTicket { endpoint_id: host_id, handshake_secret: SECRET, session_id: None };
    let script = async move {
        client.pair(&ticket, "phone", NOW).await.expect("pair");
        client.list_sessions().await.expect("list").len()
        // `client` drops here → shutdown → pump stops → the host's serve ends.
    };
    let (pump_res, count) = join(pump.run(), script).await;
    pump_res.expect("pump ran to a clean shutdown");
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_host_serves_multiple_connections_then_shuts_down() {
    let host_ep = bind_host(None).await.expect("bind host endpoint");
    host_ep.online().await;
    let host_id = *host_ep.id().as_bytes();
    let direct: Vec<SocketAddr> = host_ep.addr().ip_addrs().copied().collect();
    assert!(!direct.is_empty(), "host must have a direct address to dial");

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    let dispatcher = Arc::new(Dispatcher::new(seeded_registry(), auth).with_clock(clock));

    // Run the accept loop on its own task with a controllable shutdown.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let host_task = tokio::spawn(serve_host(host_ep, dispatcher, async move {
        let _ = shutdown_rx.await;
    }));

    // Two phones connect one after the other. The second proves the loop kept
    // accepting after the first connection was fully served and dropped.
    let first = dial_and_list(host_id, direct.clone(), [7u8; 32]).await;
    assert_eq!(first, 1, "first phone sees the seeded session");
    let second = dial_and_list(host_id, direct.clone(), [9u8; 32]).await;
    assert_eq!(second, 1, "second phone is accepted and served after the first hung up");

    // Asking the loop to stop makes it return (and close the endpoint).
    shutdown_tx.send(()).expect("signal shutdown");
    host_task.await.expect("serve_host returns on shutdown");
}

/// The endpoint id must be pinned by the supplied secret, not regenerated per
/// bind. A paired phone dials the host *by* that id, so a rotating identity would
/// silently invalidate every stored pairing on each restart — the durable device
/// table would survive while the address it points at did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fixed_endpoint_secret_keeps_the_host_identity_across_binds() {
    let secret = [0x5a; 32];

    let first = bind_host(Some(secret)).await.expect("first bind");
    let first_id = *first.id().as_bytes();
    first.close().await;

    let second = bind_host(Some(secret)).await.expect("second bind");
    let second_id = *second.id().as_bytes();
    second.close().await;

    assert_eq!(first_id, second_id, "the same secret yields the same endpoint id");

    // A different secret is a different host…
    let other = bind_host(Some([0xa5; 32])).await.expect("other bind");
    let other_id = *other.id().as_bytes();
    other.close().await;
    assert_ne!(first_id, other_id);

    // …and no secret is the old throwaway-identity behavior.
    let ephemeral = bind_host(None).await.expect("ephemeral bind");
    let ephemeral_id = *ephemeral.id().as_bytes();
    ephemeral.close().await;
    assert_ne!(ephemeral_id, first_id, "an unseeded bind is a fresh identity");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_host_yields_a_ticket_and_stops_cleanly() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(Dispatcher::new(seeded_registry(), auth).with_clock(clock));

    // `start_host` binds lazily (only here, never at boot), waits until dialable,
    // and hands back the ticket a phone would scan.
    let handle = start_host(dispatcher, SECRET, None).await.expect("start host");
    assert_eq!(handle.ticket().handshake_secret, SECRET, "ticket carries the shared secret");
    assert_ne!(handle.ticket().endpoint_id, [0u8; 32], "ticket carries a real endpoint key");
    assert!(handle.ticket().session_id.is_none(), "a host ticket pins no single session");

    // Stopping returns promptly, proving the accept loop honors shutdown.
    handle.join().await;
}
