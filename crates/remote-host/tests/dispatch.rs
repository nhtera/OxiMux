//! End-to-end dispatcher tests over the in-memory loopback transport — a full
//! pair → command → revoke conversation, plus the Ed25519 reconnect handshake,
//! with no network.

use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use futures::executor::block_on;
use futures::future::join;
use oximux_agent_core::thread::{PermissionDecision, PermissionKind, ThreadEvent};
use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::{AgentCapabilities, StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::messages::{ConnectReq, HelloReq, RegisterReq, SendPromptReq};
use oximux_remote_proto::proto::{
    MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Request, Response, RpcError,
};
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_proto::{AuthProveReq, ResolvePermissionReq, Transport};
use serde_json::json;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];

/// One request → one response over a client transport.
async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    read_response(client).await
}

/// Read the next response frame with no preceding request — for the unsolicited
/// live `Response::Event` frames a subscription pushes.
async fn read_response(client: &dyn Transport) -> Response {
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

/// A registry with one session `sess-1` that has already streamed a text event
/// and an outstanding permission request.
fn seeded_registry() -> Arc<SessionRegistry> {
    let registry = Arc::new(SessionRegistry::new());
    // A steer-capable stub so the Steer RPC exercises the ack path (a default
    // stub refuses mid-turn steer, which would instead exercise error mapping).
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
fn full_pairing_and_session_control_over_the_loopback() {
    let registry = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    let dispatcher = Dispatcher::new(registry, auth.clone()).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        // Unauthenticated: a session RPC is refused before pairing.
        assert_eq!(
            call(&client, Request::ListSessions).await,
            Response::Error(RpcError::Unauthorized),
            "no session access before auth"
        );

        // Pair.
        let Response::Registered { .. } = call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // List reflects the seeded session's real seq + awaiting-permission.
        let Response::Sessions(sessions) = call(&client, Request::ListSessions).await else {
            panic!("expected Sessions");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-1");
        assert_eq!(sessions[0].last_seq, 2);
        assert!(sessions[0].awaiting_permission);

        // Backlog replay decodes back into the exact events, Value intact.
        let Response::Events(frames) = call(
            &client,
            Request::EventsSince { session_id: "sess-1".into(), after_seq: 0 },
        )
        .await
        else {
            panic!("expected Events");
        };
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event().unwrap(), ThreadEvent::AssistantText("hi".into()));
        assert!(matches!(frames[1].event().unwrap(), ThreadEvent::PermissionRequested { .. }));

        // Commands ack.
        assert_eq!(
            call(
                &client,
                Request::SendPrompt(SendPromptReq {
                    session_id: "sess-1".into(),
                    text: "go".into(),
                    images: vec![],
                    corr_id: 1,
                })
            )
            .await,
            Response::Ack
        );
        assert_eq!(
            call(&client, Request::Steer { session_id: "sess-1".into(), text: "focus".into() }).await,
            Response::Ack
        );
        assert_eq!(
            call(&client, Request::Cancel { session_id: "sess-1".into() }).await,
            Response::Ack
        );

        // Resolve is idempotent: first wins, a re-resolve reports AlreadyDecided.
        let resolve = Request::ResolvePermission(
            ResolvePermissionReq::new(
                "sess-1",
                "req-1",
                &PermissionDecision::Allow { updated_input: json!({}) },
            )
            .unwrap(),
        );
        assert_eq!(call(&client, resolve.clone()).await, Response::Ack);
        assert_eq!(
            call(&client, resolve).await,
            Response::Error(RpcError::AlreadyDecided)
        );

        // Unknown session is distinguished from unauthorized.
        assert_eq!(
            call(&client, Request::GetSessionInfo { session_id: "nope".into() }).await,
            Response::Error(RpcError::UnknownSession)
        );
        let Response::SessionInfo(_) =
            call(&client, Request::GetSessionInfo { session_id: "sess-1".into() }).await
        else {
            panic!("expected SessionInfo");
        };

        // Revoke mid-connection: Ping still works, but session RPCs now fail the
        // per-RPC recheck even though the connection stayed open and authenticated.
        auth.revoke(&pubkey);
        assert_eq!(call(&client, Request::Ping).await, Response::Pong);
        assert_eq!(
            call(&client, Request::ListSessions).await,
            Response::Error(RpcError::Unauthorized),
            "revocation bites an already-open connection"
        );
        // client dropped here → serve loop ends
    };

    block_on(join(serve, script));
}

#[test]
fn subscribe_streams_live_events_and_revocation_silences_the_stream() {
    let registry = seeded_registry();
    // A handle clone the script can ingest into after subscribing, to drive the
    // live edge (the registry itself is moved into the dispatcher).
    let handle = registry.get("sess-1").expect("seeded session");
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false)); // static full-access ticket
    let dispatcher = Dispatcher::new(registry, auth.clone()).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // Subscribe replies with the backlog (seq 1, 2) as `Events` first.
        let Response::Events(backlog) =
            call(&client, Request::Subscribe { session_id: "sess-1".into(), after_seq: Some(0) }).await
        else {
            panic!("expected the backlog Events reply");
        };
        let seqs: Vec<u64> = backlog.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![1, 2], "backlog replayed before the live edge");

        // A new event ingested now must arrive as a pushed live `Event`, not a
        // re-sent backlog entry.
        handle.ingest(ThreadEvent::AssistantText("live!".into()));
        let Response::Event(frame) = read_response(&client).await else {
            panic!("expected a pushed live Event");
        };
        assert_eq!(frame.seq, 3, "live seq follows the backlog");
        assert_eq!(frame.event().unwrap(), ThreadEvent::AssistantText("live!".into()));
        assert_eq!(frame.status.last_seq, 3, "status snapshot rides the live frame");

        // Revoke mid-stream: the next ingest must be suppressed. Proven by a Ping
        // whose Pong is the very next frame the client reads — had seq 4 been
        // forwarded, it would arrive first.
        auth.revoke(&pubkey);
        handle.ingest(ThreadEvent::AssistantText("after-revoke".into()));
        assert_eq!(
            call(&client, Request::Ping).await,
            Response::Pong,
            "no live frame leaks past revocation ahead of the Pong"
        );
        // client dropped here → serve loop ends
    };

    block_on(join(serve, script));
}

