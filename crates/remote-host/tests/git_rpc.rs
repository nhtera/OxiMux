//! The git RPCs against a **real repository**, driven through the real dispatcher
//! over the in-memory loopback.
//!
//! The load-bearing assertion here is containment: `path_guard` is unit-tested on
//! its own, but this proves it is actually *wired in* at the RPC boundary. Without
//! that wiring a paired phone could turn `GitDiff` into "read any file on this
//! machine" — the exact hole the phase's red team flagged as critical.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_proto::Transport;

const SECRET: [u8; 16] = [0x22; 16];
const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}

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

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("run git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A real repo with one untracked file, plus a secret placed *outside* it that a
/// traversal would try to reach.
fn repo_with_untracked_file() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let outside = tempfile::tempdir().expect("outside dir");
    std::fs::write(outside.path().join("secret.txt"), b"top secret\n").expect("write secret");

    let dir = tempfile::tempdir().expect("repo dir");
    git(dir.path(), &["init"]);
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("new.txt"), b"hello\n").expect("write untracked");

    let root = dir.path().to_path_buf();
    (dir, outside, root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_diff_serves_repo_files_and_refuses_to_escape() {
    let (_dir, outside, root) = repo_with_untracked_file();
    let escape_target = outside.path().join("secret.txt").to_string_lossy().into_owned();

    let registry = Arc::new(SessionRegistry::new());
    let handle = registry.register("sess-1".into(), Arc::new(StubConnection::default()));
    handle.set_meta(SessionMeta { title: None, model: None, cwd: Some(root.clone()) });

    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(clock);
    let pubkey = [0x33; 32];

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let script = async move {
        let Response::Registered { .. } = call(&client, Request::Register(register_req(pubkey))).await
        else {
            panic!("expected Registered");
        };

        // Status sees the untracked file, which is how a client learns the path.
        let Response::GitStatus(status) =
            call(&client, Request::GitStatus { session_id: "sess-1".into() }).await
        else {
            panic!("expected GitStatus");
        };
        assert!(
            status.files.iter().any(|f| f.path == "new.txt"),
            "the untracked file shows up in status: {:?}",
            status.files,
        );

        // A path from that listing diffs normally.
        let Response::GitDiff(files) = call(
            &client,
            Request::GitDiff {
                session_id: "sess-1".into(),
                path: "new.txt".into(),
                staged: false,
                untracked: true,
            },
        )
        .await
        else {
            panic!("expected GitDiff");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new.txt");

        // …but traversal out of the repo is refused, by relative path…
        for attack in ["../secret.txt", "../../../../etc/passwd"] {
            let got = call(
                &client,
                Request::GitDiff {
                    session_id: "sess-1".into(),
                    path: attack.into(),
                    staged: false,
                    untracked: true,
                },
            )
            .await;
            assert!(
                matches!(got, Response::Error(RpcError::BadRequest(_))),
                "{attack} must be refused, got {got:?}",
            );
        }

        // …and by absolute path to a real file that genuinely exists outside.
        let got = call(
            &client,
            Request::GitDiff {
                session_id: "sess-1".into(),
                path: escape_target,
                staged: false,
                untracked: true,
            },
        )
        .await;
        assert!(
            matches!(got, Response::Error(RpcError::BadRequest(_))),
            "an absolute path outside the repo must be refused, got {got:?}",
        );
        // `client` drops here → serve ends.
    };

    tokio::join!(serve, script);
}
