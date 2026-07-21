//! The session-control RPCs (v7): listing a backend's model/mode catalog, and
//! switching between them.

use std::sync::Arc;

use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::{ModeChoice, ModelChoice, StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_proto::Transport;

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

fn model(wire: &str, label: &str) -> ModelChoice {
    ModelChoice { wire: wire.into(), label: label.into(), description: None }
}

/// A switchable (ACP-like) backend: offers a catalog and accepts changes live.
fn switchable_registry() -> (Arc<SessionRegistry>, Arc<StubConnection>) {
    let conn = Arc::new(StubConnection::default().with_switchable(
        vec![model("opus-4.8", "Opus 4.8"), model("sonnet-5", "Sonnet 5")],
        vec![ModeChoice { wire: "plan".into(), label: "Plan".into() }],
    ));
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), conn.clone());
    handle.set_meta(SessionMeta {
        title: None,
        model: Some("opus-4.8".into()),
        cwd: None,
    });
    (registry, conn)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_lists_the_catalog_and_switches_model() {
    let (registry, conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let choices = call(&client, Request::ListChoices { session_id: "sess-1".into() }).await;
        let Response::Choices(choices) = choices else {
            panic!("expected Choices, got {choices:?}");
        };
        assert_eq!(
            choices.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["opus-4.8", "sonnet-5"],
        );
        assert_eq!(choices.models[0].label, "Opus 4.8", "the label is what a person reads");
        assert_eq!(choices.modes.len(), 1);
        assert_eq!(
            choices.current_model.as_deref(),
            Some("opus-4.8"),
            "the picker marks the active model without a second round trip",
        );

        let set = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        assert_eq!(set, Response::Ack);

        drop(client);
    };
    futures::future::join(serve, script).await;

    // Asserted at the backend, not on the wire: a reply that acknowledged while
    // dropping the change would look identical from the client's side.
    let sent = conn.sent();
    assert!(
        sent.iter().any(|v| v["type"] == "set_model" && v["model"] == "sonnet-5"),
        "the switch reached the backend, saw {sent:?}",
    );
}

/// A backend that fixes its model at spawn (Claude, Codex) must fail loudly.
///
/// The desktop recovers by respawning the child, which is a view-level operation
/// the registry cannot perform. Rather than pretend, the host says so — a picker
/// that silently did nothing would be worse than one that explains itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fix_at_spawn_backend_refuses_with_an_explanation() {
    let registry = Arc::new(SessionRegistry::new());
    // A default stub is deliberately NOT switchable — it matches the trait
    // default and the real fix-at-spawn backends.
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        // An empty catalog is a legitimate answer, not an error.
        let choices = call(&client, Request::ListChoices { session_id: "sess-1".into() }).await;
        let Response::Choices(choices) = choices else {
            panic!("expected Choices, got {choices:?}");
        };
        assert!(choices.models.is_empty(), "no catalog is an answer, not a failure");

        let set = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        match set {
            Response::Error(RpcError::BadRequest(msg)) => {
                assert!(msg.contains("model"), "the message names what failed: {msg}");
                // The underlying error text is logged host-side only. Forwarding
                // it would repeat the leak the git handlers had to fix, where raw
                // tool output carried host paths to the client.
                assert!(
                    !msg.contains("does not support"),
                    "the backend's own wording is not forwarded: {msg}",
                );
            }
            other => panic!("expected a BadRequest explaining the refusal, got {other:?}"),
        }

        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// Reading the catalog is a read; changing it is a write.
///
/// A read-only device should still see which model is running — it simply cannot
/// change it. Gating the list on `may_write` too would hide information the tier
/// was never meant to withhold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_sees_the_catalog_but_cannot_switch() {
    let (registry, conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(registry, Arc::clone(&auth)).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        // Down-scoped after pairing, mirroring the real flow.
        auth.set_read_only(&pubkey, true);

        let choices = call(&client, Request::ListChoices { session_id: "sess-1".into() }).await;
        assert!(matches!(choices, Response::Choices(_)), "reads stay allowed: {choices:?}");

        let set_model = call(
            &client,
            Request::SetModel { session_id: "sess-1".into(), model: "sonnet-5".into() },
        )
        .await;
        assert_eq!(set_model, Response::Error(RpcError::Unauthorized));

        let set_mode = call(
            &client,
            Request::SetPermissionMode { session_id: "sess-1".into(), mode: "plan".into() },
        )
        .await;
        assert_eq!(set_mode, Response::Error(RpcError::Unauthorized));

        drop(client);
    };
    futures::future::join(serve, script).await;

    // The refusal is checked at the backend, not just on the wire — an
    // Unauthorized reply that still applied the change would look identical.
    assert!(
        conn.sent().is_empty(),
        "nothing reached the backend, saw {:?}",
        conn.sent(),
    );
}

/// An unknown session is distinguishable from an unauthorized one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_is_reported_as_such() {
    let (registry, _conn) = switchable_registry();
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let choices = call(&client, Request::ListChoices { session_id: "nope".into() }).await;
        assert_eq!(choices, Response::Error(RpcError::UnknownSession));
        drop(client);
    };
    futures::future::join(serve, script).await;
}