#[test]
fn repeat_subscribe_serves_backlog_without_a_second_live_stream() {
    let registry = seeded_registry();
    let handle = registry.get("sess-1").expect("seeded session");
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // First subscribe opens the live stream + replays the backlog.
        let sub = Request::Subscribe { session_id: "sess-1".into(), after_seq: Some(0) };
        assert!(matches!(call(&client, sub.clone()).await, Response::Events(_)));
        // Re-subscribing the same session on the same connection is idempotent: it
        // serves the backlog again but must NOT open a second live stream.
        assert!(matches!(call(&client, sub).await, Response::Events(_)));

        // One ingest must yield exactly ONE live frame (not one per subscribe).
        handle.ingest(ThreadEvent::AssistantText("once".into()));
        let Response::Event(frame) = read_response(&client).await else {
            panic!("expected a single live Event");
        };
        assert_eq!(frame.seq, 3);

        // Had a second stream been registered, a duplicate seq-3 Event would sit
        // ahead of the Pong; the Pong being next proves there was only one.
        assert_eq!(call(&client, Request::Ping).await, Response::Pong, "no duplicate live frame");
    };
    block_on(join(serve, script));
}

/// The desktop publishes a title/model per session; both must reach the client's
/// list + detail views, and an untitled session must still render as something.
#[test]
fn session_meta_published_by_the_desktop_reaches_the_client() {
    let registry = Arc::new(SessionRegistry::new());
    let titled = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    titled.set_meta(SessionMeta {
        title: Some("Fix auth".into()),
        model: Some("claude-opus-4-8".into()),
        cwd: None,
    });
    // Registered but never titled — the fallback path.
    registry.register("sess-2".into(), Arc::new(StubConnection::default()));

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        let Response::Sessions(sessions) = call(&client, Request::ListSessions).await else {
            panic!("expected Sessions");
        };
        let titled = sessions.iter().find(|s| s.session_id == "sess-1").expect("sess-1 listed");
        assert_eq!(titled.title, "Fix auth", "the desktop's title, not the raw id");
        assert_eq!(titled.model.as_deref(), Some("claude-opus-4-8"));

        let untitled = sessions.iter().find(|s| s.session_id == "sess-2").expect("sess-2 listed");
        assert_eq!(untitled.title, "sess-2", "an untitled session falls back to its id");
        assert_eq!(untitled.model, None);

        // The same meta rides the detail view.
        let Response::SessionInfo(info) =
            call(&client, Request::GetSessionInfo { session_id: "sess-1".into() }).await
        else {
            panic!("expected SessionInfo");
        };
        assert_eq!(info.summary.title, "Fix auth");
        assert_eq!(info.summary.model.as_deref(), Some("claude-opus-4-8"));
    };
    block_on(join(serve, script));
}

