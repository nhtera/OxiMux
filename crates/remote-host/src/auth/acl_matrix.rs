//! The ACL as one table: every predicate against every caller class.
//!
//! The sibling [`tests`](super::tests) module proves the *pairing* machinery —
//! that a proof registers, that revocation bites, that a one-time code is spent.
//! This one proves the **authorization** half, and it exists because that half
//! had no direct coverage at all: eleven of the seventeen predicates were
//! reachable only through a dispatcher RPC, and the two local classes — the
//! operator and the session-confined agent — were not exercised anywhere.
//!
//! That gap mattered most where the design leans hardest. Agent CLI access is on
//! by default and session-scoped, so [`LocalScope::Session`] is the *sole*
//! containment boundary around an agent that can already run arbitrary code in
//! its own workspace. A silent widening of any full-host predicate would hand
//! that agent the host, and nothing here would have noticed.
//!
//! Written as a whole-row comparison rather than a heap of individual asserts:
//! a mismatch prints the expected and actual rows side by side, so a diff reads
//! as "this class gained `manage_worktrees`" instead of as one anonymous
//! `assert!` line number.

use ed25519_dalek::SigningKey;
use oximux_remote_proto::messages::RegisterReq;

use super::{AppPubkey, AuthStore, LocalScope, PairingSlot, Peer, registration_proof};

/// The session a scoped caller is confined to, and one it is not. Every
/// session-shaped predicate is asked about both, because "may act on its own
/// session" and "may not act on another" are two different claims and only the
/// pair of them describes confinement.
const IN_SCOPE: &str = "sess-1";
const OUT_OF_SCOPE: &str = "sess-2";

/// What one caller class may do. Every field is one predicate; the
/// [`the_matrix_covers_every_predicate`] test below fails if `acl.rs` grows one
/// that never made it into this struct.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    // Session-shaped: asked about the confined session and about another.
    allowed_in_scope: bool,
    allowed_out_of_scope: bool,
    write_in_scope: bool,
    write_out_of_scope: bool,
    manage_heartbeats_in_scope: bool,
    manage_heartbeats_out_of_scope: bool,
    read_heartbeats_in_scope: bool,
    read_heartbeats_out_of_scope: bool,
    // Full-host surfaces: no session to narrow against, so a confined caller is
    // refused outright rather than served a filtered view.
    use_terminals: bool,
    drive_terminals: bool,
    create_sessions: bool,
    read_schedules: bool,
    manage_schedules: bool,
    read_worktrees: bool,
    manage_worktrees: bool,
    administer_pairing: bool,
    manage_teams: bool,
    // A run the caller is in, and one it is not.
    read_own_team_run: bool,
    read_other_team_run: bool,
    // The coarse pre-target gate an idempotent heartbeat delete needs.
    attempt_heartbeat_writes: bool,
    // The one surface a confined caller deliberately shares host-wide.
    read_state: bool,
    write_state: bool,
}

/// Ask every predicate about one caller.
fn observe(store: &AuthStore, peer: &Peer) -> Row {
    Row {
        allowed_in_scope: store.is_allowed_for(peer, IN_SCOPE),
        allowed_out_of_scope: store.is_allowed_for(peer, OUT_OF_SCOPE),
        write_in_scope: store.may_write(peer, IN_SCOPE),
        write_out_of_scope: store.may_write(peer, OUT_OF_SCOPE),
        manage_heartbeats_in_scope: store.may_manage_heartbeats(peer, IN_SCOPE),
        manage_heartbeats_out_of_scope: store.may_manage_heartbeats(peer, OUT_OF_SCOPE),
        read_heartbeats_in_scope: store.may_read_heartbeats(peer, IN_SCOPE),
        read_heartbeats_out_of_scope: store.may_read_heartbeats(peer, OUT_OF_SCOPE),
        use_terminals: store.may_use_terminals(peer),
        drive_terminals: store.may_drive_terminals(peer),
        create_sessions: store.may_create_sessions(peer),
        read_schedules: store.may_read_schedules(peer),
        manage_schedules: store.may_manage_schedules(peer),
        read_worktrees: store.may_read_worktrees(peer),
        manage_worktrees: store.may_manage_worktrees(peer),
        administer_pairing: store.may_administer_pairing(peer),
        manage_teams: store.may_manage_teams(peer),
        read_own_team_run: store.may_read_team_run(peer, &[IN_SCOPE.to_string()]),
        read_other_team_run: store.may_read_team_run(peer, &[OUT_OF_SCOPE.to_string()]),
        attempt_heartbeat_writes: store.may_attempt_heartbeat_writes(peer),
        read_state: store.may_read_state(peer),
        write_state: store.may_write_state(peer),
    }
}

