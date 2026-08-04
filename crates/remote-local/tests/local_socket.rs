//! End-to-end over a real socket: bind owner-only, dial with the token,
//! carry frames both ways — and the two-factor refusals (no token file,
//! wrong token) that back the CLI's exit-code contract.

#[cfg(unix)]
use interprocess::local_socket::ToFsName as _;
#[cfg(unix)]
use interprocess::local_socket::traits::tokio::Stream as _;
use oximux_remote_local::{
    DialError, LocalClaim, LocalControlListener, dial, generate_token, token_path,
    write_token_file,
};
use oximux_remote_proto::Transport;

/// Happy path: handshake grants, the claim arrives, frames cross both ways,
/// and every on-disk artifact is owner-only by readback.
#[tokio::test]
async fn dial_handshake_and_frames_over_a_real_socket() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let token = generate_token();
    let listener = LocalControlListener::bind(&runtime_dir, &token).unwrap();

    // The two trust factors, asserted from the outside.
    assert!(oximux_owner_only::is_restricted_to_owner(&token_path(&runtime_dir)).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let dir_mode = std::fs::metadata(&runtime_dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700, "runtime dir is owner-only");
        let sock_mode = std::fs::metadata(oximux_remote_local::socket_path(&runtime_dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(sock_mode & 0o777, 0o600, "socket node is owner-only");
    }

    let server = async {
        let (transport, claim) = listener.accept().await.unwrap();
        assert_eq!(claim, LocalClaim::Operator, "the token file is the operator credential");
        let frame = transport.recv().await.unwrap().expect("a request frame");
        assert_eq!(frame, b"ping".to_vec());
        transport.send(b"pong".to_vec()).await.unwrap();
    };
    let client = async {
        let transport = dial(&runtime_dir).await.unwrap();
        transport.send(b"ping".to_vec()).await.unwrap();
        let frame = transport.recv().await.unwrap().expect("a response frame");
        assert_eq!(frame, b"pong".to_vec());
    };
    tokio::join!(server, client);
}

/// A granted session credential earns that session's scope and no more.
///
/// Presented explicitly rather than through the environment: `std::env` is
/// process-global and these tests run concurrently. The env-to-credential
/// mapping an agent child actually goes through is covered by `credential`'s
/// own synchronous unit tests, and end-to-end by the CLI suite, which sets the
/// variables on a *child process* where they cannot race anything.
#[tokio::test]
async fn a_session_credential_earns_only_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let listener = LocalControlListener::bind(&runtime_dir, &generate_token()).unwrap();
    let secret = listener.grant_session("sess-7");

    let server = async {
        let (_transport, claim) = listener.accept().await.unwrap();
        assert_eq!(claim, LocalClaim::Session("sess-7".into()));
    };
    let client = async {
        oximux_remote_local::dial_as(
            &runtime_dir,
            oximux_remote_local::LocalIdentity::Session("sess-7".into()),
            &secret,
        )
        .await
        .expect("its own session is granted");
    };
    tokio::join!(server, client);
}

/// The spawn-order property: a credential minted before its session existed is
/// re-pointed at the real id, and the holder — whose environment still carries
/// the mint-time handle — earns the SESSION's scope, not the handle's.
///
/// This is what makes agent confinement possible at all: an agent's id arrives
/// with its own `SessionInit`, long after the environment it was spawned with
/// was fixed.
#[tokio::test]
async fn a_rebound_credential_earns_the_session_it_was_bound_to() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let listener = LocalControlListener::bind(&runtime_dir, &generate_token()).unwrap();
    let secret = listener.grant_session("launch-abc");
    listener.bind_session("launch-abc", "sess-real");

    let server = async {
        let (_transport, claim) = listener.accept().await.unwrap();
        assert_eq!(claim, LocalClaim::Session("sess-real".into()));
    };
    let client = async {
        oximux_remote_local::dial_as(
            &runtime_dir,
            oximux_remote_local::LocalIdentity::Session("launch-abc".into()),
            &secret,
        )
        .await
        .expect("the handle still names the credential");
    };
    tokio::join!(server, client);
}

/// Revocation is keyed on the mint-time handle, and binding an unknown handle
/// creates nothing — a session that ended before its agent announced itself
/// must not be able to resurrect a credential.
#[tokio::test]
async fn a_revoked_handle_stops_working_and_binding_it_back_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let listener = LocalControlListener::bind(&runtime_dir, &generate_token()).unwrap();
    let secret = listener.grant_session("launch-abc");
    listener.revoke_session("launch-abc");
    listener.bind_session("launch-abc", "sess-real");

    let server = async {
        assert!(listener.accept().await.is_err(), "a revoked handle grants nothing");
    };
    let client = async {
        let denied = oximux_remote_local::dial_as(
            &runtime_dir,
            oximux_remote_local::LocalIdentity::Session("launch-abc".into()),
            &secret,
        )
        .await;
        assert!(denied.is_err(), "the secret died with the credential");
    };
    tokio::join!(server, client);
}

/// The containment property end-to-end: an agent holding a session secret
/// cannot reach operator scope by naming the operator identity. It fails at
/// the host proof — the label it named is bound to a secret it does not hold.
#[tokio::test]
async fn a_session_holder_cannot_escalate_to_operator() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let listener = LocalControlListener::bind(&runtime_dir, &generate_token()).unwrap();
    let session_secret = listener.grant_session("sess-7");

    let server = async {
        assert!(listener.accept().await.is_err(), "the host must grant nothing");
    };
    let client = async {
        // The agent's own secret, presented while naming OPERATOR — the exact
        // move a prompt-injected agent would try.
        let err = oximux_remote_local::dial_as(
            &runtime_dir,
            oximux_remote_local::LocalIdentity::Operator,
            &session_secret,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DialError::Denied(_)), "got {err:?}");
    };
    tokio::join!(server, client);
}

