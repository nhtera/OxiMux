//! The v16 pairing administration: local operator only, one-time expiring
//! tickets, and the read-only opt-down flowing into the minted enrollment.
//! The lateral-movement property is the point of most of these: no remote
//! device — whatever its tier — may mint, list, or erase enrollments.

use std::sync::Arc;

use futures::executor::block_on;
use futures::future::join;
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{AuthStore, Dispatcher, LocalScope, registration_proof};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::pairing::PairingTicket;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const ENDPOINT: [u8; 32] = [0x11; 32];

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn dispatcher(auth: Arc<AuthStore>) -> Dispatcher {
    Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_pairing_endpoint(ENDPOINT)
        .with_clock(clock)
}

fn register_with(secret: &[u8; 16], pubkey: [u8; 32]) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: "laptop".into(),
        proof: registration_proof(secret, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: None,
    }
}

/// A valid Ed25519 pubkey for enrollments the tests mint (the register path
/// validates key structure, so an all-zeros key would be refused). Seeded from
/// random bytes rather than `SigningKey::generate` — that constructor needs a
/// feature this crate does not enable, and any 32-byte seed is a valid key.
fn test_pubkey() -> [u8; 32] {
    use rand::RngCore as _;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key().to_bytes()
}

/// The full loop: operator mints a ticket over the local socket, a device
/// redeems it, the enrollment shows in `PairList` at the minted tier, and the
/// one-time window is spent — a second redemption is refused.
#[test]
fn minted_ticket_enrolls_once_at_the_chosen_tier() {
    let auth = Arc::new(AuthStore::new());
    let dispatcher = dispatcher(auth.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let ticket = block_on(async {
        let script = async {
            let Response::PairingIssued(issued) =
                call(&client, Request::PairNew { read_only: true }).await
            else {
                panic!("the operator may mint");
            };
            assert!(issued.read_only, "the opt-down is echoed");
            assert_eq!(issued.expires_at, NOW + 120, "short-lived by construction");
            drop(client);
            issued.ticket
        };
        join(serve, script).await.1
    });
    let ticket = PairingTicket::decode(&ticket).expect("the wire carries the canonical encoding");
    assert_eq!(ticket.endpoint_id, ENDPOINT, "the ticket names the bound endpoint");

    // A device redeems it over the remote path.
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth.clone())
        .with_pairing_endpoint(ENDPOINT)
        .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let device = test_pubkey();
    block_on(join(serve, async {
        let reply =
            call(&client, Request::Register(register_with(&ticket.handshake_secret, device)))
                .await;
        assert!(matches!(reply, Response::Registered { .. }), "first redemption succeeds");
        // One-time: the window is spent.
        let again = test_pubkey();
        let reply =
            call(&client, Request::Register(register_with(&ticket.handshake_secret, again)))
                .await;
        assert_eq!(
            reply,
            Response::Error(RpcError::Unauthorized),
            "a spent window redeems nothing"
        );
        drop(client);
    }));

    // The enrollment landed read-only, and the operator can see and erase it.
    let dispatcher = dispatcher_over(auth.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    block_on(join(serve, async {
        let Response::PairedDeviceList(rows) = call(&client, Request::PairList).await else {
            panic!("the operator may list");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pubkey, device);
        assert!(rows[0].read_only, "the ticket's tier reached the enrollment");
        assert_eq!(
            call(&client, Request::PairRemove { pubkey: device }).await,
            Response::Ack
        );
        let Response::PairedDeviceList(rows) = call(&client, Request::PairList).await else {
            panic!("the operator may list");
        };
        assert!(rows.is_empty(), "erased, so the device may pair again someday");
        drop(client);
    }));
}

fn dispatcher_over(auth: Arc<AuthStore>) -> Dispatcher {
    Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_pairing_endpoint(ENDPOINT)
        .with_clock(clock)
}

/// The default tier is full write; `read_only: false` mints an enrollment
/// that can act.
#[test]
fn default_tier_is_full_write() {
    let auth = Arc::new(AuthStore::new());
    let dispatcher = dispatcher(auth.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let ticket = block_on(async {
        let script = async {
            let Response::PairingIssued(issued) =
                call(&client, Request::PairNew { read_only: false }).await
            else {
                panic!("the operator may mint");
            };
            assert!(!issued.read_only);
            drop(client);
            issued.ticket
        };
        join(serve, script).await.1
    });
    let ticket = PairingTicket::decode(&ticket).unwrap();
    let dispatcher = dispatcher_over(auth.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let device = test_pubkey();
    block_on(join(serve, async {
        let reply =
            call(&client, Request::Register(register_with(&ticket.handshake_secret, device)))
                .await;
        assert!(matches!(reply, Response::Registered { .. }));
        drop(client);
    }));
    let listed = auth.devices();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].read_only, "the default enrollment can act");
}

/// No remote device — even one enrolled at full write — may administer
/// pairing. This is the lateral-movement boundary.
#[test]
fn remote_devices_cannot_administer_pairing() {
    let auth = Arc::new(AuthStore::new());
    // Enroll a full-write device the legitimate way.
    let secret = [0x22; 16];
    auth.set_pairing(oximux_remote_host::PairingSlot::new(secret, None, false));
    let dispatcher = dispatcher_over(auth.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    block_on(join(serve, async {
        let device = test_pubkey();
        let reply = call(&client, Request::Register(register_with(&secret, device))).await;
        assert!(matches!(reply, Response::Registered { .. }));
        // Now, authenticated at the highest remote tier, every admin verb is
        // refused.
        for (what, req) in [
            ("mint", Request::PairNew { read_only: false }),
            ("list", Request::PairList),
            ("remove", Request::PairRemove { pubkey: device }),
        ] {
            assert_eq!(
                call(&client, req).await,
                Response::Error(RpcError::Unauthorized),
                "a remote device must not {what} enrollments"
            );
        }
        drop(client);
    }));
}

/// A session-scoped local caller (an agent) is refused too — pairing
/// administration is the operator, nothing less.
#[test]
fn a_session_scoped_local_caller_cannot_administer_pairing() {
    let dispatcher = dispatcher(Arc::new(AuthStore::new()));
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Session("sess-1".into()));
    block_on(join(serve, async {
        for req in [
            Request::PairNew { read_only: false },
            Request::PairList,
            Request::PairRemove { pubkey: [0x33; 32] },
        ] {
            assert_eq!(call(&client, req).await, Response::Error(RpcError::Unauthorized));
        }
        drop(client);
    }));
}

/// A host with no bound endpoint has nothing redeemable to offer: `PairNew`
/// answers `Unsupported` to the operator (list/remove still work — the device
/// table exists regardless).
#[test]
fn no_endpoint_means_no_tickets() {
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()))
        .with_clock(clock);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    block_on(join(serve, async {
        assert_eq!(
            call(&client, Request::PairNew { read_only: false }).await,
            Response::Error(RpcError::Unsupported)
        );
        let Response::PairedDeviceList(rows) = call(&client, Request::PairList).await else {
            panic!("listing needs no endpoint");
        };
        assert!(rows.is_empty());
        drop(client);
    }));
}

