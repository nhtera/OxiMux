//! The end-to-end proof: the real `MobileClient` (the phone's Rust core) pairs
//! with and drives the real `remote-host` `Dispatcher` over the in-memory
//! loopback — a test `Connector` stands in for iroh. Everything crosses our
//! actual FFI-facing wrapper: pairing, `list_sessions`, and a folded transcript
//! pushed into a foreign `ThreadSink`. No network, no FFI codegen.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::select;
use oximux_agent_core::thread::{PermissionKind, ThreadEvent};
use oximux_agents::session_registry::{SessionHandle, SessionRegistry};
use oximux_agents::thread::{AgentCapabilities, StubConnection};
use oximux_mobile_core::{
    ChatImage, ConnState, ConnStateListener, MobileClient, ThreadSink, ThreadSnapshot,
};
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

/// Hands out pre-made client transports in order — one per (re)dial — so the
/// driver can reconnect after a link drop.
struct QueueConnector {
    queue: Mutex<VecDeque<Arc<dyn Transport>>>,
}
#[async_trait]
impl Connector for QueueConnector {
    async fn connect(&self) -> Result<Arc<dyn Transport>, ConnectError> {
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ConnectError::Unreachable("no more loopback transports".into()))
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
    snapshots: Mutex<Vec<ThreadSnapshot>>,
}
impl ThreadSink for RecordingSink {
    fn on_thread(&self, snapshot: ThreadSnapshot) {
        self.snapshots.lock().unwrap().push(snapshot);
    }
}

impl RecordingSink {
    fn count(&self) -> usize {
        self.snapshots.lock().unwrap().len()
    }

    /// The most recent transcript pushed — what the app would be rendering.
    fn latest_json(&self) -> String {
        self.snapshots.lock().unwrap().last().expect("a snapshot was pushed").thread_json.clone()
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

    // Subscribe: the whole backlog folds into ONE snapshot — the cold-open
    // bootstrap. Two events, one push: the app renders a transcript, not a
    // replay.
    let sink = Arc::new(RecordingSink::default());
    client.subscribe("sess-1".into(), sink.clone()).await.expect("subscribe");
    assert_eq!(sink.count(), 1, "the backlog folds into a single snapshot");
    let bootstrap = sink.snapshots.lock().unwrap()[0].clone();
    assert!(bootstrap.thread_json.contains("hi"), "carries the folded assistant text");
    assert!(
        bootstrap.awaiting_permission,
        "the seeded permission blocks the session, so the snapshot says so",
    );
    assert!(
        bootstrap.thread_json.contains("req-1"),
        "the pending request rides the transcript, so the card can answer it: {}",
        bootstrap.thread_json,
    );

    // A live event pushed on the host streams through the dispatcher and folds
    // onto the SAME thread — the text accumulates rather than arriving loose.
    handle.ingest(ThreadEvent::AssistantText(" there".into()));
    let got_live = wait_until(|| sink.count() >= 2).await;
    assert!(got_live, "live event pushed a fresh snapshot, saw {} total", sink.count());
    let latest: serde_json::Value =
        serde_json::from_str(&sink.latest_json()).expect("snapshot is valid json");
    let text = latest["entries"][0]["Assistant"]["text"].as_str().expect("assistant text");
    assert!(
        text.contains("hi") && text.contains("there"),
        "the live text folded into the SAME assistant entry rather than a loose second one, \
         saw {text:?}",
    );

    // Answering crosses the FFI as JSON and comes back idempotent, same contract
    // as resolving a permission.
    let questions = r#"[{"id":"q1","header":"Pick","question":"Which one?",
        "options":[{"label":"A","description":"first"}],
        "kind":"SingleSelect","other_allowed":false,"is_secret":false}]"#;
    let answers = r#"{"by_question":{"q1":{"selected":["A"],"custom":null}},"response":null}"#;
    assert!(
        client
            .answer_question("sess-1".into(), "req-q".into(), questions.into(), answers.into())
            .await
            .expect("answer_question"),
        "the first answer decides the request",
    );
    assert!(
        !client
            .answer_question("sess-1".into(), "req-q".into(), questions.into(), answers.into())
            .await
            .expect("re-answer"),
        "a second answer is already-decided, not an error",
    );

    // A secret question is refused before anything leaves the phone: the desktop
    // redacts a secret answer by flagging its own thread as the answer is sent,
    // and this path has no thread to flag, so a remote answer would land in the
    // persisted transcript in plain text.
    let secret = questions.replace("\"is_secret\":false", "\"is_secret\":true");
    let err = client
        .answer_question("sess-1".into(), "req-s".into(), secret, answers.into())
        .await
        .expect_err("a secret question is refused");
    assert!(
        format!("{err:?}").contains("desktop"),
        "the refusal says where it CAN be answered, saw {err:?}",
    );

    client.disconnect().await;
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mobile_client_self_heals_and_restores_subscriptions_after_a_drop() {
    let (registry, handle) = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(Dispatcher::new(registry, auth));

    // Two loopback connections: the client pairs over the first, then reconnects
    // (token fast-path) over the second after the first is severed.
    let (client1, server1) = duplex_pair();
    let (client2, server2) = duplex_pair();
    let (drop1_tx, drop1_rx) = oneshot::channel::<()>();

    let host = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            // Serve conn 1 until told to sever it (simulating a network drop).
            select(Box::pin(dispatcher.serve(&server1)), drop1_rx).await;
            drop(server1); // link drops → the client's pump ends → the driver reconnects
            // Serve conn 2 (the reconnect) until the client disconnects.
            dispatcher.serve(&server2).await;
        })
    };

    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None };
    let client = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    let listener = Arc::new(RecordingListener::default());
    let connector = Arc::new(QueueConnector {
        queue: Mutex::new(VecDeque::from([
            Arc::new(client1) as Arc<dyn Transport>,
            Arc::new(client2) as Arc<dyn Transport>,
        ])),
    });

    client
        .connect_with(connector, ticket, "phone".into(), listener.clone())
        .await
        .expect("connect_with");

    // Subscribe over conn 1: the backlog bootstraps as one snapshot.
    let sink = Arc::new(RecordingSink::default());
    client.subscribe("sess-1".into(), sink.clone()).await.expect("subscribe");
    assert_eq!(sink.count(), 1, "conn-1 backlog bootstrap");

    // Sever conn 1 → the driver must reconnect over conn 2 on its own and restore
    // the subscription — resuming from the fold cursor, so no backlog re-flood.
    drop1_tx.send(()).expect("signal drop");
    let reconnected = wait_until(|| connected_count(&listener) >= 2).await;
    assert!(reconnected, "self-healed, saw {:?}", listener.states.lock().unwrap());
    assert_eq!(
        sink.count(),
        1,
        "reconnect resumed from the cursor — the backlog was not re-folded, so no new push",
    );

    // The connection is live again: an RPC round-trips over the fresh link...
    let sessions = client.list_sessions().await.expect("list after reconnect");
    assert_eq!(sessions.len(), 1, "session list served over the reconnected link");

    // ...and a NEW event pushed now streams over conn 2, folded onto the preserved
    // cursor (seq 3), landing exactly once.
    handle.ingest(ThreadEvent::AssistantText(" back".into()));
    let got_live = wait_until(|| sink.count() == 2).await;
    assert!(got_live, "live event resumed after reconnect, saw {} pushes", sink.count());
    assert!(
        sink.latest_json().contains("back"),
        "the resumed live event folded into the transcript, saw {}",
        sink.latest_json(),
    );

    client.disconnect().await;
    let _ = host.await;
}

