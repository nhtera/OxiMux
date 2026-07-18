//! The end-to-end proof: the real `MobileClient` (the phone's Rust core) pairs
//! with and drives the real `remote-host` `Dispatcher` over the in-memory
//! loopback — a test `Connector` stands in for iroh. Everything crosses our
//! actual FFI-facing wrapper: pairing, `list_sessions`, and a folded event
//! subscription pushed into a foreign `EventSink`. No network, no FFI codegen.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oximux_agent_core::thread::{PermissionKind, ThreadEvent};
use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::{AgentCapabilities, StubConnection};
use oximux_mobile_core::{ConnState, ConnStateListener, EventSink, MobileClient, RemoteEvent};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_proto::transport::Transport;
use oximux_remote_proto::{PairingTicket, testing::duplex_pair};
use oximux_remote_session::{ConnectError, Connector};
use serde_json::json;

const SECRET: [u8; 16] = [0x22; 16];
const CLIENT_SEED: [u8; 32] = [9u8; 32];

/// Hands the pre-made client transport to `connect_with` on the first dial.
struct LoopbackConnector {
    transport: Mutex<Option<Arc<dyn Transport>>>,
}
#[async_trait]
impl Connector for LoopbackConnector {
    async fn connect(&self) -> Result<Arc<dyn Transport>, ConnectError> {
        self.transport
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| ConnectError::Unreachable("loopback already dialed".into()))
    }
}

#[derive(Default)]
struct RecordingListener {
    states: Mutex<Vec<ConnState>>,
}
impl ConnStateListener for RecordingListener {
    fn on_state(&self, state: ConnState) {
        self.states.lock().unwrap().push(state);
    }
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<RemoteEvent>>,
}
impl EventSink for RecordingSink {
    fn on_event(&self, event: RemoteEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// A registry with one session carrying a text event + an outstanding permission,
/// returning the handle so the test can push a live event after subscribing.
fn seeded_registry() -> (Arc<SessionRegistry>, Arc<SessionHandle>) {
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
    (registry, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mobile_client_pairs_lists_and_subscribes_over_the_loopback() {
    let (registry, handle) = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    // Real (system) clock on both sides — the client pairs at `now`, so no clock injection.
    let dispatcher = Arc::new(Dispatcher::new(registry, auth));

    let (client_t, server_t) = duplex_pair();
    let server = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t).await })
    };

    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None };
    let client = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    let listener = Arc::new(RecordingListener::default());
    let connector = Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t))) });

    // Pair + connect over the loopback.
    client
        .connect_with(connector, ticket, "phone".into(), listener.clone())
        .await
        .expect("connect_with");
    assert!(
        matches!(listener.states.lock().unwrap().last(), Some(ConnState::Connected)),
        "settles Connected, saw {:?}",
        listener.states.lock().unwrap(),
    );

    // The seeded session round-trips with its real seq + awaiting-permission.
    let sessions = client.list_sessions().await.expect("list_sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "sess-1");
    assert_eq!(sessions[0].last_seq, 2);
    assert!(sessions[0].awaiting_permission);

    // Subscribe: the backlog is folded and pushed into the foreign sink.
    let sink = Arc::new(RecordingSink::default());
    client.subscribe("sess-1".into(), sink.clone()).await.expect("subscribe");
    assert_eq!(sink.events.lock().unwrap().len(), 2, "backlog: text + permission");
    assert!(
        sink.events.lock().unwrap()[0].event_json.contains("hi"),
        "first folded event is the assistant text, saw {:?}",
        sink.events.lock().unwrap()[0].event_json,
    );

    // A live event pushed on the host now streams through the dispatcher to the sink.
    handle.ingest(ThreadEvent::AssistantText(" there".into()));
    let got_live = wait_until(|| sink.events.lock().unwrap().len() >= 3).await;
    assert!(got_live, "live event forwarded, saw {} total", sink.events.lock().unwrap().len());

    client.disconnect().await;
    let _ = server.await;
}

/// Poll a condition for up to ~2s (the pump + dispatcher run on the core runtime).
async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    cond()
}