/// The minted window really expires: past `expires_at` the secret redeems
/// nothing, even unused.
#[test]
fn an_expired_window_redeems_nothing() {
    let auth = Arc::new(AuthStore::new());
    let dispatcher = dispatcher(auth.clone());
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve_local(&server, LocalScope::Full);
    let ticket = block_on(async {
        let script = async {
            let Response::PairingIssued(issued) =
                call(&client, Request::PairNew { read_only: false }).await
            else {
                panic!("the operator may mint");
            };
            drop(client);
            issued.ticket
        };
        join(serve, script).await.1
    });
    let ticket = PairingTicket::decode(&ticket).unwrap();
    // A dispatcher whose clock sits past the window.
    fn later() -> u64 {
        NOW + 121
    }
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_pairing_endpoint(ENDPOINT)
        .with_clock(later);
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    block_on(join(serve, async {
        let device = test_pubkey();
        let mut req = register_with(&ticket.handshake_secret, device);
        // The proof itself is fresh relative to the later clock; only the
        // window has lapsed.
        req.timestamp_secs = later();
        req.proof = registration_proof(&ticket.handshake_secret, &device, later());
        assert_eq!(
            call(&client, Request::Register(req)).await,
            Response::Error(RpcError::Unauthorized),
            "an expired window is dead even though it was never used"
        );
        drop(client);
    }));
}
