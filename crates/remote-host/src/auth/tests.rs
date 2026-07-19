use ed25519_dalek::SigningKey;
use oximux_remote_proto::RpcError;
use oximux_remote_proto::messages::RegisterReq;

use super::{AppPubkey, AuthStore, PairingSlot, registration_proof};

fn slot(secret: [u8; 16]) -> PairingSlot {
    PairingSlot::new(secret, None, true)
}

/// A *valid* Ed25519 public key from a seed — `register` verifies the point is
/// on the curve, so tests can't use arbitrary byte patterns.
fn vk(seed: u8) -> AppPubkey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes()
}

#[test]
fn valid_proof_registers_and_issues_a_working_token() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(slot(secret));
    let pubkey = vk(0x33);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    let token = store.register(&req, ts).expect("register");
    assert!(store.is_authorized(&pubkey));
    assert_eq!(store.authorize_token(&token), Some(pubkey));
}

#[test]
fn wrong_secret_is_rejected() {
    let store = AuthStore::new();
    store.set_pairing(slot([0x01; 16]));
    let pubkey = vk(0x33);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&[0x99; 16], &pubkey, ts), // attacker's guess
        timestamp_secs: ts,
        session_id: None,
    };
    assert_eq!(store.register(&req, ts), Err(RpcError::Unauthorized));
    assert!(!store.is_authorized(&pubkey));
}

#[test]
fn stale_timestamp_is_rejected() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(slot(secret));
    let pubkey = vk(0x33);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    // now is well outside the ±60s window.
    assert!(matches!(store.register(&req, ts + 300), Err(RpcError::BadRequest(_))));
}

#[test]
fn one_time_ticket_cannot_be_reused() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(slot(secret));
    let ts = 1_700_000_000;
    let mk = |pk: AppPubkey| RegisterReq {
        app_pubkey: pk,
        device_name: "d".into(),
        proof: registration_proof(&secret, &pk, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    store.register(&mk(vk(0x01)), ts).expect("first use ok");
    assert_eq!(store.register(&mk(vk(0x02)), ts), Err(RpcError::Unauthorized), "one-time ticket burned");
}

#[test]
fn revocation_blocks_further_authorization() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(slot(secret));
    let pubkey = vk(0x44);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    let token = store.register(&req, ts).expect("register");
    store.revoke(&pubkey);
    assert!(!store.is_authorized(&pubkey), "revoked device fails the per-RPC gate");
    assert_eq!(store.authorize_token(&token), None, "revoked device's token dies");
}

#[test]
fn revoked_device_cannot_re_register_itself() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    // Static ticket: the secret stays valid, so only the revoked-guard — not
    // a one-time burn — can block the re-pair attempt.
    store.set_pairing(PairingSlot::new(secret, None, false));
    let pubkey = vk(0x33);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    store.register(&req, ts).expect("initial pairing");
    store.revoke(&pubkey);
    // A revoked device that still knows the secret must NOT resurrect itself.
    assert_eq!(store.register(&req, ts), Err(RpcError::Unauthorized));
    assert!(!store.is_authorized(&pubkey), "revoked stays revoked");
}

#[test]
fn cleared_pairing_rejects_registration() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(PairingSlot::new(secret, None, false));
    store.clear_pairing();
    let pubkey = vk(0x33);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    assert_eq!(store.register(&req, ts), Err(RpcError::Unauthorized), "no advertised secret");
}

#[test]
fn session_bound_ticket_restricts_the_acl() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(PairingSlot::new(secret, Some("sess-1".into()), true));
    let pubkey = vk(0x55);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: Some("sess-1".into()),
    };
    store.register(&req, ts).expect("register");
    assert!(store.is_allowed_for(&pubkey, "sess-1"), "allowed on its bound session");
    assert!(!store.is_allowed_for(&pubkey, "sess-2"), "denied on other sessions");
}

/// Register `pubkey` against a fresh store, optionally session-bound.
fn registered(session: Option<&str>) -> (AuthStore, AppPubkey) {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(PairingSlot::new(secret, session.map(Into::into), false));
    let pubkey = vk(0x33);
    let ts = 1_700_000_000;
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: session.map(Into::into),
    };
    store.register(&req, ts).expect("register");
    (store, pubkey)
}

