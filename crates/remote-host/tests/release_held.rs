//! `ReleaseHeld` (v40): a client letting go of the queue a host restart put on
//! hold.
//!
//! The hold itself is the gate's, and tested there — these are the facts only
//! the dispatcher decides: who may lift it, what an empty hold answers, and
//! that the count the caller reads is the number of lines that actually moved.
//! A caller can read how *many* lines a session holds and never which (the
//! queue's ids do not cross the wire), so the count is the whole of its
//! answer, and a `Response::Ack` would have left "did anything happen?"
//! unanswerable.

use std::sync::{Arc, Mutex};

use oximux_agents::send_gate::{SendGate, SendSource, SendWait};
use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];
/// A valid curve point: registration verifies the key, so an arbitrary byte
/// pattern is refused as `Unauthorized` before the verb is ever reached.
const PUBKEY: [u8; 32] = [0x33; 32];

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

/// One session driven like a host drives one — its gate attached — holding
/// `held` restored lines and `queued` ordinary ones.
fn session_holding(held: usize, queued: usize) -> (Arc<SessionRegistry>, Arc<Mutex<SendGate>>) {
    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    let gate = Arc::new(Mutex::new(SendGate::new()));
    {
        let mut g = gate.lock().unwrap();
        for i in 0..held {
            g.push_restored(format!("held {i}"), SendSource::Remote, None);
        }
        for i in 0..queued {
            g.push(format!("since {i}"), Vec::new(), SendSource::User, Default::default(), None);
        }
    }
    handle.set_gate(Some(gate.clone()));
    (registry, gate)
}

/// Run `script` against a dispatcher over the in-memory pair, with `auth`
/// reachable so a test can down-scope the device mid-connection.
async fn against<F, Fut>(registry: Arc<SessionRegistry>, auth: Arc<AuthStore>, script: F)
where
    F: FnOnce(oximux_remote_proto::testing::DuplexTransport) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, Arc::clone(&auth)).with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    futures::future::join(serve, script(client)).await;
}

/// The count is the lines that moved — the held ones, and not the queue.
///
/// Both numbers matter: reporting the whole queue would tell a caller that
/// messages it never held had just been let go, and reporting a constant would
/// make the reply useless for the one thing it is for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_count_is_the_held_lines_and_they_become_ordinary_sends() {
    let (registry, gate) = session_holding(2, 1);
    let auth = Arc::new(AuthStore::new());
    against(registry, Arc::clone(&auth), |client| async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(PUBKEY))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(&client, Request::ReleaseHeld { session_id: "sess-1".into() }).await,
            Response::Released { released: 2 },
            "the two held lines, not the three in the queue",
        );
        drop(client);
    })
    .await;

    let g = gate.lock().unwrap();
    assert_eq!(g.counts(), (0, 3), "nothing held now, and nothing dropped");
    assert!(
        g.queued().all(|q| g.wait_of(q) != SendWait::HeldRestored),
        "and no line still says the restart is what it waits for",
    );
}

/// Nothing held is a success, not an error: "there was nothing to release" is
/// an answer a script can act on, while an error would have it retry or abort
/// over a session that is simply up to date.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_hold_answers_zero_rather_than_failing() {
    let (registry, _gate) = session_holding(0, 2);
    let auth = Arc::new(AuthStore::new());
    against(registry, Arc::clone(&auth), |client| async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(PUBKEY))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(&client, Request::ReleaseHeld { session_id: "sess-1".into() }).await,
            Response::Released { released: 0 },
        );
        drop(client);
    })
    .await;
}

/// A session with no gate attached — nothing drives it, so nothing can be
/// held — answers zero too, and not an internal fault. This is also the
/// desktop's answer: it keeps its restored messages in the composer rather
/// than the gate, so there is nothing here for this verb to lift.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_with_no_gate_answers_zero() {
    let registry = Arc::new(SessionRegistry::new());
    registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    let auth = Arc::new(AuthStore::new());
    against(registry, Arc::clone(&auth), |client| async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(PUBKEY))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(&client, Request::ReleaseHeld { session_id: "sess-1".into() }).await,
            Response::Released { released: 0 },
        );
        drop(client);
    })
    .await;
}

/// The write tier, because this is what sends those lines. A read-only device
/// may see that a session is holding messages — the count is on its health —
/// and must not be able to put them into an agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_cannot_release_a_hold() {
    let (registry, gate) = session_holding(2, 0);
    let auth = Arc::new(AuthStore::new());
    let for_script = Arc::clone(&auth);
    against(registry, Arc::clone(&auth), |client| async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(PUBKEY))).await
        else {
            panic!("expected Registered");
        };
        // Down-scoped after pairing, mirroring the real flow: the user pairs,
        // then assigns read-only from the paired-device list.
        for_script.set_read_only(&PUBKEY, true);
        assert_eq!(
            call(&client, Request::ReleaseHeld { session_id: "sess-1".into() }).await,
            Response::Error(RpcError::Unauthorized),
        );
        drop(client);
    })
    .await;

    assert_eq!(gate.lock().unwrap().counts(), (2, 0), "and the hold is untouched");
}

/// The refusal is not a session probe: an id this host does not have is
/// `UnknownSession`, the same answer every other session verb gives, so a
/// caller cannot tell a held queue from an absent one by the error it gets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_session_is_unknown() {
    let (registry, _gate) = session_holding(1, 0);
    let auth = Arc::new(AuthStore::new());
    against(registry, Arc::clone(&auth), |client| async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(PUBKEY))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(&client, Request::ReleaseHeld { session_id: "nope".into() }).await,
            Response::Error(RpcError::UnknownSession),
        );
        drop(client);
    })
    .await;
}