/// Git access rides the same session ACL as every other RPC (no second, wider
/// authorization surface), and a session that never published a working directory
/// is refused rather than the host guessing at a repository.
#[test]
fn git_status_is_acl_gated_and_requires_a_working_directory() {
    let registry = Arc::new(SessionRegistry::new());
    // Registered, but no cwd ever published by a desktop view.
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        // Unauthenticated: git is refused like any other session RPC.
        assert_eq!(
            call(&client, Request::GitStatus { session_id: "sess-1".into() }).await,
            Response::Error(RpcError::Unauthorized),
            "no git access before pairing",
        );

        let Response::Registered { .. } = call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // An unknown session stays distinguishable from an unauthorized one.
        assert_eq!(
            call(&client, Request::GitStatus { session_id: "nope".into() }).await,
            Response::Error(RpcError::UnknownSession),
        );

        // Known session, but no cwd → refused before any repository is opened.
        assert!(
            matches!(
                call(&client, Request::GitStatus { session_id: "sess-1".into() }).await,
                Response::Error(RpcError::BadRequest(_)),
            ),
            "a session with no working directory cannot resolve a repo",
        );
    };
    block_on(join(serve, script));
}

/// The read-only tier over the wire: the device keeps reading its sessions but
/// every state-changing RPC is refused, and the downgrade bites an ALREADY-OPEN
/// connection (the dispatcher rechecks per call, like revocation).
#[test]
fn read_only_device_is_refused_writes_mid_connection() {
    let registry = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth.clone()).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // Read-write to start: a prompt is accepted.
        assert_eq!(
            call(
                &client,
                Request::SendPrompt(SendPromptReq {
                    session_id: "sess-1".into(),
                    text: "go".into(),
                    images: vec![],
                    corr_id: 1,
                })
            )
            .await,
            Response::Ack,
        );

        // Downgrade the live device.
        auth.set_read_only(&pubkey, true);

        // Reads keep working…
        assert!(matches!(call(&client, Request::ListSessions).await, Response::Sessions(_)));
        assert!(matches!(
            call(&client, Request::GetSessionInfo { session_id: "sess-1".into() }).await,
            Response::SessionInfo(_),
        ));

        // …while every write is refused on the same open connection.
        for write in [
            Request::SendPrompt(SendPromptReq {
                session_id: "sess-1".into(),
                text: "again".into(),
                images: vec![],
                corr_id: 2,
            }),
            Request::Steer { session_id: "sess-1".into(), text: "focus".into() },
            Request::Cancel { session_id: "sess-1".into() },
        ] {
            assert_eq!(
                call(&client, write).await,
                Response::Error(RpcError::Unauthorized),
                "a read-only device must not drive the agent",
            );
        }
    };
    block_on(join(serve, script));
}

#[test]
fn list_sessions_respects_device_scope() {
    let registry = Arc::new(SessionRegistry::new());
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    registry.register("sess-2".into(), Arc::new(StubConnection::default()));
    let auth = Arc::new(AuthStore::new());
    // A session-bound ticket → the device is scoped to sess-1 only.
    auth.set_pairing(PairingSlot::new(SECRET, Some("sess-1".into()), false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let reg = RegisterReq {
            app_pubkey: pubkey,
            device_name: "phone".into(),
            proof: registration_proof(&SECRET, &pubkey, NOW),
            timestamp_secs: NOW,
            session_id: Some("sess-1".into()),
        };
        let Response::Registered { .. } = call(&client, Request::Register(reg)).await else {
            panic!("expected Registered");
        };

        // The list is filtered to the device's scope — sess-2 must not leak.
        let Response::Sessions(sessions) = call(&client, Request::ListSessions).await else {
            panic!("expected Sessions");
        };
        let ids: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["sess-1"], "a session-scoped device never enumerates other sessions");

        // And it cannot reach sess-2 directly.
        assert_eq!(
            call(&client, Request::GetSessionInfo { session_id: "sess-2".into() }).await,
            Response::Error(RpcError::Unauthorized)
        );
    };
    block_on(join(serve, script));
}

