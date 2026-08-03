//! End-to-end over a real socket: bind owner-only, dial with the token,
//! carry frames both ways — and the two-factor refusals (no token file,
//! wrong token) that back the CLI's exit-code contract.

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
