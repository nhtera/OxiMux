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
fn registration_is_rejected_when_no_pairing_is_advertised() {
    // No `set_pairing` at all — the state the host is in whenever remote access
    // is off, and the one a `Register` must never get through.
    let store = AuthStore::new();
    let secret = [0x22; 16];
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
    assert!(store.devices().iter().all(|d| !d.read_only), "fresh device is not read-only");

    store.set_read_only(&pubkey, true);

    assert!(store.is_allowed_for(&pubkey, "sess-1"), "reads still served");
    assert!(store.is_authorized(&pubkey), "read-only is not revocation");
    assert!(!store.may_write(&pubkey, "sess-1"), "writes refused");
    assert!(store.devices().iter().any(|d| d.pubkey == pubkey && d.read_only), "the tier shows in the device list the UI reads");

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

/// The behaviour the pairing row exists to provide: a second device pairs through
/// a freshly-opened window while the first keeps everything it had.
///
/// This used to require toggling remote access off and on, which rebuilds the auth
/// store and closes the iroh endpoint under whoever is already connected. Opening a
/// window on the live store is the whole difference, so it is worth pinning that
/// the first device survives it — authorization, and not merely presence in the
/// list.
#[test]
fn a_second_device_pairs_through_a_new_window_without_disturbing_the_first() {
    let store = AuthStore::new();
    let ts = 1_700_000_000;

    let first_secret = [0x11; 16];
    store.set_pairing(slot(first_secret));
    let first = vk(0x33);
    let first_token = store
        .register(&a_register(first, "first phone", &first_secret, ts), ts)
        .expect("first device pairs");
    assert!(!store.pairing_open(), "the one-time code is spent");

    // What "Pair a device" does: a new secret on the SAME store, no rebind.
    let second_secret = [0x22; 16];
    store.set_pairing(slot(second_secret));
    assert!(store.pairing_open(), "a fresh window is open again");

    let second = vk(0x44);
    store
        .register(&a_register(second, "second phone", &second_secret, ts), ts)
        .expect("second device pairs through the new window");

    assert!(store.is_authorized(&second), "the second device gained access");
    assert!(store.is_authorized(&first), "the first device kept its access");
    assert_eq!(
        store.authorize_token(&first_token),
        Some(first),
        "and its reconnect token still resolves — the session was never torn down",
    );
    assert_eq!(store.devices().len(), 2, "both are listed");
}

/// A code that outlived its window is refused even though it was never redeemed.
/// Without this the countdown would be theatre: the desktop would say a code had
/// lapsed while the host went on accepting it.
#[test]
fn an_expired_window_stops_redeeming() {
    let store = AuthStore::new();
    let secret = [0x55; 16];
    let opened = 1_700_000_000;
    store.set_pairing(PairingSlot::expiring(secret, None, true, opened + 300));

    assert!(store.pairing_open_at(opened + 299), "still open a second before");
    assert!(!store.pairing_open_at(opened + 300), "closed on the boundary");

    let pubkey = vk(0x66);
    // Registered at a timestamp the skew window accepts, so expiry is the only
    // thing that can refuse it.
    let late = opened + 300;
    assert_eq!(
        store.register(&a_register(pubkey, "late phone", &secret, late), late),
        Err(RpcError::Unauthorized),
        "the secret is correct but the window has closed",
    );
    assert!(!store.is_authorized(&pubkey), "no access was granted");
}

/// Leaving the pairing view retires the code immediately, rather than leaving a
/// window nobody is watching open until it times out.
#[test]
fn closing_the_window_revokes_an_unused_code() {
    let store = AuthStore::new();
    let secret = [0x77; 16];
    store.set_pairing(slot(secret));
    assert!(store.pairing_open(), "open to begin with");

    store.close_pairing();

    assert!(!store.pairing_open(), "closed on request");
    let ts = 1_700_000_000;
    let pubkey = vk(0x88);
    assert_eq!(
        store.register(&a_register(pubkey, "phone", &secret, ts), ts),
        Err(RpcError::Unauthorized),
        "a code from the closed window no longer redeems",
    );
}

/// A well-formed registration for `pubkey`, proved against `secret`.
fn a_register(pubkey: AppPubkey, name: &str, secret: &[u8; 16], ts: u64) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: name.into(),
        proof: registration_proof(secret, &pubkey, ts),
        timestamp_secs: ts,
        session_id: None,
    }
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

/// A session-scoped device gets **no** terminal access at all — not a filtered
/// list, not read-only, none.
///
/// Terminals have no owning agent session, so there is nothing for a narrowed
/// scope to be narrowed *to*. The tempting alternative — matching a terminal's
/// cwd against the scoped session's — would hand a device the desktop user
/// deliberately confined to one conversation a shell on the machine, which is
/// the single widest privilege this protocol can grant. Refusing outright is the
/// conservative reading of a scope the user chose.
#[test]
fn a_session_scoped_device_gets_no_terminal_access() {
    let (store, pubkey) = registered(Some("sess-1"));

    assert!(store.is_allowed_for(&pubkey, "sess-1"), "its own session still works");
    assert!(!store.may_use_terminals(&pubkey), "terminals are not in a narrowed scope");
    assert!(!store.may_drive_terminals(&pubkey), "and certainly not writable");
}

/// A full-scope device can watch terminals; read-only stops it typing.
///
/// This is the tier that makes terminal attach survivable: typing into a live
/// shell is arbitrary code execution on the desktop, so "can see it" and "can
/// drive it" must be separable.
#[test]
fn read_only_lets_a_device_watch_a_terminal_but_not_type() {
    let (store, pubkey) = registered(None);
    assert!(store.may_drive_terminals(&pubkey), "full access types by default");

    store.set_read_only(&pubkey, true);

    assert!(store.may_use_terminals(&pubkey), "still allowed to watch");
    assert!(!store.may_drive_terminals(&pubkey), "but not to type or resize");

    store.set_read_only(&pubkey, false);
    assert!(store.may_drive_terminals(&pubkey), "the opt-down is reversible");
}

/// Revocation outranks everything, including terminal read access.
#[test]
fn a_revoked_device_loses_terminals_entirely() {
    let (store, pubkey) = registered(None);
    store.revoke(&pubkey);

    assert!(!store.may_use_terminals(&pubkey));
    assert!(!store.may_drive_terminals(&pubkey));
}