#[test]
fn reconnect_via_challenge_and_token() {
    let registry = seeded_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));

    // Pre-authorize a real Ed25519 client key (as if it had already paired).
    let client_key = SigningKey::from_bytes(&[7u8; 32]);
    let client_pub = client_key.verifying_key().to_bytes();
    auth.register(&register_req(client_pub), NOW).expect("pre-authorize");

    let dispatcher = Dispatcher::new(registry, auth.clone()).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        // No token → challenge.
        let Response::Challenge { nonce } = call(
            &client,
            Request::Connect(ConnectReq { app_pubkey: client_pub, session_token: None }),
        )
        .await
        else {
            panic!("expected Challenge");
        };

        // Sign the nonce with the app key → Connected + a token.
        let signature = client_key.sign(&nonce).to_bytes().to_vec();
        let Response::Connected { session_token } =
            call(&client, Request::AuthProve(AuthProveReq { signature })).await
        else {
            panic!("expected Connected");
        };
        // Authenticated now.
        assert!(matches!(call(&client, Request::ListSessions).await, Response::Sessions(_)));

        // The issued token is a valid fast-path reconnect credential.
        let Response::Connected { .. } = call(
            &client,
            Request::Connect(ConnectReq { app_pubkey: client_pub, session_token: Some(session_token) }),
        )
        .await
        else {
            panic!("expected Connected via token");
        };

        // A wrong signature is rejected.
        let Response::Challenge { .. } = call(
            &client,
            Request::Connect(ConnectReq { app_pubkey: client_pub, session_token: None }),
        )
        .await
        else {
            panic!("expected Challenge");
        };
        assert_eq!(
            call(&client, Request::AuthProve(AuthProveReq { signature: vec![0u8; 64] })).await,
            Response::Error(RpcError::Unauthorized),
            "a bad signature does not authenticate"
        );
    };

    block_on(join(serve, script));
}

/// A real Ed25519 public key. Arbitrary byte arrays will not do — `register`
/// runs `VerifyingKey::from_bytes`, which rejects anything that is not a valid
/// curve point with `Unauthorized`, indistinguishable from a bad proof.
fn real_pubkey(seed: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes()
}

/// The version handshake answers with the host's range, and — critically — an
/// **older** client is still served. An equality check here would mean every
/// appended RPC on the desktop disconnected every already-paired phone.
#[test]
fn hello_reports_the_host_range_and_serves_an_older_client() {
    let (client, server) = duplex_pair();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    let script = async {
        // A client one version behind the host.
        let older = PROTOCOL_VERSION - 1;
        let ack = call(&client, Request::Hello(HelloReq { protocol_version: older })).await;
        match ack {
            Response::HelloAck(ack) => {
                assert_eq!(ack.protocol_version, PROTOCOL_VERSION);
                assert_eq!(ack.min_compatible, MIN_COMPATIBLE_VERSION);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
        // …and it can still complete a real pairing afterwards.
        let pubkey = real_pubkey(41);
        match call(&client, Request::Register(register_req(pubkey))).await {
            Response::Registered { .. } => {}
            other => panic!("an older-but-compatible client must still pair, got {other:?}"),
        }
        drop(client);
    };
    block_on(join(dispatcher.serve(&server), script));
}

/// A client that never sends `Hello` is read as v1 and served normally — the
/// handshake must not become the thing that breaks pre-handshake clients.
#[test]
fn a_client_that_never_says_hello_is_still_served() {
    let (client, server) = duplex_pair();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    let script = async {
        let pubkey = real_pubkey(42);
        match call(&client, Request::Register(register_req(pubkey))).await {
            Response::Registered { .. } => {}
            other => panic!("a silent (v1) client must still pair, got {other:?}"),
        }
        drop(client);
    };
    block_on(join(dispatcher.serve(&server), script));
}

/// The refusal path carries both host numbers, so the client can tell the user
/// which side is behind instead of surfacing a bare connection failure. Driven
/// through a version below the floor rather than by mutating the constant.
#[test]
fn a_client_below_the_floor_is_refused_before_offering_a_credential() {
    let (client, server) = duplex_pair();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(seeded_registry(), auth).with_clock(clock);

    let script = async {
        // Version 0 is below `MIN_COMPATIBLE_VERSION` (1) for every build.
        let resp = call(&client, Request::Hello(HelloReq { protocol_version: 0 })).await;
        match resp {
            Response::Error(RpcError::IncompatibleVersion {
                host_version,
                host_min_compatible,
            }) => {
                assert_eq!(host_version, PROTOCOL_VERSION);
                assert_eq!(
                    host_min_compatible,
                    MIN_COMPATIBLE_VERSION
                );
            }
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
        // And the refusal sticks: a subsequent pairing attempt on the same
        // connection is refused too, so a rejected peer cannot simply carry on.
        let pubkey = real_pubkey(43);
        match call(&client, Request::Register(register_req(pubkey))).await {
            Response::Error(RpcError::IncompatibleVersion { .. }) => {}
            other => panic!("a refused client must stay refused, got {other:?}"),
        }
        drop(client);
    };
    block_on(join(dispatcher.serve(&server), script));
}