/// A row where everything is refused — the base every deny-shaped class is
/// written as, so a class that admits something says so in one line.
const NOTHING: Row = Row {
    allowed_in_scope: false,
    allowed_out_of_scope: false,
    write_in_scope: false,
    write_out_of_scope: false,
    manage_heartbeats_in_scope: false,
    manage_heartbeats_out_of_scope: false,
    read_heartbeats_in_scope: false,
    read_heartbeats_out_of_scope: false,
    use_terminals: false,
    drive_terminals: false,
    create_sessions: false,
    read_schedules: false,
    manage_schedules: false,
    read_worktrees: false,
    manage_worktrees: false,
    administer_pairing: false,
    manage_teams: false,
    read_own_team_run: false,
    read_other_team_run: false,
    attempt_heartbeat_writes: false,
    read_state: false,
    write_state: false,
};

/// A *valid* Ed25519 public key from a seed — `register` verifies the point is
/// on the curve, so an arbitrary byte pattern will not do.
fn vk(seed: u8) -> AppPubkey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes()
}

/// A store holding one registered device, scoped as `session` says.
fn remote(session: Option<&str>) -> (AuthStore, Peer) {
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
    (store, Peer::remote(pubkey))
}

/// The same, then opted down to the watch-only tier.
fn remote_read_only(session: Option<&str>) -> (AuthStore, Peer) {
    let (store, peer) = remote(session);
    store.set_read_only(peer.remote_pubkey().expect("remote"), true);
    (store, peer)
}

// ---------------------------------------------------------------------------
// Local classes — the CLI over the host's own owner-only socket.
// ---------------------------------------------------------------------------

/// The operator at their own keyboard: the same authority the desktop already
/// obeys, so every gate opens. `administer_pairing` is the one that separates
/// this class from a fully-trusted remote device.
#[test]
fn a_local_operator_reaches_everything_including_pairing() {
    let store = AuthStore::new();
    let peer = Peer::local(LocalScope::Full);
    assert_eq!(
        observe(&store, &peer),
        Row {
            allowed_in_scope: true,
            allowed_out_of_scope: true,
            write_in_scope: true,
            write_out_of_scope: true,
            manage_heartbeats_in_scope: true,
            manage_heartbeats_out_of_scope: true,
            read_heartbeats_in_scope: true,
            read_heartbeats_out_of_scope: true,
            use_terminals: true,
            drive_terminals: true,
            create_sessions: true,
            read_schedules: true,
            manage_schedules: true,
            read_worktrees: true,
            manage_worktrees: true,
            administer_pairing: true,
            manage_teams: true,
            read_own_team_run: true,
            read_other_team_run: true,
            attempt_heartbeat_writes: true,
            read_state: true,
            write_state: true,
        },
        "the local operator is the host's own owner",
    );
}