/// The read-only opt-down: reads stay allowed, every write is refused. This is the
/// tier that makes terminal attach and git writes safe to grant to a device the
/// user doesn't fully trust.
#[test]
fn read_only_device_may_read_but_not_write() {
    let (store, pubkey) = registered(None);

    // Pairing grants full access, so a fresh device may write.
    assert!(store.may_write(&pubkey, "sess-1"), "pairing default is read-write");
    assert!(!store.is_read_only(&pubkey));

    store.set_read_only(&pubkey, true);

    assert!(store.is_allowed_for(&pubkey, "sess-1"), "reads still served");
    assert!(store.is_authorized(&pubkey), "read-only is not revocation");
    assert!(!store.may_write(&pubkey, "sess-1"), "writes refused");
    assert!(store.is_read_only(&pubkey));

    // The opt-down is reversible.
    store.set_read_only(&pubkey, false);
    assert!(store.may_write(&pubkey, "sess-1"), "restored to read-write");
}

/// Read-only and session-scoping are independent dimensions.
#[test]
fn read_only_composes_with_session_scope() {
    let (store, pubkey) = registered(Some("sess-1"));
    store.set_read_only(&pubkey, true);

    assert!(store.is_allowed_for(&pubkey, "sess-1"), "in-scope read allowed");
    assert!(!store.may_write(&pubkey, "sess-1"), "in-scope write refused when read-only");
    assert!(!store.may_write(&pubkey, "sess-2"), "out-of-scope write refused");
    assert!(!store.is_allowed_for(&pubkey, "sess-2"), "out-of-scope read still refused");
}

/// Revocation outranks the tier — a revoked device writes nothing.
#[test]
fn revoked_device_may_not_write() {
    let (store, pubkey) = registered(None);
    store.revoke(&pubkey);

    assert!(!store.may_write(&pubkey, "sess-1"));
}

/// A one-time code is spent by the first device: a second scan of the SAME code
/// is refused, and `pairing_open` reports it so the UI stops showing a dead code.
/// This is what stops a photographed QR being redeemed later while remote access
/// happens to still be on.
#[test]
fn a_one_time_code_is_spent_by_the_first_device() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(PairingSlot::new(secret, None, true));
    assert!(store.pairing_open(), "a fresh code is redeemable");

    let ts = 1_700_000_000;
    let first = vk(0x33);
    let req = RegisterReq {
        app_pubkey: first,
        device_name: "phone".into(),
        proof: registration_proof(&secret, &first, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    store.register(&req, ts).expect("first device pairs");
    assert!(!store.pairing_open(), "the code is spent");

    // A different device presenting the same (valid) proof is refused.
    let second = vk(0x44);
    let replay = RegisterReq {
        app_pubkey: second,
        device_name: "attacker".into(),
        proof: registration_proof(&secret, &second, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    assert_eq!(store.register(&replay, ts), Err(RpcError::Unauthorized), "code cannot be reused");
    assert!(!store.is_authorized(&second), "the second device never gains access");
    assert!(store.is_authorized(&first), "the first device keeps its access");
}

/// Pairing is announced, so the desktop can confirm it rather than a device
/// silently gaining full access.
#[test]
fn pairing_is_announced_to_subscribers() {
    let store = AuthStore::new();
    let secret = [0x22; 16];
    store.set_pairing(PairingSlot::new(secret, None, true));
    let mut events = store.subscribe_pairings();

    let ts = 1_700_000_000;
    let pubkey = vk(0x33);
    let req = RegisterReq {
        app_pubkey: pubkey,
        device_name: "Tien's phone".into(),
        proof: registration_proof(&secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    store.register(&req, ts).expect("register");

    let announced = events.try_recv().expect("a pairing was announced");
    assert_eq!(announced.pubkey, pubkey);
    assert_eq!(announced.name, "Tien's phone", "the name the user will see");
}

/// A failed registration announces nothing — no false confirmation.
#[test]
fn a_rejected_registration_is_not_announced() {
    let store = AuthStore::new();
    store.set_pairing(PairingSlot::new([0x01; 16], None, true));
    let mut events = store.subscribe_pairings();

    let ts = 1_700_000_000;
    let pubkey = vk(0x33);
    let wrong = RegisterReq {
        app_pubkey: pubkey,
        device_name: "attacker".into(),
        // Proof computed against a different secret.
        proof: registration_proof(&[0x02; 16], &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    };
    assert!(store.register(&wrong, ts).is_err(), "wrong secret is refused");
    assert!(events.try_recv().is_err(), "nothing announced for a refused pairing");
}