/// No token file = local access never enabled: unreachable, not denied — the
/// CLI turns this into exit 3 with "enable local access" guidance.
#[tokio::test]
async fn missing_token_file_reads_as_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let err = dial(dir.path()).await.unwrap_err();
    assert!(matches!(err, DialError::Unreachable { .. }), "got {err:?}");
}

/// A stale/wrong token on the caller's side is a refusal (exit 5), and the
/// caller refuses the host before proving anything — the host's proof fails
/// against the wrong token.
#[tokio::test]
async fn stale_token_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let listener = LocalControlListener::bind(&runtime_dir, &generate_token()).unwrap();
    let server = async {
        // The handshake fails host-side too; the accept surfaces the refusal.
        assert!(listener.accept().await.is_err());
    };
    let client = async {
        // Rotate the on-disk token AFTER bind, so the dial reads a token the
        // listener no longer holds.
        write_token_file(&token_path(&runtime_dir), &generate_token()).unwrap();
        let err = dial(&runtime_dir).await.unwrap_err();
        assert!(matches!(err, DialError::Denied(_)), "got {err:?}");
    };
    tokio::join!(server, client);
}

/// A crashed host leaves a stale socket node; the next bind must take the
/// name over rather than failing AddrInUse.
#[cfg(unix)]
#[tokio::test]
async fn rebind_over_a_stale_socket_node() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let token = generate_token();
    let first = LocalControlListener::bind(&runtime_dir, &token).unwrap();
    // Simulate a crash: forget the listener without its Drop unlinking the
    // node (std::mem::forget leaks the fd, which is fine for one test).
    std::mem::forget(first);
    assert!(oximux_remote_local::socket_path(&runtime_dir).exists());
    let _second = LocalControlListener::bind(&runtime_dir, &token)
        .expect("rebind over the stale node");
}

/// A listener dropped AFTER a successor has rebound the same path must leave the
/// successor's socket node alone.
///
/// This is not hypothetical ordering. The desktop's handle aborts its accept task
/// to shut a listener down, and an abort is asynchronous — the task's `Arc` on the
/// listener is released whenever the runtime next drops that future, which can be
/// after a toggle-off/toggle-on has already rebound. An unconditional unlink there
/// deletes the LIVE listener's node: it keeps serving on its fd with no directory
/// entry, so every dial fails ENOENT while the UI still reads "enabled".
#[cfg(unix)]
#[tokio::test]
async fn a_late_drop_does_not_unlink_a_successors_socket() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let token = generate_token();
    let socket = oximux_remote_local::socket_path(&runtime_dir);

    let first = LocalControlListener::bind(&runtime_dir, &token).unwrap();
    // The successor unlinks the stale node and creates its own in its place.
    let second = LocalControlListener::bind(&runtime_dir, &token).unwrap();
    assert!(socket.exists(), "the successor bound a node");

    // The predecessor's drop lands late, as a cancelled accept task's would.
    drop(first);
    assert!(
        socket.exists(),
        "a late drop must not unlink the node a later bind created"
    );

    // And the successor still serves through it: the node is not merely present,
    // it is the one a caller reaches.
    let server = async {
        let (_transport, claim) = second.accept().await.unwrap();
        assert_eq!(claim, LocalClaim::Operator);
    };
    let client = async {
        dial(&runtime_dir).await.expect("the surviving listener is dialable");
    };
    tokio::join!(server, client);

    // The successor's own drop still cleans up after itself.
    drop(second);
    assert!(!socket.exists(), "a listener unlinks the node it bound");
}

/// A peer that connects and then says nothing must not hold the accept path.
///
/// `accept_pending` returns as soon as the connection exists, before any peer
/// input, so the handshake — the first I/O an unauthenticated caller controls —
/// runs in a task of the host's choosing. When it ran inside `accept`, one silent
/// process wedged local CLI access for every later caller until the app restarted.
///
/// Unix-only for its plumbing, not its subject: the silent peer is a raw stream,
/// and naming the endpoint takes the filesystem spelling. The behaviour under
/// test is platform-independent.
#[cfg(unix)]
#[tokio::test]
async fn a_silent_peer_does_not_block_the_accept_path() {
    let dir = tempfile::tempdir().unwrap();
    let runtime_dir = dir.path().join("runtime");
    let token = generate_token();
    let listener = LocalControlListener::bind(&runtime_dir, &token).unwrap();

    let server = async {
        // The silent caller is accepted...
        let stalled = listener.accept_pending().await.expect("accept the silent peer");
        // ...and the NEXT caller is served without it having said a word. Holding
        // `stalled` across this is the whole point: it stands in for a peer that
        // never sends its hello.
        let (_transport, claim) = listener.accept().await.expect("accept the real caller");
        assert_eq!(claim, LocalClaim::Operator, "a later caller is served regardless");
        drop(stalled);
    };
    let clients = async {
        // Connects and sends nothing, staying alive for the duration.
        let _silent = interprocess::local_socket::tokio::Stream::connect(
            oximux_remote_local::socket_path(&runtime_dir)
                .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                .unwrap(),
        )
        .await
        .expect("the silent peer connects");
        dial(&runtime_dir).await.expect("a real caller still gets through");
    };
    tokio::join!(server, clients);
}
