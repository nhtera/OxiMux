//! The read-only forge RPCs (v9): issues, pull requests and CI checks.
//!
//! These tests run against a directory that is **not** forge-hosted (a bare temp
//! dir with no `origin`), which is deliberate: it exercises the contract that
//! matters most here. Every "can't tell" case — no CLI, signed-out CLI, repo
//! hosted elsewhere, network down — must surface as an empty list, never an
//! error. A client cannot act on an error it has no way to fix, and the host
//! genuinely cannot tell those cases apart.
//!
//! They do not assert on real forge data. Doing so would require a live `gh`
//! login and network, making the suite depend on someone's GitHub session.

use std::sync::Arc;

use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{ForgeItemKindWire, ForgeStateWire, RegisterReq};
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

/// A registry holding one session rooted at a directory with no forge remote.
fn registry_with_cwd(cwd: std::path::PathBuf) -> Arc<SessionRegistry> {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: None, model: None, cwd: Some(cwd) });
    registry
}

fn paired_auth() -> Arc<AuthStore> {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    auth
}

/// The central contract: a repo that is not forge-hosted answers with empty
/// lists, not errors. The phone shows "nothing here" — the same state the
/// desktop's own Tasks page shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repo_with_no_forge_answers_empty_not_an_error() {
    let dir = std::env::temp_dir();
    let dispatcher = Dispatcher::new(registry_with_cwd(dir), paired_auth()).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };

        let issues = call(
            &client,
            Request::ListForgeItems {
                session_id: "sess-1".into(),
                kind: ForgeItemKindWire::Issue,
                state: ForgeStateWire::Open,
                mine: false,
            },
        )
        .await;
        assert_eq!(issues, Response::ForgeItems(vec![]), "empty, not an error");

        let prs = call(
            &client,
            Request::ListForgeItems {
                session_id: "sess-1".into(),
                kind: ForgeItemKindWire::Pull,
                state: ForgeStateWire::Open,
                mine: false,
            },
        )
        .await;
        assert_eq!(prs, Response::ForgeItems(vec![]));

        let checks = call(&client, Request::ListForgeChecks { session_id: "sess-1".into() }).await;
        assert_eq!(checks, Response::ForgeChecks(vec![]));

        let detail = call(
            &client,
            Request::GetForgeItemDetail {
                session_id: "sess-1".into(),
                kind: ForgeItemKindWire::Pull,
                number: 1,
            },
        )
        .await;
        // `None` rather than an empty body: "the CLI could not tell us" and "an
        // item with no description" are different answers, and the client shows
        // different things for them.
        assert_eq!(detail, Response::ForgeItemDetail(None));

        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// Forge access is scoped by the session's own `cwd`, so a session-scoped device
/// cannot enumerate another project's issues — the same containment the git RPCs
/// rely on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_scoped_device_cannot_read_another_sessions_forge() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, Some("sess-mine".into()), false));
    let dispatcher =
        Dispatcher::new(registry_with_cwd(std::env::temp_dir()), Arc::clone(&auth)).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let refused = call(
            &client,
            Request::ListForgeItems {
                session_id: "sess-1".into(),
                kind: ForgeItemKindWire::Issue,
                state: ForgeStateWire::Open,
                mine: false,
            },
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A **read-only device may read the forge.** That is the point of the tier: it
/// watches without acting, and nothing on this surface acts. Asserted rather
/// than assumed, because the reflex when adding an RPC is to reach for
/// `may_write` — which would silently withhold information this tier was always
/// meant to see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_can_still_read_the_forge() {
    let auth = paired_auth();
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(registry_with_cwd(std::env::temp_dir()), Arc::clone(&auth))
        .with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };
        auth.set_read_only(&pubkey, true);

        let issues = call(
            &client,
            Request::ListForgeItems {
                session_id: "sess-1".into(),
                kind: ForgeItemKindWire::Issue,
                state: ForgeStateWire::Open,
                mine: false,
            },
        )
        .await;
        assert!(
            matches!(issues, Response::ForgeItems(_)),
            "reads stay allowed for a read-only device, saw {issues:?}",
        );
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// A session the host does not know is a distinct answer from an empty list —
/// the client should resync rather than render "no issues".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_is_reported_as_such() {
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), paired_auth())
        .with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let missing = call(&client, Request::ListForgeChecks { session_id: "nope".into() }).await;
        assert_eq!(missing, Response::Error(RpcError::UnknownSession));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// An unregistered connection reads nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unregistered_connection_cannot_read_the_forge() {
    let dispatcher =
        Dispatcher::new(registry_with_cwd(std::env::temp_dir()), paired_auth()).with_clock(clock);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let refused = call(
            &client,
            Request::ListForgeItems {
                session_id: "sess-1".into(),
                kind: ForgeItemKindWire::Issue,
                state: ForgeStateWire::Open,
                mine: false,
            },
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
}