/// How many times the listener has seen [`ConnState::Connected`] — the pair plus
/// each self-healed reconnect.
fn connected_count(listener: &RecordingListener) -> usize {
    listener.states.lock().unwrap().iter().filter(|s| matches!(s, ConnState::Connected)).count()
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

/// A relaunched app resumes its pairing without a ticket.
///
/// This is the path every app launch takes after the first, and it is the reason
/// the ticket must NOT be persisted: its `handshake_secret` is one-time, so a
/// stored ticket would be stale. The device is recognised instead by its Ed25519
/// identity — modelled here by building the second client from the same seed,
/// exactly as the app rebuilds it from the OS keystore.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relaunched_client_resumes_the_pairing_without_a_ticket() {
    let (registry, _handle) = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    // One dispatcher, so the AuthStore carries the pairing across both connections
    // just as the desktop's durable device record does across an app restart.
    let dispatcher = Arc::new(Dispatcher::new(registry, auth));

    // First launch: pair over the first loopback.
    let (client_t, server_t) = duplex_pair();
    let serve_1 = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t).await })
    };
    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None };
    let first = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    first
        .connect_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t))) }),
            ticket,
            "phone".into(),
            Arc::new(RecordingListener::default()),
        )
        .await
        .expect("initial pairing");
    // The host id is what the app persists; the ticket is deliberately not stored.
    assert_eq!(first.host_endpoint_id(), Some(vec![0u8; 32]), "exposes the host id to persist");
    first.disconnect().await;
    serve_1.abort();

    // Relaunch: a brand-new client from the same stored seed, resuming with no ticket.
    let (client_t2, server_t2) = duplex_pair();
    let serve_2 = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t2).await })
    };
    let relaunched = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    let listener = Arc::new(RecordingListener::default());
    relaunched
        .resume_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t2))) }),
            listener.clone(),
        )
        .await
        .expect("resume without a ticket");
    assert!(
        matches!(listener.states.lock().unwrap().last(), Some(ConnState::Connected)),
        "resumes to Connected, saw {:?}",
        listener.states.lock().unwrap(),
    );

    // And the resumed session is usable, not merely connected.
    let sessions = relaunched.list_sessions().await.expect("list_sessions after resume");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, "sess-1");
    serve_2.abort();
}