/// **The load-bearing row.** An agent's own CLI, confined to the session that
/// spawned it. It drives its own conversation and nothing else: no other
/// session, no terminal, no schedule, no worktree, no pairing, no team — every
/// one of those would be a way to create or reach its way out of the box.
///
/// The two deliberate exceptions are called out below, because a reader
/// checking this row will otherwise read them as leaks.
#[test]
fn a_session_confined_agent_reaches_only_its_own_session() {
    let store = AuthStore::new();
    let peer = Peer::local(LocalScope::Session(IN_SCOPE.into()));
    assert_eq!(
        observe(&store, &peer),
        Row {
            allowed_in_scope: true,
            write_in_scope: true,
            manage_heartbeats_in_scope: true,
            read_heartbeats_in_scope: true,
            read_own_team_run: true,
            // Deliberate exception 1: the coarse gate an idempotent heartbeat
            // delete needs *before* a target is known. Admitting a confined
            // caller here is the point — deleting its own wake-up is what it is
            // for — and the per-target scope check still runs the moment a row
            // is found (`manage_heartbeats_out_of_scope: false`, above).
            attempt_heartbeat_writes: true,
            // Deliberate exception 2: the coordination blackboard is shared
            // host-wide, because a board narrowed to one session coordinates
            // nothing. What keeps it defensible is that it holds only what
            // agents deliberately put on it — no host paths, no credentials, no
            // session contents.
            read_state: true,
            write_state: true,
            ..NOTHING
        },
        "session confinement is the only boundary around an agent's own CLI",
    );
}

/// Confinement is about the *named* session, not about the caller having been
/// confined to something. A scope bound to one id refuses every other id, which
/// is what stops an agent from walking into the session next door.
#[test]
fn a_confined_scope_refuses_the_session_next_door() {
    let store = AuthStore::new();
    let mine = Peer::local(LocalScope::Session(IN_SCOPE.into()));
    let theirs = Peer::local(LocalScope::Session(OUT_OF_SCOPE.into()));

    assert!(store.may_write(&mine, IN_SCOPE));
    assert!(!store.may_write(&mine, OUT_OF_SCOPE));
    assert!(store.may_write(&theirs, OUT_OF_SCOPE));
    assert!(!store.may_write(&theirs, IN_SCOPE));
}

// ---------------------------------------------------------------------------
// Remote classes — paired devices over the wire.
// ---------------------------------------------------------------------------

/// A fully-paired, write-capable device: everything an operator has **except
/// pairing administration**. That single `false` is the anti-lateral-movement
/// gate — a device that could mint tickets could enroll a fleet.
#[test]
fn a_full_remote_device_reaches_everything_except_pairing() {
    let (store, peer) = remote(None);
    assert_eq!(
        observe(&store, &peer),
        Row {
            allowed_in_scope: true,
            allowed_out_of_scope: true,
            write_in_scope: true,
            write_out_of_scope: true,
            manage_heartbeats_in_scope: true,
            manage_heartbeats_out_of_scope: true,
            read_heartbeats_in_scope: true,
            read_heartbeats_out_of_scope: true,
            use_terminals: true,
            drive_terminals: true,
            create_sessions: true,
            read_schedules: true,
            manage_schedules: true,
            read_worktrees: true,
            manage_worktrees: true,
            // The strictest gate on the protocol: local operator only.
            administer_pairing: false,
            manage_teams: true,
            read_own_team_run: true,
            read_other_team_run: true,
            attempt_heartbeat_writes: true,
            read_state: true,
            write_state: true,
        },
        "a paired device may not enroll further devices",
    );
}

/// The watch-only opt-down. Reads are served at full breadth — including seeing
/// a terminal — and every state-changing predicate is refused, including the
/// blackboard write that would otherwise let a watcher steer agents.
#[test]
fn a_read_only_remote_device_watches_but_never_acts() {
    let (store, peer) = remote_read_only(None);
    assert_eq!(
        observe(&store, &peer),
        Row {
            allowed_in_scope: true,
            allowed_out_of_scope: true,
            read_heartbeats_in_scope: true,
            read_heartbeats_out_of_scope: true,
            // Read access to a terminal shows the screen; it does not type.
            use_terminals: true,
            read_schedules: true,
            read_worktrees: true,
            read_own_team_run: true,
            read_other_team_run: true,
            read_state: true,
            ..NOTHING
        },
        "read-only is the tier that separates watching from driving",
    );
}

