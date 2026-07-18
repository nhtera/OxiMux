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