/// The recovery path a phone takes when it forgot its host but the desktop still
/// knows it: `Register` is refused (a known key must reconnect, not re-pair), the
/// app falls back to a resume on the *same* client, and that resume has to leave a
/// usable session behind.
///
/// Covers the sequence end to end: a refused re-pair must not leave the client in
/// a state where the following resume reports success but serves nothing. Asserts
/// the session is *usable*, not merely that the resume resolved — "connected but
/// every RPC returns `NotConnected`" is the failure worth catching here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_after_a_refused_re_pair_leaves_a_usable_session() {
    let (registry, _handle) = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(Dispatcher::new(registry, auth.clone()));
    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None };

    // Pair once, then drop the client's side of the pairing the way the phone's
    // "forget this desktop" does — the desktop's record survives.
    let (client_t, server_t) = duplex_pair();
    let serve_1 = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t).await })
    };
    let client = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    client
        .connect_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t))) }),
            ticket.clone(),
            "phone".into(),
            Arc::new(RecordingListener::default()),
        )
        .await
        .expect("initial pairing");
    client.disconnect().await;
    serve_1.abort();

    // A fresh code is offered (remote toggled off and on), but this key is already
    // known, so pairing again must fail.
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let (client_t2, server_t2) = duplex_pair();
    let serve_2 = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t2).await })
    };
    client
        .connect_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t2))) }),
            ticket,
            "phone".into(),
            Arc::new(RecordingListener::default()),
        )
        .await
        .expect_err("a device the host already knows cannot re-register");
    serve_2.abort();

    // The fallback: resume on the same client, by identity.
    let (client_t3, server_t3) = duplex_pair();
    let serve_3 = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t3).await })
    };
    let listener = Arc::new(RecordingListener::default());
    client
        .resume_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t3))) }),
            listener.clone(),
        )
        .await
        .expect("resume recovers the pairing");

    // The assertion that matters: connected *and* usable. Reporting success while
    // the session is gone is the failure this guards.
    let sessions = client.list_sessions().await.expect("the resumed session must serve RPCs");
    assert_eq!(sessions.len(), 1, "sees the host's sessions after recovery");
    serve_3.abort();
}

/// An attached image has to survive the whole chain — FFI record, postcard
/// envelope, host handler, registry — and land in the folded transcript the app
/// renders.
///
/// The fold is where it has to arrive, not the backend: `AgentConnection`'s
/// default `send_user_message_with_images` drops images on the floor for a
/// backend that cannot take them, so proving the stub saw it would prove nothing
/// about what the user sees. The user's own bubble is published by the registry
/// regardless, which is what makes the attachment visible remotely at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attached_image_reaches_the_folded_transcript() {
    let (registry, _handle) = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(Dispatcher::new(registry, auth));

    let (client_t, server_t) = duplex_pair();
    let serve = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t).await })
    };

    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None };
    let client = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    client
        .connect_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t))) }),
            ticket,
            "phone".into(),
            Arc::new(RecordingListener::default()),
        )
        .await
        .expect("connect");

    let sink = Arc::new(RecordingSink::default());
    client.subscribe("sess-1".into(), sink.clone()).await.expect("subscribe");
    let before = sink.count();

    client
        .send_prompt("sess-1".into(), "what is this?".into(), vec![ChatImage {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        }])
        .await
        .expect("send a prompt carrying an image");

    assert!(wait_until(|| sink.count() > before).await, "the prompt pushed a snapshot");
    let json = sink.latest_json();
    assert!(json.contains("what is this?"), "the prompt text is in the transcript: {json}");
    assert!(json.contains("aGVsbG8="), "and so is the image payload: {json}");
    assert!(json.contains("image/png"), "with its media type: {json}");

    serve.abort();
}

/// A prompt too large for one frame is refused here, with a message naming the
/// cause. The transport would otherwise reject the frame as a bare IO error, some
/// distance from the photo that caused it — and a camera roll makes that
/// reachable without doing anything unusual.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversize_attachment_is_refused_with_a_useful_message() {
    let (registry, _handle) = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Arc::new(Dispatcher::new(registry, auth));

    let (client_t, server_t) = duplex_pair();
    let serve = {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move { dispatcher.serve(&server_t).await })
    };

    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None };
    let client = MobileClient::new(Some(CLIENT_SEED.to_vec()));
    client
        .connect_with(
            Arc::new(LoopbackConnector { transport: Mutex::new(Some(Arc::new(client_t))) }),
            ticket,
            "phone".into(),
            Arc::new(RecordingListener::default()),
        )
        .await
        .expect("connect");

    let huge = "A".repeat(oximux_remote_iroh::MAX_FRAME);
    let err = client
        .send_prompt("sess-1".into(), "look".into(), vec![ChatImage {
            media_type: "image/png".into(),
            data: huge,
        }])
        .await
        .expect_err("an attachment over the frame cap must not be sent");

    let msg = err.to_string();
    assert!(msg.contains("MB"), "the message is in the units a user thinks in: {msg}");
    assert!(
        msg.contains("fewer or smaller"),
        "and says what to do about it rather than just failing: {msg}",
    );

    serve.abort();
}
