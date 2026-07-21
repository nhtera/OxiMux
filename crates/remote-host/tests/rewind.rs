//! The rewind RPC (v8): dropping a session back to an earlier turn.
//!
//! Rewinding is the most destructive call on this protocol after terminal
//! input — it removes conversation the user may not be able to reconstruct —
//! so these tests care as much about what *doesn't* reach the service as about
//! what does.

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{
    AuthStore, Dispatcher, PairingSlot, RewindError, RewindService, registration_proof,
};
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

/// A rewind service whose outcome the test scripts, recording every call so a
/// test can assert a refused request reached it *not at all*.
struct ScriptedRewinder {
    calls: std::sync::Mutex<Vec<(String, usize, bool)>>,
    result: Result<(), &'static str>,
}

#[async_trait::async_trait]
impl RewindService for ScriptedRewinder {
    async fn rewind(
        &self,
        session_id: &str,
        ordinal: usize,
        include_files: bool,
    ) -> Result<(), RewindError> {
        self.calls.lock().unwrap().push((session_id.to_string(), ordinal, include_files));
        match self.result {
            Ok(()) => Ok(()),
            Err("files") => Err(RewindError::FilesUnsupported),
            Err("ordinal") => Err(RewindError::OrdinalMismatch),
            _ => Err(RewindError::Failed),
        }
    }
}

fn rewinder(result: Result<(), &'static str>) -> Arc<ScriptedRewinder> {
    Arc::new(ScriptedRewinder { calls: std::sync::Mutex::new(Vec::new()), result })
}

fn paired_auth() -> Arc<AuthStore> {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    auth
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paired_device_rewinds_a_session() {
    let rewinder = rewinder(Ok(()));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), paired_auth())
        .with_clock(clock)
        .with_rewinder(rewinder.clone());

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32]))).await
        else {
            panic!("expected Registered");
        };
        let done = call(
            &client,
            Request::RewindSession {
                session_id: "sess-1".into(),
                ordinal: 2,
                include_files: false,
            },
        )
        .await;
        // An acknowledgement only — the truncation itself arrives on the event
        // stream, the same path a desktop-initiated rewind takes.
        assert_eq!(done, Response::Ack);
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert_eq!(
        rewinder.calls.lock().unwrap().as_slice(),
        &[("sess-1".to_string(), 2, false)],
        "the target and ordinal reached the service unchanged",
    );
}

/// The whole point of the read-only tier: watch without acting. A rewind
/// destroys conversation, so it is squarely on the acting side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_cannot_rewind() {
    let rewinder = rewinder(Ok(()));
    let auth = paired_auth();
    let pubkey = [0x33; 32];
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_rewinder(rewinder.clone());

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

        let refused = call(
            &client,
            Request::RewindSession {
                session_id: "sess-1".into(),
                ordinal: 0,
                include_files: false,
            },
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;

    // The gate must stop the request before the service sees it — a refusal that
    // still ran the rewind would be no refusal at all.
    assert!(
        rewinder.calls.lock().unwrap().is_empty(),
        "nothing reached the rewind service, saw {:?}",
        rewinder.calls.lock().unwrap(),
    );
}

/// A session-scoped device may rewind the session it is scoped to, and nothing
/// else. Unlike `CreateSession`, plain `may_write` is the correct gate here
/// because the request names a session to narrow against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_scoped_device_cannot_rewind_another_session() {
    let rewinder = rewinder(Ok(()));
    let auth = Arc::new(AuthStore::new());
    // Paired into a single session's scope, as a session-bound pairing does.
    auth.set_pairing(PairingSlot::new(SECRET, Some("sess-mine".into()), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_rewinder(rewinder.clone());

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
            Request::RewindSession {
                session_id: "sess-other".into(),
                ordinal: 0,
                include_files: false,
            },
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));

        let allowed = call(
            &client,
            Request::RewindSession {
                session_id: "sess-mine".into(),
                ordinal: 0,
                include_files: false,
            },
        )
        .await;
        assert_eq!(allowed, Response::Ack, "its own session is still reachable");
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert_eq!(
        rewinder.calls.lock().unwrap().as_slice(),
        &[("sess-mine".to_string(), 0, false)],
        "only the in-scope session reached the service",
    );
}

/// A refusal must keep its category. A client that asked for the files axis
/// needs to distinguish "not offered here" (fall back to conversation-only)
/// from "the rewind failed" (surface an error), and both from a stale ordinal
/// (resync).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_reach_the_client_distinguishably() {
    for (scripted, expected) in [
        (Err("files"), RewindError::FilesUnsupported.to_string()),
        (Err("ordinal"), RewindError::OrdinalMismatch.to_string()),
    ] {
        let rewinder = rewinder(scripted);
        let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), paired_auth())
            .with_clock(clock)
            .with_rewinder(rewinder.clone());

        let (client, server) = duplex_pair();
        let serve = dispatcher.serve(&server);
        let expected = expected.clone();
        let script = async move {
            let Response::Registered { .. } =
                call(&client, Request::Register(register_req([0x33; 32]))).await
            else {
                panic!("expected Registered");
            };
            let refused = call(
                &client,
                Request::RewindSession {
                    session_id: "sess-1".into(),
                    ordinal: 1,
                    include_files: true,
                },
            )
            .await;
            assert_eq!(refused, Response::Error(RpcError::BadRequest(expected)));
            drop(client);
        };
        futures::future::join(serve, script).await;
    }
}

/// A host with no rewind service answers `Unauthorized`, not a distinct
/// "unsupported" — whether this desktop can rewind is not something an
/// unauthorized client should be able to probe. Same reasoning the terminal and
/// launch RPCs use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_a_rewinder_is_indistinguishable_from_one_that_refuses() {
    let dispatcher =
        Dispatcher::new(Arc::new(SessionRegistry::new()), paired_auth()).with_clock(clock);

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
            Request::RewindSession {
                session_id: "sess-1".into(),
                ordinal: 0,
                include_files: false,
            },
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;
}

/// An unregistered connection must not rewind anything. The authentication
/// check precedes the authorization gate, and both precede the service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unregistered_connection_cannot_rewind() {
    let rewinder = rewinder(Ok(()));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), paired_auth())
        .with_clock(clock)
        .with_rewinder(rewinder.clone());

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let refused = call(
            &client,
            Request::RewindSession {
                session_id: "sess-1".into(),
                ordinal: 0,
                include_files: false,
            },
        )
        .await;
        assert_eq!(refused, Response::Error(RpcError::Unauthorized));
        drop(client);
    };
    futures::future::join(serve, script).await;

    assert!(rewinder.calls.lock().unwrap().is_empty(), "nothing reached the service");
}