/// A device enrolled through a session-bound ticket. The remote mirror of the
/// confined-agent row, and it must stay the mirror: the two scoping mechanisms
/// are separate code paths that are supposed to mean the same thing.
#[test]
fn a_session_scoped_remote_device_mirrors_the_confined_agent() {
    let (store, peer) = remote(Some(IN_SCOPE));
    assert_eq!(
        observe(&store, &peer),
        Row {
            allowed_in_scope: true,
            write_in_scope: true,
            manage_heartbeats_in_scope: true,
            read_heartbeats_in_scope: true,
            read_own_team_run: true,
            attempt_heartbeat_writes: true,
            read_state: true,
            write_state: true,
            ..NOTHING
        },
        "a session-bound pairing confines exactly as a session-scoped local caller does",
    );
}

/// Scope and tier are orthogonal, and compose: confined to one session *and*
/// unable to change anything in it.
#[test]
fn scope_and_read_only_compose() {
    let (store, peer) = remote_read_only(Some(IN_SCOPE));
    assert_eq!(
        observe(&store, &peer),
        Row {
            allowed_in_scope: true,
            read_heartbeats_in_scope: true,
            read_own_team_run: true,
            read_state: true,
            ..NOTHING
        },
        "a narrowed, watch-only device may read one session and nothing else",
    );
}

/// Revocation is total and immediate — it is the kill switch for a lost phone,
/// so no predicate may survive it, including the two a confined caller keeps.
#[test]
fn revocation_closes_every_gate() {
    let (store, peer) = remote(None);
    store.revoke(peer.remote_pubkey().expect("remote"));
    assert_eq!(observe(&store, &peer), NOTHING, "a revoked device reaches nothing at all");
}

/// A key the host never paired. Holding a pubkey proves nothing by itself —
/// `Peer::remote` is free to mint, and every predicate still consults the
/// device store.
#[test]
fn an_unknown_key_reaches_nothing() {
    let store = AuthStore::new();
    let peer = Peer::remote(vk(0x77));
    assert_eq!(observe(&store, &peer), NOTHING, "an unpaired key is not a caller");
}

// ---------------------------------------------------------------------------
// The drift gate.
// ---------------------------------------------------------------------------

/// The matrix is only a proof while it is complete, and nothing about adding a
/// method to `acl.rs` would otherwise make a stale table fail. So the table is
/// counted against its own source: every `pub fn` on `AuthStore` that answers
/// an authorization question must appear in [`observe`].
///
/// A new predicate therefore lands red, with a message naming what to do, rather
/// than landing silently unproven — which is the failure mode this whole module
/// exists to close.
#[test]
fn the_matrix_covers_every_predicate() {
    let source = include_str!("acl.rs");
    let observed = include_str!("acl_matrix.rs");

    let predicates: Vec<&str> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub fn "))
        .filter_map(|sig| sig.split(['(', '<']).next())
        .filter(|name| name.starts_with("may_") || name.starts_with("is_allowed"))
        .collect();

    // A sanity floor: if the parse above ever silently matches nothing, an empty
    // list would make this test vacuous rather than failing.
    assert!(
        predicates.len() >= 17,
        "parsed only {} predicates from acl.rs — the signature scan is broken, not the matrix",
        predicates.len(),
    );

    let missing: Vec<&str> = predicates
        .iter()
        .copied()
        .filter(|name| !observed.contains(&format!("store.{name}(")))
        .collect();

    assert!(
        missing.is_empty(),
        "acl.rs has authorization predicates the matrix never asks about: {missing:?}\n\
         Add a field to `Row`, ask it in `observe`, and give every class row an \
         expected value — an unproven gate is how a scope silently widens.",
    );
}
